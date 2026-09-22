//! 管理端口：REST API + SSE 实时推送 + JSON/HAR 导出 + 静态界面服务。

use std::sync::Arc;

use hyper::body::{Bytes, HttpBody};
use hyper::header;
use hyper::http::HeaderValue;
use hyper::{Body, Method, Request, Response, StatusCode};

use crate::capture::{self, Filters};
use crate::dial;
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
        (&Method::GET, "/api/videos") => list_videos(&app),
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
        (&Method::GET, "/api/upstream") => upstream_status(&app),
        (&Method::POST, "/api/upstream") => upstream_set(req, &app).await,
        (&Method::POST, "/api/upstream/scan") => upstream_scan(&app).await,
        (&Method::GET, "/api/export") => export(req, &app),
        (&Method::GET, "/api/ca.crt") => ca_cert(&app),
        (&Method::GET, p) if p.starts_with("/api/entries/") && p.ends_with("/stitch") => {
            entry_stitch(&app, p)
        }
        (&Method::GET, p) if p.starts_with("/api/entries/") && p.ends_with("/fullvideo") => {
            return Ok(entry_fullvideo(req, app.clone(), p).await)
        }
        (&Method::GET, p) if p.starts_with("/api/entries/") && p.ends_with("/body") => {
            entry_body(req, &app, p)
        }
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

/// 从 URL 里取一个安全的 ASCII 文件名（用于下载时的 Content-Disposition）。
fn safe_download_name(e: &crate::capture::Entry) -> String {
    let path = e.url.split(['?', '#']).next().unwrap_or("");
    let last = path.rsplit('/').next().unwrap_or("");
    let ok = !last.is_empty()
        && last.len() <= 64
        && last
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-~".contains(c));
    let name = if ok {
        last.to_string()
    } else {
        format!("entry-{}", e.id)
    };
    // .m4s 与 .mp4 是同一种 fMP4 容器，改后缀浏览器/播放器才能直接识别
    match name.rsplit_once('.') {
        Some((stem, "m4s")) => format!("{}.mp4", stem),
        _ => name,
    }
}

/// 完整正文端点：`GET /api/entries/:id/body?side=req|resp[&dl=1]`。
/// 详情接口的原始字节视图只回传前 256 KB（RAW_VIEW_CAP），这里回传后端保留的
/// 完整正文（最多 BODY_CAP = 4 MB），供前端多媒体预览与全量下载使用。
/// 截断（存储期超 4 MB）时带 `X-Miniproxy-Truncated: 1` 响应头。
fn entry_body(req: Request<Body>, app: &App, path: &str) -> Response<Body> {
    let rest = path.trim_start_matches("/api/entries/");
    let id: Option<u64> = rest
        .strip_suffix("/body")
        .and_then(|s| s.parse().ok());
    let query = req.uri().query().unwrap_or("");
    let get_param = |key: &str| -> Option<String> {
        query.split('&').find_map(|kv| {
            let mut it = kv.splitn(2, '=');
            if it.next()? == key {
                Some(it.next().unwrap_or("").to_string())
            } else {
                None
            }
        })
    };
    let side = get_param("side").unwrap_or_else(|| "resp".to_string());
    let download = query.split('&').any(|kv| kv == "dl=1");

    let entry = match id.and_then(|id| app.store.find(id)) {
        Some(e) => e,
        None => return not_found(),
    };
    let inner = entry.inner.lock().unwrap();
    let (bytes, truncated, content_type) = if side == "req" {
        (
            inner.req_body.clone(),
            inner.req_truncated,
            None::<String>,
        )
    } else {
        // 有解压结果时优先给解压内容（音视频一般无压缩，行为不变）
        (
            inner.resp_decoded.clone().or_else(|| inner.resp_body.clone()),
            inner.resp_truncated,
            inner.content_type.clone(),
        )
    };
    drop(inner);

    let bytes = match bytes {
        Some(b) if !b.is_empty() => b,
        _ => return not_found(),
    };

    let mut builder = Response::builder()
        .header(
            header::CONTENT_TYPE,
            content_type
                .filter(|ct| !ct.trim().is_empty())
                .unwrap_or_else(|| "application/octet-stream".to_string()),
        )
        .header("X-Miniproxy-Truncated", if truncated { "1" } else { "0" })
        .header(header::CONTENT_LENGTH, bytes.len());
    if download {
        builder = builder.header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{}\"", safe_download_name(&entry)),
        );
    }
    builder.body(Body::from(bytes)).unwrap()
}

/* ---------------- 分段视频拼接（DASH .m4s / Range 分块） ---------------- */

fn json_error(status: StatusCode, msg: &str) -> Response<Body> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
        .body(Body::from(
            serde_json::to_string(&serde_json::json!({ "error": msg })).unwrap_or_default(),
        ))
        .unwrap()
}

/// ISOBMFF 第一个 box 的类型（偏移 4-8 字节）。
fn first_box_type(b: &[u8]) -> Option<&[u8]> {
    if b.len() < 8 {
        return None;
    }
    Some(&b[4..8])
}

/// fMP4 媒体分段：styp / moof / sidx 开头（ftyp 开头的完整文件不算）。
fn is_mp4_segment(b: &[u8]) -> bool {
    match first_box_type(b) {
        Some(t) => t == b"styp" || t == b"moof" || t == b"sidx",
        None => false,
    }
}

/// init 片段：ftyp 开头且含 moov（承载编码参数），且不是媒体分段。
fn is_mp4_init(b: &[u8]) -> bool {
    match first_box_type(b) {
        Some(t) => {
            t == b"ftyp"
                && b.windows(4).any(|w| w == b"moov")
                && !b.windows(4).take(4096).any(|w| w == b"moof")
                // 不含 mdat：init 只有编码头；完整 MP4 文件（ftyp+moov+mdat）不算 init
                && !has_mdat_box(b)
        }
        None => false,
    }
}

/// 沿顶层 box 链找 mdat（完整 MP4 的标志）
fn has_mdat_box(b: &[u8]) -> bool {
    let mut off = 0usize;
    while off + 8 <= b.len() {
        let size = u32::from_be_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]]) as u64;
        let typ = &b[off + 4..off + 8];
        if typ == b"mdat" {
            return true;
        }
        if size == 0 {
            return false; // mdat 到文件尾，后面没有别的 box
        }
        if size == 1 {
            // 64 位 largesize
            if off + 16 > b.len() {
                return false;
            }
            let large = u64::from_be_bytes([
                b[off + 8], b[off + 9], b[off + 10], b[off + 11],
                b[off + 12], b[off + 13], b[off + 14], b[off + 15],
            ]);
            if large < 16 {
                return false;
            }
            off += large as usize;
        } else {
            off += size as usize;
        }
    }
    false
}

/// 从请求头里取 Range 起始字节（bytes=START-END）。
fn range_start(headers: &[(String, String)]) -> Option<u64> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("range"))
        .and_then(|(_, v)| {
            let v = v.trim();
            v.strip_prefix("bytes=")
                .and_then(|rest| rest.split('-').next())
                .and_then(|s| s.parse().ok())
        })
}

fn url_path(url: &str) -> &str {
    url.split(['?', '#']).next().unwrap_or(url)
}

fn url_dir(url: &str) -> &str {
    let p = url_path(url);
    match p.rfind('/') {
        Some(i) => &p[..=i],
        None => "/",
    }
}

fn url_name(url: &str) -> &str {
    let p = url_path(url);
    p.rsplit('/').next().unwrap_or(p)
}

fn strip_ext(name: &str) -> &str {
    match name.rfind('.') {
        Some(i) if i > 0 => &name[..i],
        _ => name,
    }
}

/// 分段基名：去掉结尾的 -m<数字>（DASH 分段命名，如 video-m1 -> video）。
fn seg_base(name_no_ext: &str) -> &str {
    if let Some(i) = name_no_ext.rfind('-') {
        let tail = &name_no_ext[i + 1..];
        let mut chars = tail.chars();
        if chars.next() == Some('m') && tail.len() > 1 && tail[1..].bytes().all(|c| c.is_ascii_digit())
        {
            return &name_no_ext[..i];
        }
    }
    name_no_ext
}

/// -m<数字> 里的序号（无则 None）。
fn seg_seq(name_no_ext: &str) -> Option<u64> {
    let base = seg_base(name_no_ext);
    if base.len() + 2 <= name_no_ext.len() && name_no_ext[base.len()..].starts_with("-m") {
        return name_no_ext[base.len() + 2..].parse().ok();
    }
    None
}

/// 条目正文的统一读取（resp 优先解压内容）。
fn body_of(entry: &crate::capture::Entry, side: &str) -> Option<Vec<u8>> {
    let inner = entry.inner.lock().unwrap();
    let b = if side == "req" {
        inner.req_body.clone()
    } else {
        inner.resp_decoded.clone().or_else(|| inner.resp_body.clone())
    };
    b.filter(|b| !b.is_empty())
}

/// 拼接端点：`GET /api/entries/:id/stitch[?side=req|resp]`。
/// 两种模式：
/// - Range 分块：同一 URL 被多次 Range 请求（B 站等），按 Range 起点排序拼接还原完整文件；
/// - DASH 分段：目标是 fMP4 分段（styp/moof/sidx 开头，推特 .m4s 等），
///   自动找到 init 片段（ftyp+moov）与同目录同基名的其他分段，按序拼接。
/// 成功时 `X-Miniproxy-Stitch` 头描述拼接内容；失败返回 422 + JSON error。
fn entry_stitch(app: &App, path: &str) -> Response<Body> {
    let rest = path.trim_start_matches("/api/entries/");
    let id = rest.strip_suffix("/stitch").and_then(|s| s.parse::<u64>().ok());
    let target = match id.and_then(|id| app.store.find(id)) {
        Some(e) => e,
        None => return not_found(),
    };
    let tb = match body_of(&target, "resp") {
        Some(b) => b,
        None => return json_error(StatusCode::UNPROCESSABLE_ENTITY, "该条目没有响应正文"),
    };
    let mime = {
        let inner = target.inner.lock().unwrap();
        inner
            .content_type
            .clone()
            .filter(|ct| ct.starts_with("video/") || ct.starts_with("audio/"))
            .unwrap_or_else(|| "video/mp4".to_string())
    };

    // 收集同一 URL 的所有分块（含 Range 起点）
    let mut chunks: Vec<(u64, Vec<u8>)> = Vec::new();
    let mut ranged = false;
    {
        let list = app.store.entries.lock().unwrap();
        for e in list.iter() {
            if e.kind != "http" || e.url != target.url {
                continue;
            }
            if range_start(&e.req_headers).map(|s| s > 0).unwrap_or(false) {
                ranged = true;
            }
            if let Some(b) = body_of(e, "resp") {
                chunks.push((range_start(&e.req_headers).unwrap_or(0), b));
            }
        }
    }

    if is_mp4_segment(&tb) {
        return stitch_dash(&target, &tb, &mime, app);
    }
    if ranged && chunks.len() > 1 {
        return stitch_ranges(chunks, &mime);
    }
    json_error(
        StatusCode::UNPROCESSABLE_ENTITY,
        "该条目不是可识别的视频分段（既不是 DASH .m4s 分段，也没有 Range 分块）",
    )
}

/// Range 分块拼接：按起点排序、跳过重叠、发现缺口即报错。
fn stitch_ranges(mut chunks: Vec<(u64, Vec<u8>)>, mime: &str) -> Response<Body> {
    chunks.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.len().cmp(&a.1.len())));
    if chunks[0].0 != 0 {
        return json_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "该 URL 的第一块（起始字节 0）未被抓到，无法还原完整文件；请从头播放/下载一次再试",
        );
    }
    let mut out: Vec<u8> = Vec::new();
    let mut pos: u64 = 0;
    let mut n = 0usize;
    for (start, body) in chunks {
        let end = start + body.len() as u64;
        if start > pos {
            return json_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                &format!("分块之间存在缺口（字节 {}..{} 未被抓到），无法完整还原", pos, start),
            );
        }
        if end <= pos {
            continue; // 完全重叠的重复块
        }
        let skip = (pos - start) as usize;
        out.extend_from_slice(&body[skip..]);
        pos = end;
        n += 1;
    }
    Response::builder()
        .header(header::CONTENT_TYPE, mime)
        .header("X-Miniproxy-Stitch", format!("ranges={}", n))
        .header(header::CONTENT_LENGTH, out.len())
        .body(Body::from(out))
        .unwrap()
}

/// DASH 分段拼接：找 init 片段（ftyp+moov）+ 同目录同基名的媒体分段，按序拼接。
fn stitch_dash(
    target: &Arc<crate::capture::Entry>,
    tb: &[u8],
    mime: &str,
    app: &App,
) -> Response<Body> {
    let tdir = url_dir(&target.url).to_string();
    let tname = strip_ext(url_name(&target.url)).to_string();
    let tbase = seg_base(&tname).to_string();

    let mut init: Option<(i32, u64, Vec<u8>)> = None; // (匹配得分, id, bytes)
    // 排序键：有 -m<数字> 序号用序号，否则用抓包 id（目标分段同样规则）
    let t_seq = seg_seq(&tname).unwrap_or(target.id);
    let mut segs: Vec<(u64, u64, Vec<u8>)> = vec![(t_seq, target.id, tb.to_vec())]; // (序号或 id, id, bytes)

    {
        let list = app.store.entries.lock().unwrap();
        for e in list.iter() {
            if e.kind != "http" || e.host != target.host || e.id == target.id {
                continue;
            }
            let Some(b) = body_of(e, "resp") else { continue };
            let nm = strip_ext(url_name(&e.url)).to_string();
            if is_mp4_init(&b) {
                let same_dir = url_dir(&e.url) == tdir;
                let base_match =
                    seg_base(&nm) == tbase || nm == "init" || nm.starts_with(&tbase);
                if !same_dir && !base_match {
                    continue;
                }
                let score = (same_dir as i32) * 2 + (base_match as i32);
                if init.as_ref().map(|(s, _, _)| score > *s).unwrap_or(true) {
                    init = Some((score, e.id, b));
                }
            } else if is_mp4_segment(&b) && url_dir(&e.url) == tdir && seg_base(&nm) == tbase {
                let seq = seg_seq(&nm).unwrap_or(e.id);
                segs.push((seq, e.id, b));
            }
        }
    }

    let Some((_, init_id, init_bytes)) = init else {
        return json_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "未抓到该视频的初始化片段（init segment）。请在播放视频前开启抓包并从头播放一次；若视频较大，建议直接用原始链接下载",
        );
    };

    segs.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    // 去重：同一序号只保留第一份
    segs.dedup_by(|a, b| a.0 == b.0);

    let mut out = init_bytes;
    let n_seg = segs.len();
    for (_, _, b) in segs {
        out.extend_from_slice(&b);
    }
    Response::builder()
        .header(header::CONTENT_TYPE, mime)
        .header(
            "X-Miniproxy-Stitch",
            format!("init=#{}; segments={}", init_id, n_seg),
        )
        .header(header::CONTENT_LENGTH, out.len())
        .body(Body::from(out))
        .unwrap()
}

/* ---------------- 完整视频下载（m3u8 拼段 / 整文件重拉） ---------------- */

const HLS_PLAYLIST_CAP: usize = 4 * 1024 * 1024;
const HLS_SEGMENT_CAP: usize = 256 * 1024 * 1024;
const HLS_MAX_SEGMENTS: usize = 4096;
const HLS_MAX_TOTAL: usize = 1024 * 1024 * 1024;

/// 相对地址解析：绝对 URL / //host/path / /绝对路径 / 相对路径
fn resolve_url(base: &str, href: &str) -> String {
    let href = href.trim();
    if href.starts_with("http://") || href.starts_with("https://") {
        return href.to_string();
    }
    if let Some(rest) = href.strip_prefix("//") {
        if let Some(i) = base.find("://") {
            return format!("{}://{}", &base[..i], rest);
        }
    }
    let base_no_q = base.split('?').next().unwrap_or(base);
    if let Some(path) = href.strip_prefix('/') {
        if let Some(i) = base_no_q.find("://") {
            let after = &base_no_q[i + 3..];
            let host_end = after.find('/').map(|j| i + 3 + j).unwrap_or(base_no_q.len());
            return format!("{}/{}", &base_no_q[..host_end], path);
        }
    }
    let dir = match base_no_q.rfind('/') {
        Some(i) => &base_no_q[..=i],
        None => "/",
    };
    format!("{}{}", dir, href.trim_start_matches('/'))
}

/// 通过代理自身的 HTTP 客户端（走上游级联）GET 一个地址。
async fn fetch_via_client(
    app: &App,
    url: &str,
    ua: Option<&str>,
    referer: Option<&str>,
) -> Result<Response<Body>, String> {
    let mut builder = hyper::Request::builder()
        .method(Method::GET)
        .uri(url);
    if let Some(ua) = ua {
        builder = builder.header(header::USER_AGENT, ua);
    }
    if let Some(r) = referer {
        builder = builder.header(header::REFERER, r);
    }
    let req = builder
        .body(Body::empty())
        .map_err(|e| format!("构造请求失败: {}", e))?;
    app.client
        .request(req)
        .await
        .map_err(|e| format!("请求失败: {}", e))
}

/// GET 并把响应体读成 Vec（带上限）。
async fn fetch_body_vec(
    app: &App,
    url: &str,
    ua: Option<&str>,
    referer: Option<&str>,
    cap: usize,
) -> Result<Vec<u8>, String> {
    let resp = fetch_via_client(app, url, ua, referer).await?;
    if !resp.status().is_success() {
        return Err(format!("源站返回 HTTP {}", resp.status()));
    }
    let mut body = resp.into_body();
    let mut out: Vec<u8> = Vec::new();
    while let Some(chunk) = body.data().await {
        let chunk = chunk.map_err(|e| format!("读取响应失败: {}", e))?;
        if out.len() + chunk.len() > cap {
            return Err(format!("响应超过 {} 上限", format_bytes_cap(cap)));
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

fn format_bytes_cap(n: usize) -> String {
    if n >= 1024 * 1024 {
        format!("{} MB", n / 1024 / 1024)
    } else {
        format!("{} KB", n / 1024)
    }
}

/// 完整视频下载端点：`GET /api/entries/:id/fullvideo`。
/// 不依赖抓包碎片，直接从源站重新拉取：
/// - .m3u8：解析播放列表（支持主playlist选最高码率、#EXT-X-MAP init、相对地址），
///   顺序下载全部分段并拼接，TS / fMP4 自动识别；
/// - 普通媒体（video/audio）：整文件重新 GET，流式转发，不受 4 MB 存储上限影响。
/// 浏览器以附件形式下载。
async fn entry_fullvideo(req: Request<Body>, app: Arc<App>, path: &str) -> Response<Body> {
    let _ = req;
    let rest = path.trim_start_matches("/api/entries/");
    let id = rest
        .strip_suffix("/fullvideo")
        .and_then(|s| s.parse::<u64>().ok());
    let entry = match id.and_then(|id| app.store.find(id)) {
        Some(e) => e,
        None => return not_found(),
    };
    let (url, content_type) = {
        let inner = entry.inner.lock().unwrap();
        (entry.url.clone(), inner.content_type.clone())
    };
    // 复用抓包请求里的 UA / Referer，绕过常见防盗链
    let (ua, referer) = {
        let mut ua = None;
        let mut ref_ = None;
        for (k, v) in &entry.req_headers {
            let kl = k.to_lowercase();
            if kl == "user-agent" {
                ua = Some(v.clone());
            } else if kl == "referer" {
                ref_ = Some(v.clone());
            }
        }
        (ua, ref_)
    };

    let is_hls = url_path(&url).ends_with(".m3u8")
        || content_type
            .as_deref()
            .map(|ct| ct.to_lowercase().contains("mpegurl"))
            .unwrap_or(false);

    if !is_hls {
        // 普通媒体：必须是音视频类型才允许（Content-Type 不可靠，B 站等会回 octet-stream，
        // 此时用 URL 扩展名兜底）
        let media_ok = content_type
            .as_deref()
            .map(|ct| ct.starts_with("video/") || ct.starts_with("audio/"))
            .unwrap_or(false)
            || VIDEO_EXTS.contains(&path_ext(&url).as_str());
        if !media_ok {
            return json_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "该条目不是音视频内容，无法下载完整文件",
            );
        }
        let resp = match fetch_via_client(&app, &url, ua.as_deref(), referer.as_deref()).await {
            Ok(r) if r.status().is_success() => r,
            Ok(r) => {
                return json_error(
                    StatusCode::BAD_GATEWAY,
                    &format!("源站返回 HTTP {}，无法获取完整文件", r.status()),
                )
            }
            Err(e) => return json_error(StatusCode::BAD_GATEWAY, &e),
        };
        let (parts, body) = resp.into_parts();
        let ct = parts
            .headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
            .or(content_type)
            .unwrap_or_else(|| "application/octet-stream".to_string());
        return Response::builder()
            .header(header::CONTENT_TYPE, ct)
            .header(
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{}\"", safe_download_name(&entry)),
            )
            .body(body)
            .unwrap();
    }

    // ---- HLS：解析 playlist ----
    let playlist = match fetch_body_vec(
        &app,
        &url,
        ua.as_deref(),
        referer.as_deref(),
        HLS_PLAYLIST_CAP,
    )
    .await
    {
        Ok(t) => String::from_utf8_lossy(&t).to_string(),
        Err(e) => return json_error(StatusCode::BAD_GATEWAY, &format!("获取 m3u8 失败: {}", e)),
    };

    let mut playlist_url = url.clone();
    let mut pl_text = playlist;

    // 主 playlist：选 BANDWIDTH 最高的变体再取一级
    if pl_text.contains("#EXT-X-STREAM-INF") {
        let lines: Vec<&str> = pl_text.lines().collect();
        let mut best: Option<(u64, String)> = None;
        let mut i = 0;
        while i < lines.len() {
            if let Some(line) = lines[i].strip_prefix("#EXT-X-STREAM-INF") {
                let bw = line
                    .split(',')
                    .find_map(|kv| {
                        let kv = kv.trim().trim_start_matches(':');
                        let mut it = kv.split('=');
                        let k = it.next()?.trim().to_ascii_uppercase();
                        if k == "BANDWIDTH" {
                            it.next()?.trim().parse::<u64>().ok()
                        } else {
                            None
                        }
                    })
                    .unwrap_or(0);
                if let Some(uri) = lines[i + 1..].iter().find(|l| !l.trim().starts_with('#')) {
                    let full = resolve_url(&playlist_url, uri.trim());
                    if best.as_ref().map(|(b, _)| bw > *b).unwrap_or(true) {
                        best = Some((bw, full));
                    }
                }
                i += 2;
            } else {
                i += 1;
            }
        }
        match best {
            Some((_, variant_url)) => {
                playlist_url = variant_url;
                match fetch_body_vec(
                    &app,
                    &playlist_url,
                    ua.as_deref(),
                    referer.as_deref(),
                    HLS_PLAYLIST_CAP,
                )
                .await
                {
                    Ok(t) => pl_text = String::from_utf8_lossy(&t).to_string(),
                    Err(e) => {
                        return json_error(
                            StatusCode::BAD_GATEWAY,
                            &format!("获取子 m3u8 失败: {}", e),
                        )
                    }
                }
            }
            None => {
                return json_error(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "m3u8 里没有可用的播放地址",
                )
            }
        }
    }

    // 收集 init + 分段
    let mut init_uri: Option<String> = None;
    let mut segs: Vec<String> = Vec::new();
    for line in pl_text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("#EXT-X-MAP") {
            if let Some(p) = rest.split("URI=\"").nth(1) {
                if let Some(q) = p.find('"') {
                    init_uri = Some(resolve_url(&playlist_url, &p[..q]));
                }
            }
        } else if !line.is_empty() && !line.starts_with('#') {
            segs.push(resolve_url(&playlist_url, line));
        }
    }
    if segs.is_empty() {
        return json_error(StatusCode::UNPROCESSABLE_ENTITY, "m3u8 里没有找到视频分段");
    }
    let total_hint = segs.len();
    if segs.len() > HLS_MAX_SEGMENTS {
        segs.truncate(HLS_MAX_SEGMENTS);
    }

    // 容器类型：有 init 或分段是 .m4s/.mp4 → fMP4；否则 TS
    let first_path = url_path(&segs[0]).to_lowercase();
    let is_mp4 = init_uri.is_some()
        || first_path.ends_with(".m4s")
        || first_path.ends_with(".mp4")
        || first_path.contains(".m4s?");
    let out_ct = if is_mp4 { "video/mp4" } else { "video/mp2t" };
    let out_ext = if is_mp4 { "mp4" } else { "ts" };

    // 下载文件名：playlist 名去扩展名 + .ts/.mp4
    let mut name = strip_ext(url_name(&playlist_url)).to_string();
    if name.is_empty() || name == "index" || name == "playlist" {
        name = format!("video-{}", entry.id);
    }
    let filename = format!("{}.{}", name.replace(['"', '\\'], ""), out_ext);

    let app2 = app.clone();
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(16);
    tokio::spawn(async move {
        let mut total = 0usize;
        let mut chain: Vec<String> = Vec::new();
        if let Some(init) = init_uri {
            chain.push(init);
        }
        chain.extend(segs);
        for u in chain {
            if total > HLS_MAX_TOTAL {
                break;
            }
            match fetch_body_vec(&app2, &u, ua.as_deref(), referer.as_deref(), HLS_SEGMENT_CAP)
                .await
            {
                Ok(bytes) => {
                    total += bytes.len();
                    if tx.send(Ok(Bytes::from(bytes))).await.is_err() {
                        break;
                    }
                }
                Err(e) => {
                    let _ = tx
                        .send(Err(std::io::Error::new(
                            std::io::ErrorKind::Other,
                            format!("分段下载失败: {}", e),
                        )))
                        .await;
                    break;
                }
            }
        }
    });

    Response::builder()
        .header(header::CONTENT_TYPE, out_ct)
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{}\"", filename),
        )
        .header("X-Miniproxy-Segments", format!("{}", total_hint))
        .body(Body::wrap_stream(tokio_stream::wrappers::ReceiverStream::new(
            rx,
        )))
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

/* ---------------- 视频下载器聚合列表 ---------------- */

/// MP4 解析：在字节里找 moov/tkhd，取面积最大的视频轨宽高（16.16 定点存储）。
/// 只读不猜：找不到 moov（如存储被截断）就返回 None。
fn mp4_resolution(b: &[u8]) -> Option<(u32, u32)> {
    /// 遍历一层 box：返回 (类型, payload)。兼容 64 位 largesize 与 size==0（到文件尾）。
    fn boxes(b: &[u8]) -> Vec<(&[u8], &[u8])> {
        let mut out = Vec::new();
        let mut i = 0usize;
        while i + 8 <= b.len() {
            let size = u32::from_be_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]) as usize;
            let typ = &b[i + 4..i + 8];
            let (payload_start, payload_len) = if size == 1 {
                if i + 16 > b.len() {
                    break;
                }
                let l = u64::from_be_bytes(b[i + 8..i + 16].try_into().unwrap()) as usize;
                if l < 16 {
                    break;
                }
                (i + 16, l - 16)
            } else if size == 0 {
                (i + 8, b.len() - i - 8)
            } else if size >= 8 {
                (i + 8, size - 8)
            } else {
                break;
            };
            let end = (payload_start + payload_len).min(b.len());
            if payload_start > end {
                break;
            }
            out.push((typ, &b[payload_start..end]));
            i = end;
        }
        out
    }
    for (t, payload) in boxes(b) {
        if t != b"moov" {
            continue;
        }
        let mut best: Option<(u64, u32, u32)> = None;
        for (ct, p) in boxes(payload) {
            if ct == b"tkhd" && p.len() >= 8 {
                // width/height 固定在 tkhd 末尾 8 字节
                let w = u32::from_be_bytes(p[p.len() - 8..p.len() - 4].try_into().unwrap()) >> 16;
                let h = u32::from_be_bytes(p[p.len() - 4..].try_into().unwrap()) >> 16;
                if w > 0 && h > 0 && best.as_ref().map(|(a, _, _)| w as u64 * h as u64 > *a).unwrap_or(true) {
                    best = Some((w as u64 * h as u64, w, h));
                }
            }
        }
        if let Some((_, w, h)) = best {
            return Some((w, h));
        }
    }
    None
}

/// URL 路径（去 query）的小写扩展名
fn path_ext(url: &str) -> String {
    let p = url_path(url).to_lowercase();
    match p.rsplit_once('.') {
        Some((_, ext)) if !ext.contains('/') && ext.len() <= 8 => ext.to_string(),
        _ => String::new(),
    }
}

const VIDEO_EXTS: &[&str] = &["mp4", "m4s", "webm", "flv", "mkv", "mov", "m4v", "avi", "ts", "mp3", "m4a", "aac", "wav", "ogg", "opus"];

/// m3u8 文本里 SUM(EXTINF) 秒数
fn hls_duration(text: &str) -> f64 {
    text.lines()
        .filter_map(|l| l.strip_prefix("#EXTINF:"))
        .filter_map(|v| v.split(',').next())
        .filter_map(|v| v.trim().parse::<f64>().ok())
        .sum()
}

/// master playlist 里的最大 RESOLUTION（"1280x720"）
fn hls_resolution(text: &str) -> Option<String> {
    let mut best: Option<(u64, String)> = None;
    for line in text.lines() {
        let Some(attrs) = line.strip_prefix("#EXT-X-STREAM-INF") else { continue };
        for kv in attrs.split(',') {
            let kv = kv.trim().trim_start_matches(':');
            let mut it = kv.split('=');
            let k = it.next().unwrap_or("").trim().to_ascii_uppercase();
            if k != "RESOLUTION" {
                continue;
            }
            let v = it.next().unwrap_or("").trim();
            let (w, h) = match v.split_once('x') {
                Some((w, h)) => (w.trim().parse::<u64>().unwrap_or(0), h.trim().parse::<u64>().unwrap_or(0)),
                None => continue,
            };
            if w > 0 && h > 0 && best.as_ref().map(|(a, _)| w * h > *a).unwrap_or(true) {
                best = Some((w * h, format!("{}x{}", w, h)));
            }
        }
    }
    best.map(|(_, r)| r)
}

/// 单条抓包条目的轻量快照（只对「可能是媒体」的条目拷贝正文，控制内存）
struct VidSnap {
    id: u64,
    url: String,
    host: String,
    ct: Option<String>,
    body: Option<Vec<u8>>,
    body_full: bool,
    size: u64,
    range: Option<u64>,
    total_len: Option<u64>, // Content-Range 里的总大小
}

/// 视频下载器：把抓包条目聚合为「可完整获取」的视频列表。
/// 三类来源（下载入口各不相同）：
/// - file：独立音视频文件 → /fullvideo 整文件重拉，不受存储截断影响
/// - hls：VOD m3u8（须见过 #EXT-X-ENDLIST，直播流排除）→ /fullvideo 实时拼段
/// - dash：init 齐全的 fMP4 分段组（推特 .m4s / Range 分块）→ /stitch 拼接已捕获分段
fn list_videos(app: &App) -> Response<Body> {
    use std::collections::BTreeMap;

    // ---- 快照：只拷贝可能是媒体的条目 ----
    let mut snaps: Vec<VidSnap> = Vec::new();
    {
        let list = app.store.entries.lock().unwrap();
        for e in list.iter() {
            if e.kind != "http" {
                continue;
            }
            let inner = e.inner.lock().unwrap();
            let ok = matches!(inner.resp_status, Some(s) if (200..300).contains(&s));
            if !ok {
                continue;
            }
            let ct = inner.content_type.clone();
            let ext = path_ext(&e.url);
            let ct_media = ct
                .as_deref()
                .map(|c| c.starts_with("video/") || c.starts_with("audio/") || c.to_lowercase().contains("mpegurl"))
                .unwrap_or(false);
            if !ct_media && !VIDEO_EXTS.contains(&ext.as_str()) && ext != "m4s" {
                continue;
            }
            let total_len = inner.resp_headers.as_ref().and_then(|hs| {
                hs.iter().find(|(k, _)| k.eq_ignore_ascii_case("content-range")).and_then(|(_, v)| {
                    v.rsplit('/').next()?.trim().parse::<u64>().ok()
                })
            });
            // Range 头只有在响应确实是 206 分段时才算数（有些服务器忽略 Range 返回 200 全量）
            let range = if inner.resp_status == Some(206) {
                range_start(&e.req_headers)
            } else {
                None
            };
            // 注意：body_of 会再次锁 e.inner，std Mutex 不可重入，
            // 必须先在本块里取完所有字段并释放 inner 锁，再拷正文
            let (body, body_full, size) = {
                let need_body = ct_media || ext == "m4s" || ext == "ts" || ext == "mp4";
                let full = !inner.resp_truncated;
                let sz = inner.resp_body.as_ref().map(|b| b.len()).unwrap_or(0) as u64;
                if need_body {
                    drop(inner);
                    (body_of(e, "resp"), full, sz)
                } else {
                    (None, full, sz)
                }
            };
            snaps.push(VidSnap {
                id: e.id,
                url: e.url.clone(),
                host: e.host.clone(),
                ct,
                body,
                body_full,
                size,
                range,
                total_len,
            });
        }
    }

    let mut items: Vec<serde_json::Value> = Vec::new();

    // ---- 1. HLS：m3u8 条目聚合 ----
    // master 优先作为代表（fullvideo 会自动选最高码率）；它引用的变体 playlist 不再单独出条目。
    struct HlsItem {
        id: u64,
        name: String,
        host: String,
        url: String,
        resolution: Option<String>,
        duration: f64,
        variants: Vec<String>, // master 引用的变体 URL（media playlist 为空）
    }
    let mut masters: Vec<HlsItem> = Vec::new();
    let mut medias: Vec<HlsItem> = Vec::new();
    // 只有「含 ENDLIST 的 media playlist 所在目录」才算 HLS 分段目录；
    // master 常与无关文件同目录，不能拿来排除
    let mut hls_seg_dirs: Vec<String> = Vec::new();

    for s in &snaps {
        let ext = path_ext(&s.url);
        let is_pl = ext == "m3u8"
            || s.ct.as_deref().map(|c| c.to_lowercase().contains("mpegurl")).unwrap_or(false);
        if !is_pl {
            continue;
        }
        let Some(body) = &s.body else { continue };
        let text = String::from_utf8_lossy(body);
        let name = {
            let n = strip_ext(url_name(&s.url)).to_string();
            if n.is_empty() || n == "index" || n == "playlist" || n == "master" {
                format!("video-{}", s.id)
            } else {
                n
            }
        };
        if text.contains("#EXT-X-STREAM-INF") {
            let mut variants: Vec<String> = Vec::new();
            let lines: Vec<&str> = text.lines().collect();
            let mut i = 0;
            while i < lines.len() {
                if lines[i].starts_with("#EXT-X-STREAM-INF") {
                    if let Some(uri) = lines[i + 1..].iter().find(|l| !l.trim().starts_with('#')) {
                        variants.push(resolve_url(&s.url, uri.trim()));
                    }
                    i += 2;
                } else {
                    i += 1;
                }
            }
            masters.push(HlsItem {
                id: s.id,
                name,
                host: s.host.clone(),
                url: s.url.clone(),
                resolution: hls_resolution(&text),
                duration: 0.0,
                variants,
            });
        } else if text.contains("#EXT-X-ENDLIST") {
            // 点播 media playlist：只有见过 ENDLIST 才认定「完整视频存在」，直播流排除
            hls_seg_dirs.push(url_dir(&s.url).to_string());
            medias.push(HlsItem {
                id: s.id,
                name,
                host: s.host.clone(),
                url: s.url.clone(),
                resolution: None,
                duration: hls_duration(&text),
                variants: Vec::new(),
            });
        }
    }

    // 变体被 master 引用 → 时长并入 master、不单独出条目；其余 media playlist 独立成条目
    let mut consumed: Vec<String> = Vec::new();
    for m in &mut masters {
        for md in &medias {
            if m.variants.iter().any(|v| *v == md.url) {
                m.duration = m.duration.max(md.duration);
                consumed.push(md.url.clone());
            }
        }
        let covered = m.variants.iter().any(|v| {
            medias.iter().any(|md| md.url == *v)
        });
        if !covered {
            continue; // 引用的变体没抓到或没见过 ENDLIST，无法确认是完整点播
        }
        items.push(serde_json::json!({
            "entryId": m.id, "kind": "hls", "name": m.name, "host": m.host,
            "url": m.url, "size": serde_json::Value::Null,
            "sizeExact": false, "resolution": m.resolution,
            "durationSec": if m.duration > 0.0 { serde_json::json!(m.duration) } else { serde_json::Value::Null },
            "segments": serde_json::Value::Null,
        }));
    }
    for md in &medias {
        if consumed.contains(&md.url) {
            continue;
        }
        items.push(serde_json::json!({
            "entryId": md.id, "kind": "hls", "name": md.name, "host": md.host,
            "url": md.url, "size": serde_json::Value::Null,
            "sizeExact": false, "resolution": md.resolution,
            "durationSec": if md.duration > 0.0 { serde_json::json!(md.duration) } else { serde_json::Value::Null },
            "segments": serde_json::Value::Null,
        }));
    }

    // ---- 2. DASH 分段组：is_mp4_segment 的 m4s 按 (host, dir, base) 归组，须有 init ----
    struct DashGroup {
        repr_id: u64,
        name: String,
        host: String,
        url: String,
        size: u64,
        segments: usize,
        init_body: Option<Vec<u8>>,
        /// 组内去重后的 URL（去 query）；全部相同 = Range 分块组（可整文件重拉）
        urls: Vec<String>,
        /// 组内见过的 Content-Range 总长（精确文件大小）
        total_len: Option<u64>,
        /// 组内成员自身是完整 MP4（ftyp+moov+mdat）时，从它解析的分辨率（优先于 init）
        self_res: Option<(u32, u32)>,
    }
    let mut dash: BTreeMap<(String, String, String), DashGroup> = BTreeMap::new();
    let mut inits: Vec<(String, String, String, Vec<u8>)> = Vec::new(); // (host, dir, base, body)
    for s in &snaps {
        let ext = path_ext(&s.url);
        let is_seg = ext == "m4s" || s.body.as_deref().map(is_mp4_segment).unwrap_or(false);
        let Some(body) = &s.body else { continue };
        let dir = url_dir(&s.url).to_string();
        let base = {
            let n = strip_ext(url_name(&s.url)).to_string();
            if is_seg {
                seg_base(&n).to_string()
            } else {
                n
            }
        };
        if is_mp4_init(body) {
            inits.push((s.host.clone(), dir, base, body.clone()));
        } else if is_seg {
            let key = (s.host.clone(), dir, base.clone());
            let url_no_q = s.url.split('?').next().unwrap_or(&s.url).to_string();
            let g = dash.entry(key).or_insert_with(|| DashGroup {
                repr_id: s.id,
                name: base.clone(),
                host: s.host.clone(),
                url: s.url.clone(),
                size: 0,
                segments: 0,
                init_body: None,
                urls: Vec::new(),
                total_len: None,
                self_res: None,
            });
            g.size += s.size;
            g.segments += 1;
            if !g.urls.iter().any(|u| *u == url_no_q) {
                g.urls.push(url_no_q);
            }
            if let Some(t) = s.total_len {
                let cur = g.total_len.get_or_insert(t);
                if t > *cur {
                    *cur = t;
                }
            }
            // 成员自身是完整 MP4（ftyp+moov+mdat）→ 用它自己的 moov 解析分辨率
            if g.self_res.is_none() {
                if let Some(body) = &s.body {
                    if first_box_type(body) == Some(b"ftyp") && has_mdat_box(body) {
                        g.self_res = mp4_resolution(body);
                    }
                }
            }
        }
    }
    for (key, g) in dash.iter_mut() {
        // init 匹配：优先基名相同（最可靠），其次同目录
        g.init_body = inits
            .iter()
            .find(|(h, d, b, _)| *h == key.0 && *b == key.2)
            .or_else(|| inits.iter().find(|(h, d, _, _)| *h == key.0 && *d == key.1))
            .map(|(_, _, _, body)| body.clone());
    }

    // ---- 3. 独立媒体文件：排除 m4s 分段 / m3u8 / 已被 HLS 收编的同目录 ts、mp4 ----
    for s in &snaps {
        let ext = path_ext(&s.url);
        if ext == "m3u8" || s.ct.as_deref().map(|c| c.to_lowercase().contains("mpegurl")).unwrap_or(false) {
            continue;
        }
        let is_seg = ext == "m4s" || s.body.as_deref().map(is_mp4_segment).unwrap_or(false);
        if is_seg {
            continue;
        }
        // Range 分块的同 URL 重复条目：只收起点最小（最好是 0）的那条
        let url_key = s.url.split('?').next().unwrap_or(&s.url).to_string();
        let is_ranged = s.range.map(|r| r > 0).unwrap_or(false);
        if is_ranged {
            let better = snaps.iter().any(|o| {
                o.url.split('?').next().map(|p| p.to_string()) == Some(url_key.clone())
                    && o.range.unwrap_or(0) < s.range.unwrap_or(0)
            });
            if better {
                continue;
            }
        }
        // 该目录存在点播 media playlist 时，ts 通常是 HLS 分段，不单列
        if ext == "ts" && hls_seg_dirs.iter().any(|d| *d == url_dir(&s.url)) {
            continue;
        }
        let ct_media = s
            .ct
            .as_deref()
            .map(|c| c.starts_with("video/") || c.starts_with("audio/"))
            .unwrap_or(false);
        if !ct_media && !VIDEO_EXTS.contains(&ext.as_str()) {
            continue;
        }
        // 过滤噪音：太小的“媒体”多半是图标/试听片段
        if s.size < 16 * 1024 {
            continue;
        }
        let name = {
            let raw = url_name(&s.url);
            let n = strip_ext(raw.split('?').next().unwrap_or(raw)).to_string();
            if n.is_empty() {
                format!("media-{}", s.id)
            } else {
                n
            }
        };
        let out_ext = if VIDEO_EXTS.contains(&ext.as_str()) { ext.as_str() } else { "mp4" };
        // 大小：完整存储用实际字节数；截断时用 Content-Range 总长；再不行不给
        let (size, exact) = if s.body_full && s.range.unwrap_or(0) == 0 {
            (Some(s.size), true)
        } else if let Some(t) = s.total_len {
            (Some(t), true)
        } else {
            (None, false)
        };
        let resolution = s
            .body
            .as_deref()
            .filter(|_| s.body_full)
            .and_then(mp4_resolution)
            .map(|(w, h)| format!("{}x{}", w, h));
        items.push(serde_json::json!({
            "entryId": s.id, "kind": "file", "name": name, "host": s.host,
            "url": s.url, "size": size, "sizeExact": exact, "resolution": resolution,
            "durationSec": serde_json::Value::Null, "segments": serde_json::Value::Null,
            "ext": out_ext,
        }));
    }

    // DASH 组收进结果（须 init 齐全；Range 分块组用 Content-Range 总长做精确大小）
    for ((_, _, base), g) in dash {
        let Some(_) = g.init_body else { continue };
        let range_group = g.urls.len() <= 1; // 所有分段同一 URL = Range 分块组
        let init_res = g.init_body.as_deref().and_then(mp4_resolution);
        let resolution = g
            .self_res
            .or(init_res)
            .map(|(w, h)| format!("{}x{}", w, h));
        let (size, exact) = if range_group {
            match g.total_len {
                Some(t) => (Some(t), true),
                None => (Some(g.size), false),
            }
        } else {
            (Some(g.size), false) // 分段文件组只能按已捕获合计估算
        };
        items.push(serde_json::json!({
            "entryId": g.repr_id, "kind": "dash", "name": base, "host": g.host,
            "url": g.url, "size": size, "sizeExact": exact, "resolution": resolution,
            "durationSec": serde_json::Value::Null, "segments": g.segments,
            "rangeGroup": range_group,
        }));
    }

    // 大的排前面，同尺寸按新记录优先
    items.sort_by(|a, b| {
        let sa = a["size"].as_u64().unwrap_or(0);
        let sb = b["size"].as_u64().unwrap_or(0);
        sb.cmp(&sa).then(b["entryId"].as_u64().cmp(&a["entryId"].as_u64()))
    });
    json_response(serde_json::json!({ "items": items }))
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

/* ---------------- 上游级联（运行期可变） ---------------- */

/// 当前上游状态：`source` 说明它从哪来（环境变量 / 上次保存 / 自动检测 / 手动设置）。
fn upstream_status(app: &App) -> Response<Body> {
    let up = app.upstream();
    let env_set = std::env::var("MINIPROXY_UPSTREAM_PROXY")
        .ok()
        .filter(|v| !v.trim().is_empty());
    json_response(serde_json::json!({
        "enabled": up.is_some(),
        "addr": up.as_ref().map(|u| u.addr()),
        "source": app.upstream_source(),
        "envAddr": env_set,
    }))
}

/// 设置 / 关闭上游级联。请求体：`{"addr":"127.0.0.1:7890"}` 或 `{"enabled":false}`。
/// 保存前先做连通性测试，避免填错地址导致所有请求失败却不明原因。
async fn upstream_set(req: Request<Body>, app: &App) -> Response<Body> {
    let body = hyper::body::to_bytes(req.into_body())
        .await
        .unwrap_or_default();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
    let raw = v
        .get("addr")
        .and_then(|a| a.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let enabled = v
        .get("enabled")
        .and_then(|e| e.as_bool())
        .unwrap_or(!raw.is_empty());

    // 关闭
    if !enabled || raw.is_empty() {
        app.set_upstream(None, "off");
        app.save_upstream_config();
        return json_response(serde_json::json!({"ok": true, "enabled": false}));
    }

    if raw.starts_with("socks") {
        return json_response(serde_json::json!({
            "ok": false,
            "error": "暂不支持 socks 上游，请填写 HTTP 代理端口（如 Clash 的混合代理端口）",
        }));
    }
    let Some(up) = dial::Upstream::parse(&raw) else {
        return json_response(serde_json::json!({
            "ok": false,
            "error": "地址格式无效，应为 主机:端口（如 127.0.0.1:7890）",
        }));
    };
    if let Err(e) = up.probe().await {
        return json_response(serde_json::json!({"ok": false, "error": e}));
    }

    let addr = up.addr();
    app.set_upstream(Some(up), "manual");
    app.save_upstream_config();

    let mut payload = serde_json::json!({
        "ok": true,
        "enabled": true,
        "addr": addr,
        "source": "manual",
    });
    // 环境变量优先级更高：界面改的值在重启后会被它覆盖，明确提示
    if let Ok(env) = std::env::var("MINIPROXY_UPSTREAM_PROXY") {
        if !env.trim().is_empty() {
            payload["warning"] = serde_json::json!(format!(
                "本次已生效。但环境变量 MINIPROXY_UPSTREAM_PROXY={} 优先级更高，重启后会回到它指定的地址",
                env
            ));
        }
    }
    json_response(payload)
}

/// 扫描本机常见代理端口，返回可用的候选（CONNECT 探针通过才算）。
async fn upstream_scan(app: &App) -> Response<Body> {
    let found = dial::detect_local_upstream(&[app.proxy_port, app.api_port]).await;
    let candidates: Vec<serde_json::Value> = found
        .iter()
        .map(|u| serde_json::json!({"addr": u.addr(), "reachable": true}))
        .collect();
    json_response(serde_json::json!({ "ok": true, "candidates": candidates }))
}
