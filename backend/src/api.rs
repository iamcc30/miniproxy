//! 管理端口：REST API + SSE 实时推送 + JSON/HAR 导出 + 静态界面服务。

use std::sync::Arc;

use hyper::header;
use hyper::http::HeaderValue;
use hyper::{Body, Method, Request, Response, StatusCode};

use crate::capture::{self, Filters};
use crate::App;

use futures_util::StreamExt;

pub async fn handle_api(
    req: Request<Body>,
    app: Arc<App>,
) -> Result<Response<Body>, std::convert::Infallible> {
    let path = req.uri().path().to_string();
    let method = req.method().clone();

    let resp = match (&method, path.as_str()) {
        (&Method::GET, "/api/entries") => list_entries(req, &app),
        (&Method::GET, "/api/facets") => list_facets(&app),
        (&Method::GET, "/api/hosts") => list_hosts(&app),
        (&Method::GET, "/api/types") => list_types(&app),
        (&Method::DELETE, "/api/entries") => {
            app.store.clear();
            json_response(serde_json::json!({"ok": true}))
        }
        (&Method::GET, "/api/stream") => sse_stream(&app),
        (&Method::GET, "/api/system-proxy") => system_proxy_status(&app),
        (&Method::POST, "/api/system-proxy/enable") => system_proxy_set(&app, true),
        (&Method::POST, "/api/system-proxy/disable") => system_proxy_set(&app, false),
        (&Method::GET, "/api/export") => export(req, &app),
        (&Method::GET, "/api/ca.crt") => ca_cert(&app),
        (&Method::GET, p) if p.starts_with("/api/entries/") => {
            let id: Option<u64> = p.trim_start_matches("/api/entries/").parse().ok();
            match id.and_then(|id| app.store.find(id)) {
                Some(e) => json_response(capture::detail_json(&e)),
                None => not_found(),
            }
        }
        (&Method::GET, "/api/health") => {
            json_response(serde_json::json!({"ok": true, "name": "miniproxy"}))
        }
        (&Method::GET, "/api/info") => json_response(serde_json::json!({
            "name": "miniproxy",
            "apiPort": app.api_port,
            "proxyPort": app.proxy_port,
            "lanIp": app.lan_ip,
            "caUrl": match &app.lan_ip {
                Some(ip) => format!("http://{}:{}/api/ca.crt", ip, app.api_port),
                None => "/api/ca.crt".to_string(),
            },
        })),
        (&Method::GET, _) => serve_static(&path).await,
        _ => not_found(),
    };
    Ok(resp)
}

fn not_found() -> Response<Body> {
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .body(Body::from("404 Not Found"))
        .unwrap()
}

fn json_response(v: serde_json::Value) -> Response<Body> {
    Response::builder()
        .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
        .body(Body::from(serde_json::to_string(&v).unwrap_or_default()))
        .unwrap()
}

fn list_entries(req: Request<Body>, app: &App) -> Response<Body> {
    let filters = Filters::from_query(req.uri().query());
    let (items, total) = capture::filtered_entries(&app.store, &filters);
    let items: Vec<serde_json::Value> = items
        .iter()
        .map(|e| {
            let mut v = capture::summary_json(e);
            // 关键词只在请求/响应内容里命中时打标，界面据此显示「内容匹配」
            if filters.q_matched_in_content(e) {
                v["qInContent"] = serde_json::json!(true);
            }
            v
        })
        .collect();
    json_response(serde_json::json!({ "items": items, "total": total }))
}

fn list_hosts(app: &App) -> Response<Body> {
    let list = app.store.entries.lock().unwrap();
    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for e in list.iter() {
        if e.kind != "tcp" {
            *counts.entry(e.host.clone()).or_default() += 1;
        }
    }
    let mut hosts: Vec<serde_json::Value> = counts
        .into_iter()
        .filter(|(h, _)| !h.is_empty())
        .map(|(h, c)| serde_json::json!({"host": h, "count": c}))
        .collect();
    hosts.sort_by(|a, b| b["count"].as_u64().cmp(&a["count"].as_u64()));
    json_response(serde_json::json!({ "hosts": hosts }))
}

/// 分组维度候选值 + 计数：一次返回所有维度的 facet，供「分组方式 / 按组筛选」共用。
/// 维度：host（域名）/ site（站点）/ app（客户端应用）/ kind（协议）/ type（文件类型）/ status（状态码）。
fn list_facets(app: &App) -> Response<Body> {
    use std::collections::HashMap;

    let list = app.store.entries.lock().unwrap();
    let mut host: HashMap<String, usize> = HashMap::new();
    let mut site: HashMap<String, usize> = HashMap::new();
    let mut appn: HashMap<String, usize> = HashMap::new();
    let mut kind: HashMap<String, usize> = HashMap::new();
    let mut typ: HashMap<String, usize> = HashMap::new();
    let mut status: HashMap<String, usize> = HashMap::new();

    for e in list.iter() {
        if !e.host.is_empty() {
            *host.entry(e.host.clone()).or_default() += 1;
        }
        if !e.site.is_empty() {
            *site.entry(e.site.clone()).or_default() += 1;
        }
        if let Some(c) = e.client.as_ref().filter(|c| !c.is_empty()) {
            *appn.entry(c.clone()).or_default() += 1;
        }
        *kind.entry(e.kind.to_string()).or_default() += 1;
        *typ.entry(capture::resource_type(e).to_string()).or_default() += 1;
        if let Some(s) = e.inner.lock().unwrap().resp_status {
            let k = match s / 100 {
                2 => "2",
                3 => "3",
                4 => "4",
                5 => "5",
                _ => continue,
            };
            *status.entry(k.to_string()).or_default() += 1;
        }
    }

    let to_arr = |m: HashMap<String, usize>| -> serde_json::Value {
        let mut v: Vec<(String, usize)> = m.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        serde_json::Value::Array(
            v.into_iter()
                .map(|(k, c)| serde_json::json!({"value": k, "count": c}))
                .collect(),
        )
    };

    json_response(serde_json::json!({
        "facets": {
            "host": to_arr(host),
            "site": to_arr(site),
            "app": to_arr(appn),
            "kind": to_arr(kind),
            "type": to_arr(typ),
            "status": to_arr(status),
        }
    }))
}

fn list_types(app: &App) -> Response<Body> {
    let list = app.store.entries.lock().unwrap();
    let mut counts: std::collections::HashMap<&'static str, usize> = std::collections::HashMap::new();
    for e in list.iter() {
        *counts.entry(capture::resource_type(e)).or_default() += 1;
    }
    let mut types: Vec<serde_json::Value> = counts
        .into_iter()
        .map(|(t, c)| serde_json::json!({"type": t, "count": c}))
        .collect();
    types.sort_by(|a, b| b["count"].as_u64().cmp(&a["count"].as_u64()));
    json_response(serde_json::json!({ "types": types }))
}

fn sse_stream(app: &App) -> Response<Body> {
    let rx = app.store.tx.subscribe();
    let stream = tokio_stream::wrappers::BroadcastStream::new(rx).filter_map(|item| {
        futures_util::future::ready(match item {
            Ok(entry) => Some(Ok::<_, std::io::Error>(format!(
                "data: {}\n\n",
                capture::summary_json(&entry)
            ))),
            Err(_) => Some(Ok(":\n\n".to_string())), // 滞后/重连：发注释帧保活
        })
    });
    Response::builder()
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::wrap_stream(stream))
        .unwrap()
}

fn export(req: Request<Body>, app: &App) -> Response<Body> {
    let filters = Filters::from_query(req.uri().query());
    let format = req
        .uri()
        .query()
        .and_then(|q| {
            q.split('&').find_map(|kv| {
                let mut it = kv.splitn(2, '=');
                if it.next()? == "format" {
                    Some(it.next().unwrap_or("").to_string())
                } else {
                    None
                }
            })
        })
        .unwrap_or_else(|| "json".to_string());

    let (items, _) = capture::filtered_entries(&app.store, &filters);

    let body = if format == "har" {
        build_har(&items)
    } else {
        let v: Vec<serde_json::Value> = items.iter().map(|e| capture::detail_json(e)).collect();
        serde_json::to_string_pretty(&v).unwrap_or_default()
    };

    let ts = capture::now_ms();
    Response::builder()
        .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
        .header(
            header::CONTENT_DISPOSITION,
            HeaderValue::from_str(&format!(
                "attachment; filename=\"miniproxy-export-{}.{}\"",
                ts,
                if format == "har" { "har" } else { "json" }
            ))
            .unwrap_or(HeaderValue::from_static("attachment")),
        )
        .body(Body::from(body))
        .unwrap()
}

/// 导出 HAR 1.2。
fn build_har(entries: &[Arc<crate::capture::Entry>]) -> String {
    let har_entries: Vec<serde_json::Value> = entries
        .iter()
        .map(|e| {
            let inner = e.inner.lock().unwrap();
            let started = chrono_like_iso(e.ts);
            let headers_json = |hs: &Vec<(String, String)>| -> serde_json::Value {
                serde_json::Value::Array(
                    hs.iter()
                        .map(|(k, v)| serde_json::json!({"name": k, "value": v}))
                        .collect(),
                )
            };
            let req_text = inner
                .req_body
                .as_ref()
                .map(|b| String::from_utf8_lossy(b).to_string())
                .unwrap_or_default();
            let resp_text = inner
                .resp_decoded
                .as_ref()
                .or(inner.resp_body.as_ref())
                .map(|b| String::from_utf8_lossy(b).to_string())
                .unwrap_or_default();
            let query = e.url.split_once('?').map(|(_, qs)| {
                serde_json::Value::Array(
                    qs.split('&')
                        .filter(|kv| !kv.is_empty())
                        .map(|kv| {
                            let mut it = kv.splitn(2, '=');
                            serde_json::json!({
                                "name": it.next().unwrap_or(""),
                                "value": it.next().unwrap_or(""),
                            })
                        })
                        .collect(),
                )
            }).unwrap_or(serde_json::json!([]));
            serde_json::json!({
                "startedDateTime": started,
                "time": inner.started_at.saturating_sub(e.ts),
                "_kind": e.kind,
                "_site": e.site,
                "_client": e.client,
                "request": {
                    "method": e.method,
                    "url": e.url,
                    "httpVersion": "HTTP/1.1",
                    "headers": headers_json(&e.req_headers),
                    "queryString": query,
                    "cookies": [],
                    "headersSize": -1,
                    "bodySize": inner.req_body.as_ref().map(|b| b.len()).unwrap_or(0),
                    "postData": if req_text.is_empty() { serde_json::json!(null) } else {
                        serde_json::json!({"mimeType": inner.content_type.clone().unwrap_or_default(), "text": req_text})
                    },
                },
                "response": {
                    "status": inner.resp_status.unwrap_or(0),
                    "statusText": "",
                    "httpVersion": "HTTP/1.1",
                    "headers": headers_json(inner.resp_headers.as_ref().unwrap_or(&vec![])),
                    "content": {
                        "size": inner.resp_body.as_ref().map(|b| b.len()).unwrap_or(0),
                        "mimeType": inner.content_type.clone().unwrap_or_default(),
                        "text": resp_text,
                    },
                    "redirectURL": "",
                    "headersSize": -1,
                    "bodySize": inner.resp_body.as_ref().map(|b| b.len()).unwrap_or(0),
                },
                "cache": {},
                "timings": { "send": 0, "wait": 0, "receive": 0 },
                "_wsMessages": serde_json::to_value(&inner.ws_messages).unwrap_or(serde_json::json!([])),
                "_error": inner.error.clone(),
            })
        })
        .collect();

    serde_json::to_string_pretty(&serde_json::json!({
        "log": {
            "version": "1.2",
            "creator": { "name": "MiniProxy", "version": "0.1.0" },
            "entries": har_entries,
        }
    }))
    .unwrap_or_default()
}

fn chrono_like_iso(ts_ms: u128) -> String {
    // 简化实现：毫秒时间戳转 ISO8601（本地近似，用 UTC）
    let secs = (ts_ms / 1000) as i64;
    let ms = (ts_ms % 1000) as u32;
    let days = secs / 86400;
    let tod = secs % 86400;
    let (h, m, s) = (tod / 3600, (tod % 3600) / 60, tod % 60);
    // civil from days（Howard Hinnant 算法）
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mth = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mth <= 2 { y + 1 } else { y };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        y, mth, d, h, m, s, ms
    )
}

fn ca_cert(app: &App) -> Response<Body> {
    Response::builder()
        .header(header::CONTENT_TYPE, "application/x-x509-ca-cert")
        .header(
            header::CONTENT_DISPOSITION,
            "attachment; filename=\"miniproxy-ca.crt\"",
        )
        .body(Body::from(app.ca.cert_pem().to_string()))
        .unwrap()
}

fn static_dir() -> std::path::PathBuf {
    if let Ok(d) = std::env::var("MINIPROXY_STATIC") {
        return std::path::PathBuf::from(d);
    }
    for cand in ["./static", "./frontend/dist", "../frontend/dist"] {
        let p = std::path::PathBuf::from(cand);
        if p.join("index.html").exists() {
            return p;
        }
    }
    std::path::PathBuf::from("./static")
}

async fn serve_static(path: &str) -> Response<Body> {
    let base = static_dir();
    let rel = path.trim_start_matches('/');
    let rel = if rel.is_empty() { "index.html" } else { rel };
    // 防目录穿越
    let full = base.join(rel);
    if !full.starts_with(&base) {
        return not_found();
    }
    let fallback = || async {
        let idx = base.join("index.html");
        tokio::fs::read(idx).await.ok()
    };
    let data = match tokio::fs::read(&full).await {
        Ok(d) => Some(d),
        Err(_) => fallback().await,
    };
    match data {
        Some(data) => {
            let ct = match full.extension().and_then(|e| e.to_str()) {
                Some("html") => "text/html; charset=utf-8",
                Some("js") => "application/javascript; charset=utf-8",
                Some("css") => "text/css; charset=utf-8",
                Some("json") => "application/json; charset=utf-8",
                Some("svg") => "image/svg+xml",
                Some("png") => "image/png",
                Some("ico") => "image/x-icon",
                Some("map") => "application/json; charset=utf-8",
                _ => "application/octet-stream",
            };
            Response::builder()
                .header(header::CONTENT_TYPE, ct)
                .body(Body::from(data))
                .unwrap()
        }
        None => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
            .body(Body::from(
                "<h1>MiniProxy</h1><p>未找到前端界面。请先构建前端：<code>cd frontend && npm install && npm run build</code>，或将 dist 目录放到 ./static。</p>",
            ))
            .unwrap(),
    }
}

/// 系统代理状态：判断当前是否已指向本实例。
fn points_to_ours(v: &Option<(String, u16)>, app: &App) -> bool {
    matches!(v, Some((h, p))
        if *p == app.proxy_port && (h == "127.0.0.1" || h == "localhost"))
}

fn system_proxy_status(app: &App) -> Response<Body> {
    if !crate::sysproxy::supported() {
        return json_response(serde_json::json!({
            "supported": false,
            "hint": "当前平台暂不支持一键设置系统代理（支持 macOS）",
        }));
    }
    match crate::sysproxy::status() {
        Ok(services) => {
            let active = services.iter().any(|s| {
                points_to_ours(&s.http, app) && points_to_ours(&s.https, app)
            });
            json_response(serde_json::json!({
                "supported": true,
                "port": app.proxy_port,
                "active": active,
                "services": services,
            }))
        }
        Err(e) => json_response(serde_json::json!({
            "supported": true, "active": false, "error": e,
        })),
    }
}

fn system_proxy_set(app: &App, enable: bool) -> Response<Body> {
    if !crate::sysproxy::supported() {
        return json_response(serde_json::json!({"ok": false, "error": "当前平台不支持"}));
    }
    let result = if enable {
        crate::sysproxy::enable(app.proxy_port)
    } else {
        crate::sysproxy::disable()
    };
    match result {
        Ok(()) => system_proxy_status(app),
        Err(e) => json_response(serde_json::json!({"ok": false, "error": e})),
    }
}
