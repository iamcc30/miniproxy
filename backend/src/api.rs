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
        (&Method::GET, p) if p.starts_with("/api/entries/") && p.ends_with("/fullmux") => {
            return Ok(entry_fullmux(req, app.clone(), p).await)
        }
        (&Method::GET, p) if p.starts_with("/api/entries/") && p.ends_with("/umpsave") => {
            return Ok(entry_umpsave(app.clone(), p).await)
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
/// 从抓包请求头里取 UA / Referer（源站重拉时复用，绕常见防盗链）
fn entry_ua_referer(e: &crate::capture::Entry) -> (Option<String>, Option<String>) {
    let mut ua = None;
    let mut referer = None;
    for (k, v) in &e.req_headers {
        let kl = k.to_lowercase();
        if kl == "user-agent" {
            ua = Some(v.clone());
        } else if kl == "referer" {
            referer = Some(v.clone());
        }
    }
    (ua, referer)
}

/// 解析 m3u8：主 playlist 选 BANDWIDTH 最高变体，收集 init + 分段。
/// 返回 (有序下载链 [init?]+segs, 是否 fMP4, 实际使用的 playlist URL)。
async fn hls_chain(
    app: &App,
    url: &str,
    ua: Option<&str>,
    referer: Option<&str>,
) -> Result<(Vec<String>, bool, String), String> {
    let playlist = fetch_body_vec(app, url, ua, referer, HLS_PLAYLIST_CAP)
        .await
        .map_err(|e| format!("获取 m3u8 失败: {}", e))?;
    let mut playlist_url = url.to_string();
    let mut pl_text = String::from_utf8_lossy(&playlist).to_string();

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
                let t = fetch_body_vec(app, &playlist_url, ua, referer, HLS_PLAYLIST_CAP)
                    .await
                    .map_err(|e| format!("获取子 m3u8 失败: {}", e))?;
                pl_text = String::from_utf8_lossy(&t).to_string();
            }
            None => return Err("m3u8 里没有可用的播放地址".to_string()),
        }
    }

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
        return Err("m3u8 里没有找到视频分段".to_string());
    }
    if segs.len() > HLS_MAX_SEGMENTS {
        segs.truncate(HLS_MAX_SEGMENTS);
    }
    let first_path = url_path(&segs[0]).to_lowercase();
    let is_mp4 = init_uri.is_some()
        || first_path.ends_with(".m4s")
        || first_path.ends_with(".mp4")
        || first_path.contains(".m4s?");
    let mut chain = Vec::new();
    if let Some(init) = init_uri {
        chain.push(init);
    }
    chain.extend(segs);
    Ok((chain, is_mp4, playlist_url))
}

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

    // ---- HLS：解析 playlist（选最高码率变体 + EXT-X-MAP init）----
    let (chain, is_mp4, playlist_url) = match hls_chain(&app, &url, ua.as_deref(), referer.as_deref()).await {
        Ok(v) => v,
        Err(e) => {
            let code = if e.contains("没有") {
                StatusCode::UNPROCESSABLE_ENTITY
            } else {
                StatusCode::BAD_GATEWAY
            };
            return json_error(code, &e);
        }
    };
    let total_hint = chain.len();
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

/// 探测 ffmpeg 可执行文件路径（常见 Homebrew 位置 + PATH）
fn find_ffmpeg() -> Option<std::path::PathBuf> {
    for p in ["/opt/homebrew/bin/ffmpeg", "/usr/local/bin/ffmpeg"] {
        if std::path::Path::new(p).exists() {
            return Some(std::path::PathBuf::from(p));
        }
    }
    if let Ok(path) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path) {
            let pb = dir.join("ffmpeg");
            if pb.is_file() {
                return Some(pb);
            }
        }
    }
    None
}

/// 把某条目对应的源站内容完整下载到文件（直接媒体流式写；m3u8 逐段追加）。
async fn save_track_to_file(
    app: &App,
    entry: &std::sync::Arc<crate::capture::Entry>,
    out: &std::path::Path,
) -> Result<u64, String> {
    use tokio::io::AsyncWriteExt;
    let (url, content_type) = {
        let inner = entry.inner.lock().unwrap();
        (entry.url.clone(), inner.content_type.clone())
    };
    let (ua, referer) = entry_ua_referer(entry);
    let is_hls = url_path(&url).ends_with(".m3u8")
        || content_type
            .as_deref()
            .map(|c| c.to_lowercase().contains("mpegurl"))
            .unwrap_or(false);

    let mut f = tokio::fs::File::create(out)
        .await
        .map_err(|e| format!("创建临时文件失败: {}", e))?;
    let mut total = 0u64;
    if is_hls {
        let (chain, _, _) = hls_chain(app, &url, ua.as_deref(), referer.as_deref()).await?;
        for u in chain {
            if total > HLS_MAX_TOTAL as u64 {
                break;
            }
            let b = fetch_body_vec(app, &u, ua.as_deref(), referer.as_deref(), HLS_SEGMENT_CAP)
                .await?;
            total += b.len() as u64;
            f.write_all(&b).await.map_err(|e| e.to_string())?;
        }
    } else {
        let resp = fetch_via_client(app, &url, ua.as_deref(), referer.as_deref())
            .await
            .map_err(|e| e)?;
        let (parts, mut body) = resp.into_parts();
        if !parts.status.is_success() {
            return Err(format!("源站返回 HTTP {}", parts.status));
        }
        while let Some(chunk) = hyper::body::HttpBody::data(&mut body).await {
            let chunk = chunk.map_err(|e| e.to_string())?;
            total += chunk.len() as u64;
            f.write_all(&chunk).await.map_err(|e| e.to_string())?;
        }
    }
    f.flush().await.map_err(|e| e.to_string())?;
    Ok(total)
}

/// 音视频合并端点：`GET /api/entries/:id/fullmux?a=<audioEntryId>`。
/// 把视频轨与音频轨各自从源站完整拉取到临时文件，ffmpeg -c copy 合并后回传。
/// 需要系统安装 ffmpeg；未安装时返回 501，前端回退为分开下载。
async fn entry_fullmux(req: Request<Body>, app: Arc<App>, path: &str) -> Response<Body> {
    let _ = req;
    let rest = path.trim_start_matches("/api/entries/");
    let vid = rest
        .split('?')
        .next()
        .and_then(|s| s.strip_suffix("/fullmux"))
        .and_then(|s| s.parse::<u64>().ok());
    let aid = req
        .uri()
        .query()
        .and_then(|q| {
            q.split('&').find_map(|kv| {
                kv.strip_prefix("a=").and_then(|v| v.parse::<u64>().ok())
            })
        });
    let (Some(vid), Some(aid)) = (vid, aid) else {
        return not_found();
    };
    let Some(ffmpeg) = find_ffmpeg() else {
        return json_error(StatusCode::NOT_IMPLEMENTED, "未找到 ffmpeg，无法合并音视频");
    };
    let video_entry = match app.store.find(vid) {
        Some(e) => e,
        None => return not_found(),
    };
    let audio_entry = match app.store.find(aid) {
        Some(e) => e,
        None => return not_found(),
    };

    let dir = std::env::temp_dir().join(format!("miniproxy-mux-{}-{}", std::process::id(), vid));
    let _ = std::fs::remove_dir_all(&dir);
    if let Err(e) = tokio::fs::create_dir_all(&dir).await {
        return json_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("创建临时目录失败: {}", e));
    }
    // 扩展名要贴合真实容器：ffmpeg 对 .m4s 的格式探测不稳（fMP4 会被当成未知），
    // 用 .mp4 让它走 mp4 解复用器；HLS 拼出来的分片则是 .ts。
    let vfile = dir.join(if entry_is_hls(&video_entry) { "video.ts" } else { "video.mp4" });
    let afile = dir.join(if entry_is_hls(&audio_entry) { "audio.ts" } else { "audio.mp4" });
    let ofile = dir.join("out.mp4");

    let cleanup = |dir: &std::path::Path| {
        let _ = std::fs::remove_dir_all(dir);
    };

    if let Err(e) = save_track_to_file(&app, &video_entry, &vfile).await {
        cleanup(&dir);
        return json_error(StatusCode::BAD_GATEWAY, &format!("拉取视频轨失败: {}", e));
    }
    if let Err(e) = save_track_to_file(&app, &audio_entry, &afile).await {
        cleanup(&dir);
        return json_error(StatusCode::BAD_GATEWAY, &format!("拉取音频轨失败: {}", e));
    }

    let status = tokio::process::Command::new(&ffmpeg)
        .arg("-y")
        .arg("-i")
        .arg(&vfile)
        .arg("-i")
        .arg(&afile)
        .args(["-c", "copy", "-movflags", "+faststart"])
        .arg(&ofile)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await;
    match status {
        Ok(s) if s.success() => {}
        Ok(s) => {
            cleanup(&dir);
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("ffmpeg 合并失败（exit {}），轨道格式可能不受支持", s),
            );
        }
        Err(e) => {
            cleanup(&dir);
            return json_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("启动 ffmpeg 失败: {}", e));
        }
    }

    // 输出文件名：视频轨名去后缀 + .mp4
    let mut name = safe_download_name(&video_entry);
    if let Some((stem, _)) = name.rsplit_once('.') {
        name = format!("{}.mp4", stem);
    }

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(16);
    let ofile2 = ofile.clone();
    let dir2 = dir.clone();
    tokio::task::spawn_blocking(move || {
        use std::io::Read;
        match std::fs::File::open(&ofile2) {
            Ok(mut f) => {
                let mut buf = vec![0u8; 256 * 1024];
                loop {
                    match f.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            if tx
                                .blocking_send(Ok(Bytes::copy_from_slice(&buf[..n])))
                                .is_err()
                            {
                                break;
                            }
                        }
                        Err(e) => {
                            let _ = tx.blocking_send(Err(e));
                            break;
                        }
                    }
                }
            }
            Err(e) => {
                let _ = tx.blocking_send(Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    e.to_string(),
                )));
            }
        }
        let _ = std::fs::remove_dir_all(&dir2);
    });

    Response::builder()
        .header(header::CONTENT_TYPE, "video/mp4")
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{}\"", name),
        )
        .body(Body::wrap_stream(tokio_stream::wrappers::ReceiverStream::new(
            rx,
        )))
        .unwrap()
}

/// 把一条 SABR 轨道（init + 递增序号的媒体段）写成文件。
async fn write_sabr_track(path: &std::path::Path, t: &SabrTrack) -> Result<u64, String> {
    use tokio::io::AsyncWriteExt;
    let mut f = tokio::fs::File::create(path).await.map_err(|e| e.to_string())?;
    let mut total = 0u64;
    if let Some(init) = &t.init {
        f.write_all(init).await.map_err(|e| e.to_string())?;
        total += init.len() as u64;
    }
    for data in t.segs.values() {
        f.write_all(data).await.map_err(|e| e.to_string())?;
        total += data.len() as u64;
    }
    f.flush().await.map_err(|e| e.to_string())?;
    Ok(total)
}

/// SABR 下载端点：`GET /api/entries/:id/umpsave`。
///
/// 以该条目所属的 video_id 为线索，把 store 里所有同视频的 UMP 分段重组为完整轨道，
/// 需要时用 ffmpeg 把音视频合成一个文件回传。容器选择：
/// 视频轨与音频轨都是 mp4 → mp4（带 faststart）；任一为 WebM → mkv（AV1/VP9+opus 才装得下）。
///
/// 局限：SABR 没有 manifest 可以枚举全部分段，只能重组**浏览器已经请求过**的分段，
/// 因此用户播放到哪里就抓到哪里的内容。
async fn entry_umpsave(app: Arc<App>, path: &str) -> Response<Body> {
    use std::collections::BTreeMap;

    let id = path
        .trim_start_matches("/api/entries/")
        .split('/')
        .next()
        .and_then(|s| s.parse::<u64>().ok());
    let Some(id) = id else { return not_found() };
    let Some(entry) = app.store.find(id) else {
        return not_found();
    };

    // 1) 由该条目确定 video_id
    let Some(probe) = body_of(&entry, "resp") else {
        return json_error(StatusCode::BAD_REQUEST, "该条目没有响应正文");
    };
    let mut probe_tracks = BTreeMap::new();
    let Some(video_id) = ump_absorb(&probe, &mut probe_tracks) else {
        return json_error(StatusCode::BAD_REQUEST, "该条目不是 YouTube UMP（SABR）流");
    };
    drop(probe_tracks);

    // 2) 扫描 store，合并同一 video_id 的全部分段
    let mut tracks: BTreeMap<u32, SabrTrack> = BTreeMap::new();
    {
        let list = app.store.entries.lock().unwrap();
        for e in list.iter() {
            if e.kind != "http" {
                continue;
            }
            let (ct, status) = {
                let inner = e.inner.lock().unwrap();
                (inner.content_type.clone(), inner.resp_status)
            };
            let is_ump = ct
                .as_deref()
                .map(|c| c.to_lowercase().contains("vnd.yt-ump"))
                .unwrap_or(false);
            if !is_ump || !matches!(status, Some(s) if (200..300).contains(&s)) {
                continue;
            }
            // body_of 会锁 e.inner，必须放在上面那个块外面（std Mutex 不可重入）
            let Some(body) = body_of(e, "resp") else {
                continue;
            };
            let mut t = BTreeMap::new();
            if ump_absorb(&body, &mut t).as_deref() != Some(video_id.as_str()) {
                continue;
            }
            sabr_merge_tracks(&mut tracks, t);
        }
    }

    // 2.5) 自动补拉：把浏览器没请求到的分段（中段缺口 + 片尾）用「回放 + 位置改写」
    // 从服务端要回来。前提是已有 init 段（服务端不会重发 init，没有它拼不出文件）；
    // 拉不到新数据也不影响后续流程——能补多少算多少。
    {
        let has_init = tracks
            .iter()
            .any(|(i, t)| itag_is_video(*i) && t.init.is_some() && !t.segs.is_empty());
        if has_init {
            let mut cands: Vec<(String, Vec<u8>)> = Vec::new();
            {
                let list = app.store.entries.lock().unwrap();
                for e in list.iter().rev() {
                    if e.kind != "http" || e.method != "POST" {
                        continue;
                    }
                    // 回放目标必须是 YouTube 媒体节点；本地/其它来源的 UMP 条目
                    // （例如测试注入）不是有效的补拉目标
                    if !e.host.contains("googlevideo") {
                        continue;
                    }
                    let ct_ok = {
                        let inner = e.inner.lock().unwrap();
                        inner
                            .content_type
                            .as_deref()
                            .map(|c| c.to_lowercase().contains("vnd.yt-ump"))
                            .unwrap_or(false)
                    };
                    if !ct_ok {
                        continue;
                    }
                    // body_of 会锁 e.inner，必须放在上面那个块外面（std Mutex 不可重入）
                    if let Some(b) = body_of(e, "req") {
                        cands.push((e.url.clone(), b));
                        if cands.len() >= 8 {
                            break;
                        }
                    }
                }
            }
            if !cands.is_empty() {
                let proxy = app.upstream().map(|u| format!("http://{}", u.addr()));
                let vid = video_id.clone();
                let taken = std::mem::take(&mut tracks);
                let (tracks2, bytes, reason) =
                    tokio::task::spawn_blocking(move || {
                        sabr_refetch_blocking(cands, proxy, vid, taken)
                    })
                    .await
                    .unwrap_or_else(|e| {
                        (
                            BTreeMap::new(),
                            0usize,
                            Some(format!("补拉任务失败: {}", e)),
                        )
                    });
                tracks = tracks2;
                eprintln!(
                    "[umpsave] SABR 补拉: +{} 字节, {}",
                    bytes,
                    reason.as_deref().unwrap_or("完成")
                );
            }
        }
    }

    // 3) 选轨：视频按像素面积（并列看字节数），音频按字节数
    let mut vitag: Option<u32> = None;
    let mut vscore: (u64, u64) = (0, 0);
    for (itag, t) in &tracks {
        // 和列表侧同样的约束：没有 init 段就无法成片
        if !itag_is_video(*itag) || t.segs.is_empty() || t.init.is_none() {
            continue;
        }
        let res = t
            .init
            .as_deref()
            .and_then(mp4_resolution)
            .map(|(w, h)| format!("{}x{}", w, h));
        let score = (res_area(&res), t.segs.values().map(|b| b.len() as u64).sum::<u64>());
        if score > vscore {
            vscore = score;
            vitag = Some(*itag);
        }
    }
    let Some(vitag) = vitag else {
        return json_error(
            StatusCode::NOT_FOUND,
            "没有完整的视频轨：需要浏览器请求过初始化段（从头开始播放一次即可）",
        );
    };
    let v_webm = track_is_webm(tracks[&vitag].init.as_ref());
    // 音频优先挑与视频同容器的轨道：fMP4 视频 + mp4 音频能直接出 .mp4，
    // 混到 WebM/opus 就只能出 .mkv；同容器再按体积（≈码率）取大的。
    let aitag = tracks
        .iter()
        .filter(|(i, t)| !itag_is_video(**i) && !t.segs.is_empty() && t.init.is_some())
        .map(|(i, t)| {
            let w = track_is_webm(t.init.as_ref());
            (*i, t.segs.values().map(|b| b.len() as u64).sum::<u64>(), w)
        })
        .min_by_key(|(_, b, w)| (if v_webm { !*w } else { *w }, std::cmp::Reverse(*b)))
        .map(|(i, _, _)| i);

    let a_webm = match aitag {
        Some(a) => track_is_webm(tracks[&a].init.as_ref()),
        None => false,
    };
    // AV1/VP9 + opus 只有 mkv 装得下；纯 mp4 轨则保留 mp4
    let out_ext = if aitag.is_none() {
        if v_webm { "webm" } else { "mp4" }
    } else if v_webm || a_webm {
        "mkv"
    } else {
        "mp4"
    };

    // 4) 落临时文件
    let dir = std::env::temp_dir().join(format!("miniproxy-sabr-{}-{}", std::process::id(), id));
    let _ = std::fs::remove_dir_all(&dir);
    if let Err(e) = tokio::fs::create_dir_all(&dir).await {
        return json_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("创建临时目录失败: {}", e));
    }
    let cleanup = |d: &std::path::Path| {
        let _ = std::fs::remove_dir_all(d);
    };

    let vfile = dir.join(if v_webm { "video.webm" } else { "video.mp4" });
    if let Err(e) = write_sabr_track(&vfile, &tracks[&vitag]).await {
        cleanup(&dir);
        return json_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("写出视频轨失败: {}", e));
    }
    let mut afile: Option<std::path::PathBuf> = None;
    if let Some(a) = aitag {
        let p = dir.join(if a_webm { "audio.webm" } else { "audio.mp4" });
        if let Err(e) = write_sabr_track(&p, &tracks[&a]).await {
            cleanup(&dir);
            return json_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("写出音频轨失败: {}", e));
        }
        afile = Some(p);
    }

    // 5) 合并（无音频轨时视频轨即成品）
    let ofile = dir.join(format!("out.{}", out_ext));
    match &afile {
        Some(af) => {
            let Some(ffmpeg) = find_ffmpeg() else {
                cleanup(&dir);
                return json_error(StatusCode::NOT_IMPLEMENTED, "未找到 ffmpeg，无法合并音视频");
            };
            let mut cmd = tokio::process::Command::new(&ffmpeg);
            cmd.arg("-y").arg("-i").arg(&vfile).arg("-i").arg(af).args(["-c", "copy"]);
            if out_ext == "mp4" {
                cmd.args(["-movflags", "+faststart"]);
            }
            let status = cmd
                .arg(&ofile)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .await;
            match status {
                Ok(s) if s.success() => {}
                Ok(s) => {
                    cleanup(&dir);
                    return json_error(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        &format!("ffmpeg 合并失败（exit {}），轨道格式可能不受支持", s),
                    );
                }
                Err(e) => {
                    cleanup(&dir);
                    return json_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("启动 ffmpeg 失败: {}", e));
                }
            }
        }
        None => {
            if let Err(e) = tokio::fs::rename(&vfile, &ofile).await {
                cleanup(&dir);
                return json_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("整理输出文件失败: {}", e));
            }
        }
    }

    // 6) 流式回传
    let name = format!("youtube_{}.{}", video_id, out_ext);
    let ctype = match out_ext {
        "mkv" => "video/x-matroska",
        "webm" => "video/webm",
        _ => "video/mp4",
    };
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(16);
    let ofile2 = ofile.clone();
    let dir2 = dir.clone();
    tokio::task::spawn_blocking(move || {
        use std::io::Read;
        match std::fs::File::open(&ofile2) {
            Ok(mut f) => {
                let mut buf = vec![0u8; 256 * 1024];
                loop {
                    match f.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            if tx.blocking_send(Ok(Bytes::copy_from_slice(&buf[..n]))).is_err() {
                                break;
                            }
                        }
                        Err(e) => {
                            let _ = tx.blocking_send(Err(e));
                            break;
                        }
                    }
                }
            }
            Err(e) => {
                let _ = tx.blocking_send(Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    e.to_string(),
                )));
            }
        }
        let _ = std::fs::remove_dir_all(&dir2);
    });

    Response::builder()
        .header(header::CONTENT_TYPE, ctype)
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{}\"", name),
        )
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
/// 遍历一层 box：返回 (类型, payload)。兼容 64 位 largesize 与 size==0（到文件尾）。
fn mp4_boxes(b: &[u8]) -> Vec<(&[u8], &[u8])> {
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

fn mp4_resolution(b: &[u8]) -> Option<(u32, u32)> {
    /// 只有这些容器盒才值得下钻（tkhd 就挂在 moov → trak 下）。
    /// 必须白名单：正文被截断时，mdat 里的随机字节会被误读成 box 头，
    /// 若见 box 就递归，遇到伪造的 largesize(size==1) 时每层只前进 16 字节，
    /// 递归深度可达数万层，直接打爆 tokio worker 的栈（进程 abort）。
    fn is_container(t: &[u8]) -> bool {
        [
            &b"moov"[..],
            &b"trak"[..],
            &b"edts"[..],
            &b"mdia"[..],
            &b"minf"[..],
            &b"stbl"[..],
            &b"mvex"[..],
            &b"moof"[..],
            &b"traf"[..],
        ]
        .contains(&t)
    }
    // tkhd 嵌在 moov → trak → tkhd（也可能更深），递归整棵 box 树查找；
    // 多轨道时取像素面积最大的那个。深度上限是白名单之外的第二道保险。
    fn find_tkhds(b: &[u8], depth: u8, out: &mut Vec<Vec<u8>>) {
        if depth > 8 {
            return;
        }
        for (t, payload) in mp4_boxes(b) {
            if t == b"tkhd" {
                out.push(payload.to_vec());
            }
            // mdat 等叶子盒绝不下钻
            if is_container(t) {
                find_tkhds(payload, depth + 1, out);
            }
        }
    }
    let mut tkhds = Vec::new();
    find_tkhds(b, 0, &mut tkhds);
    let mut best: Option<(u64, u32, u32)> = None;
    for p in tkhds {
        if p.len() >= 8 {
            // width/height 固定在 tkhd 末尾 8 字节
            let w = u32::from_be_bytes(p[p.len() - 8..p.len() - 4].try_into().unwrap()) >> 16;
            let h = u32::from_be_bytes(p[p.len() - 4..].try_into().unwrap()) >> 16;
            if w > 0 && h > 0 && best.as_ref().map(|(a, _, _)| w as u64 * h as u64 > *a).unwrap_or(true) {
                best = Some((w as u64 * h as u64, w, h));
            }
        }
    }
    best.map(|(_, w, h)| (w, h))
}

/// 从 MP4 初始化段（ftyp+moov）读总时长（秒）：moov → mvhd。
/// mvhd 布局：version(1) flags(3) [creation modification](4 或 8) timescale(4) duration(4 或 8)，
/// v1 的时间字段是 8 字节、v0 是 4 字节，所以偏移随 version 变。
fn mp4_duration(b: &[u8]) -> Option<f64> {
    fn find_mvhd(b: &[u8], depth: u8) -> Option<Vec<u8>> {
        if depth > 8 {
            return None;
        }
        for (t, payload) in mp4_boxes(b) {
            if t == b"mvhd" {
                return Some(payload.to_vec());
            }
            if t == b"moov" {
                if let Some(v) = find_mvhd(payload, depth + 1) {
                    return Some(v);
                }
            }
        }
        None
    }
    let p = find_mvhd(b, 0)?;
    if p.len() < 4 {
        return None;
    }
    let (timescale, duration) = if p[0] == 1 {
        (
            u32::from_be_bytes(p.get(20..24)?.try_into().ok()?),
            u64::from_be_bytes(p.get(24..32)?.try_into().ok()?),
        )
    } else {
        (
            u32::from_be_bytes(p.get(12..16)?.try_into().ok()?),
            u32::from_be_bytes(p.get(16..20)?.try_into().ok()?) as u64,
        )
    };
    // duration 全 1 是「未知时长」的约定值
    if timescale == 0
        || duration == 0
        || duration == u64::MAX
        || duration == u32::MAX as u64
    {
        return None;
    }
    Some(duration as f64 / timescale as f64)
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

/// host 的注册主域（末两段），用于识别同一站点的不同 CDN 节点。
/// 例：`upos-sz-estgcos.bilivideo.com` 与 `upos-sz-mirror08c.bilivideo.com` → 都是 `bilivideo.com`。
fn reg_domain(host: &str) -> String {
    let parts: Vec<&str> = host.split('.').collect();
    let n = parts.len();
    if n >= 2 {
        format!("{}.{}", parts[n - 2], parts[n - 1])
    } else {
        host.to_string()
    }
}

/// URL 的路径目录（去掉 scheme 与 host）。同一视频的各轨道（video/audio）目录通常
/// 完全一致，是比 host 更稳的归类信号——B 站两条轨道就落在不同 CDN host 上。
fn url_path_dir(u: &str) -> String {
    let d = url_dir(u); // "https://host/a/b/"
    match d.find("://") {
        Some(i) => match d[i + 3..].find('/') {
            Some(j) => d[i + 3 + j..].to_string(),
            None => String::from("/"),
        },
        None => d.to_string(),
    }
}

/// 该条目是否指向 HLS 播放列表（决定轨道临时文件的扩展名）。
fn entry_is_hls(entry: &crate::capture::Entry) -> bool {
    let (url, content_type) = {
        let inner = entry.inner.lock().unwrap();
        (entry.url.clone(), inner.content_type.clone())
    };
    url_path(&url).ends_with(".m3u8")
        || content_type
            .as_deref()
            .map(|c| c.to_lowercase().contains("mpegurl"))
            .unwrap_or(false)
}

// ===================== YouTube SABR / UMP =====================
//
// YouTube 新版播放走 SABR：`POST /videoplayback?...&sabr=1`，URL 里没有 itag/range，
// 轨道信息在 POST 请求体里，响应是私有 UMP 格式（Content-Type: application/vnd.yt-ump）。
// 因此既不能按扩展名识别，也无法像 B 站那样整文件重拉——只能解析已抓到的响应。
//
// UMP 结构（参考 LuanRT/googlevideo 的 UmpReader / MediaHeader）：
//   [partType: UMP 变长整数] [partSize: 同] [partSize 字节 payload]
// UMP 变长整数按**首字节高位**决定宽度，与 protobuf 的 LEB128 不是一回事：
//   0xxxxxxx→1B | 10xxxxxx→2B | 110xxxxx→3B | 1110xxxx→4B | 1111xxxx→5B(后 4 字节小端)
// 媒体相关 part：
//   20 MEDIA_HEADER → protobuf MediaHeader（itag / sequence_number / is_init_seg / content_length）
//   21 MEDIA        → payload[0] = header_id，其余就是裸媒体字节（fMP4 或 WebM）
//   22 MEDIA_END    → payload[0] = header_id
// header_id 只在单个响应内唯一，所以“MEDIA 归属哪个 MediaHeader”必须逐响应关联。

/// UMP 变长整数（首字节高位标记宽度）。
fn ump_varint(b: &[u8], off: usize) -> Option<(u64, usize)> {
    let f = *b.get(off)? as u64;
    if f < 128 {
        Some((f, off + 1))
    } else if f < 192 {
        Some(((f & 0x3f) + 64 * (*b.get(off + 1)? as u64), off + 2))
    } else if f < 224 {
        Some((
            (f & 0x1f) + 32 * (*b.get(off + 1)? as u64 + 256 * (*b.get(off + 2)? as u64)),
            off + 3,
        ))
    } else if f < 240 {
        Some((
            (f & 0x0f)
                + 16
                    * (*b.get(off + 1)? as u64
                        + 256 * (*b.get(off + 2)? as u64 + 256 * (*b.get(off + 3)? as u64))),
            off + 4,
        ))
    } else {
        let s = b.get(off + 1..off + 5)?;
        Some((u32::from_le_bytes([s[0], s[1], s[2], s[3]]) as u64, off + 5))
    }
}

/// protobuf 标准 LEB128 varint（MediaHeader 内部用的是标准编码，不是 UMP 那套）。
fn pb_varint(b: &[u8], off: usize) -> Option<(u64, usize)> {
    let mut v = 0u64;
    let mut shift = 0u32;
    let mut i = off;
    loop {
        let c = *b.get(i)?;
        i += 1;
        v |= ((c & 0x7f) as u64) << shift;
        if c & 0x80 == 0 {
            return Some((v, i));
        }
        shift += 7;
        if shift > 63 {
            return None;
        }
    }
}

#[derive(Default, Clone)]
struct UmpHeader {
    header_id: u64,
    itag: u32,
    is_init: bool,
    seq: Option<i64>,
    /// 段在时间轴上的起点（ms，field 11），补拉时用来定位「从哪继续」。
    start_ms: i64,
    content_length: Option<i64>,
    duration_ms: i64,
    video_id: Option<String>,
}

/// 解析 MediaHeader 的 protobuf（只取关心的字段）。
fn ump_parse_header(p: &[u8]) -> Option<UmpHeader> {
    let mut h = UmpHeader::default();
    let mut i = 0usize;
    while i < p.len() {
        let (tag, ni) = pb_varint(p, i)?;
        i = ni;
        let field = tag >> 3;
        match tag & 7 {
            0 => {
                let (v, ni) = pb_varint(p, i)?;
                i = ni;
                match field {
                    1 => h.header_id = v,
                    3 => h.itag = v as u32,
                    8 => h.is_init = v != 0,
                    9 => h.seq = Some(v as i64),
                    11 => h.start_ms = v as i64,
                    12 => h.duration_ms = v as i64,
                    14 => h.content_length = Some(v as i64),
                    _ => {}
                }
            }
            2 => {
                let (l, ni) = pb_varint(p, i)?;
                if field == 2 {
                    if let Some(s) = p.get(ni..ni.saturating_add(l as usize)) {
                        h.video_id = Some(String::from_utf8_lossy(s).to_string());
                    }
                }
                i = ni.saturating_add(l as usize);
            }
            5 => i = i.saturating_add(4),
            1 => i = i.saturating_add(8),
            _ => return None,
        }
    }
    Some(h)
}

/// 该 UMP 媒体载荷是否为轨道初始化段（提供解码参数的那一段）。
///
/// 判据必须精确，因为**媒体的普通分片和 init 段长得很像**：
/// - fMP4：init 以 `ftyp` 开头，媒体分片以 `moof` 开头 → 看魔数即可；
/// - WebM：init 以 **EBML 头（1A45DFA3）** 开头并含 `Tracks` 元素（0x1654AE6B，声明
///   编解码器参数），而媒体分片以 **Cluster（1F43B675）** 开头。两者前 4 字节完全不同，
///   所以「EBML 头开头」本身就是有效判据；再要求含 `Tracks` 是为了多上一道保险。
fn looks_like_init(data: &[u8]) -> bool {
    if data.get(4..8).map(|s| s == b"ftyp").unwrap_or(false) {
        return true;
    }
    if data.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]) {
        let n = data.len().min(64 * 1024);
        let needle = [0x16u8, 0x54, 0xae, 0x6b];
        return data[..n].windows(4).any(|w| w == needle);
    }
    false
}

/// 单个 itag 的轨道数据：init 段 + 按 sequence_number 排列的媒体段。
#[derive(Default)]
struct SabrTrack {
    init: Option<Vec<u8>>,
    segs: std::collections::BTreeMap<i64, Vec<u8>>,
    /// seq → 该段时长（ms）。与 segs 同步去重，供统计「已抓到时长为多少」。
    ///
    /// 不能在合并轨道时取单个响应的累加值当总量：一个响应只带几个分段，
    /// 各响应还会重复覆盖同一 seq，只有按 seq 去重后求和才是真实抓到的时长。
    dur: std::collections::BTreeMap<i64, i64>,
    /// seq → 该段在时间轴上的起点（ms）。补拉时用它定位「第一个缺口从哪开始」。
    start: std::collections::BTreeMap<i64, i64>,
}

impl SabrTrack {
    /// 已抓到的总时长（秒）。断号时是「各段之和」而非首尾跨度，更贴近实际内容量。
    fn captured_secs(&self) -> f64 {
        self.dur.values().sum::<i64>() as f64 / 1000.0
    }
    /// 时间轴上最晚一段的结束时刻（ms）。没有时长信息时退回最后一段的起点。
    fn last_end_ms(&self) -> Option<i64> {
        let (seq, st) = self.start.iter().rev().next()?;
        Some(st + self.dur.get(seq).copied().unwrap_or(0))
    }
}

/// 一条视频（按 video_id 聚合）的全部轨道。
#[derive(Default)]
struct SabrVideo {
    video_id: String,
    first_entry: u64,
    host: String,
    url: String,
    tracks: std::collections::BTreeMap<u32, SabrTrack>,
}

/// 把一条 UMP 响应里的媒体并入轨道表；返回该响应涉及的 video_id。
fn ump_absorb(body: &[u8], tracks: &mut std::collections::BTreeMap<u32, SabrTrack>) -> Option<String> {
    let mut headers: std::collections::HashMap<u64, UmpHeader> = std::collections::HashMap::new();
    let mut acc: std::collections::HashMap<u64, Vec<u8>> = std::collections::HashMap::new();
    let mut video_id: Option<String> = None;
    let mut off = 0usize;

    while off < body.len() {
        let Some((ptype, o2)) = ump_varint(body, off) else {
            break;
        };
        let Some((psize, o3)) = ump_varint(body, o2) else {
            break;
        };
        let end = o3.saturating_add(psize as usize);
        if end > body.len() {
            break; // 正文被截断，残段丢弃（不完整的媒体块绝不能混进拼接结果）
        }
        let payload = &body[o3..end];
        off = end;
        match ptype {
            20 => {
                if let Some(h) = ump_parse_header(payload) {
                    if video_id.is_none() {
                        if let Some(v) = &h.video_id {
                            if !v.is_empty() {
                                video_id = Some(v.clone());
                            }
                        }
                    }
                    headers.insert(h.header_id, h);
                }
            }
            21 => {
                if !payload.is_empty() {
                    acc.entry(payload[0] as u64)
                        .or_default()
                        .extend_from_slice(&payload[1..]);
                }
            }
            _ => {} // MEDIA_END 及各类策略 part 直接忽略
        }
    }

    for (hid, data) in acc {
        let Some(h) = headers.get(&hid) else { continue };
        if h.itag == 0 {
            continue;
        }
        let t = tracks.entry(h.itag).or_default();
        // init 段必须以 ftyp（fMP4）或 EBML 头（WebM）开头。
        // 不能只看 is_init / seq 缺失：部分媒体段的 MediaHeader 就是不带 sequence_number，
        // 一旦把它当 init 写进去，就会顶掉真正的初始化段，分辨率和时长随之全读不出来。
        if (h.is_init || h.seq.is_none()) && looks_like_init(&data) {
            if t.init.as_ref().map(|v| v.len()).unwrap_or(0) < data.len() {
                t.init = Some(data);
            }
        } else if let Some(seq) = h.seq {
            if t.segs.insert(seq, data).is_none() {
                if h.duration_ms > 0 {
                    t.dur.insert(seq, h.duration_ms);
                }
                t.start.insert(seq, h.start_ms);
            }
        }
    }
    video_id
}

/// 把一批轨道并入聚合结果（init 取最长，分段按 sequence_number 去重）。
fn sabr_merge_tracks(
    dst_tracks: &mut std::collections::BTreeMap<u32, SabrTrack>,
    tracks: std::collections::BTreeMap<u32, SabrTrack>,
) {
    for (itag, t) in tracks {
        let dst = dst_tracks.entry(itag).or_default();
        let dst_len = dst.init.as_ref().map(|v| v.len()).unwrap_or(0);
        let new_len = t.init.as_ref().map(|v| v.len()).unwrap_or(0);
        if dst_len < new_len {
            dst.init = t.init;
        }
        for (seq, ms) in t.dur {
            dst.dur.insert(seq, ms);
        }
        for (seq, ms) in t.start {
            dst.start.insert(seq, ms);
        }
        for (seq, data) in t.segs {
            dst.segs.insert(seq, data);
        }
    }
}

/// 把快照里的 UMP 响应按 video_id 聚合。复用快照已拷出的正文，不再二次读 store。
fn sabr_scan(snaps: &[VidSnap]) -> Vec<SabrVideo> {
    use std::collections::BTreeMap;
    let mut out: BTreeMap<String, SabrVideo> = BTreeMap::new();
    for s in snaps {
        let is_ump = s
            .ct
            .as_deref()
            .map(|c| c.to_lowercase().contains("vnd.yt-ump"))
            .unwrap_or(false);
        if !is_ump {
            continue;
        }
        let Some(body) = &s.body else { continue };
        let mut tracks = BTreeMap::new();
        let Some(vid) = ump_absorb(body, &mut tracks) else {
            continue;
        };
        if tracks.is_empty() {
            continue;
        }
        let slot = out.entry(vid.clone()).or_insert_with(|| SabrVideo {
            video_id: vid.clone(),
            first_entry: s.id,
            host: s.host.clone(),
            url: s.url.clone(),
            tracks: BTreeMap::new(),
        });
        sabr_merge_tracks(&mut slot.tracks, tracks);
    }
    out.into_values().collect()
}

/// 轨道是否为 WebM（EBML 头），用来决定合并后的容器。
fn track_is_webm(init: Option<&Vec<u8>>) -> bool {
    init.map(|b| b.starts_with(&[0x1a, 0x45, 0xdf, 0xa3])).unwrap_or(false)
}

/// 该 itag 是否为音频轨。
///
/// **不能用数值阈值猜**：YouTube 的视频 itag 跨度极大（137/248/264/271/399…），
/// 任何「小于某值即音频」的假设都会错——实测 399 是 AV1 视频、251 是 opus 音频。
/// 因此列举已知音频 itag，其余一律当视频。
fn itag_is_audio(itag: u32) -> bool {
    matches!(
        itag,
        139 | 140 | 141 | 171 | 172 | 249 | 250 | 251 | 256 | 258 | 325 | 328 | 338
    )
}

fn itag_is_video(itag: u32) -> bool {
    !itag_is_audio(itag)
}

/// "1920x1080" → 像素面积（用于挑最佳视频轨）。
fn res_area(r: &Option<String>) -> u64 {
    r.as_deref()
        .and_then(|s| s.split_once('x'))
        .and_then(|(a, b)| Some(a.parse::<u64>().ok()? * b.parse::<u64>().ok()?))
        .unwrap_or(0)
}

// ---- SABR 补拉：把浏览器没请求到的分段用「回放 + 位置改写」从服务端要回来 ----
//
// 请求体是 protobuf，但里面没有公开文档。下面字段语义全部来自对真实请求体的
// 差分与受控实验（改一个字段 → 看服务端从哪开始发段），见 2026-09-23 工作日志：
// - 顶层 field 1 = client_abr_state，其子字段 28/29/36/39 是毫秒级播放位置，
//   服务端按它决定从时间轴哪个点开始发段；
// - 顶层 field 3 = 每轨「已缓冲区间声明」（含 seq），服务端会跳过声明过的区域
//   （只回一个 ~145B 的纯策略 part），必须整段删掉才能拿到任意位置的数据；
// - 服务端**不会**补发 init 段（只在浏览器从头播放时下发一次），init 只能来自已抓数据。

/// protobuf 标准 LEB128 varint 编码（与 pb_varint 互为逆运算）。
fn pb_enc_varint(mut v: u64) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let c = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(c);
            break;
        }
        out.push(c | 0x80);
    }
    out
}

/// 解析 protobuf 消息 → `(field, wiretype, tag_start, val_start, end)`。
/// 任何一处解析不下去都返回 None（调用方放弃补丁，宁可不动也别发坏请求）。
fn pb_split(b: &[u8]) -> Option<Vec<(u64, u8, usize, usize, usize)>> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < b.len() {
        let tag_start = i;
        let (tag, ni) = pb_varint(b, i)?;
        i = ni;
        let wt = (tag & 7) as u8;
        let val_start = i;
        match wt {
            0 => {
                let (_, ni) = pb_varint(b, i)?;
                i = ni;
            }
            1 => i += 8,
            5 => i += 4,
            2 => {
                let (l, ni) = pb_varint(b, i)?;
                i = ni.saturating_add(l as usize);
                if i > b.len() {
                    return None;
                }
                // wt2 的载荷起点在长度字节之后（val_start 语义 = 载荷起点）
                out.push((tag >> 3, wt, tag_start, ni, i));
                continue;
            }
            _ => return None,
        }
        out.push((tag >> 3, wt, tag_start, val_start, i));
    }
    Some(out)
}

/// 把 client_abr_state 里的毫秒位置字段（28/29/36/39）统一改成 target_ms。
fn sabr_patch_abr_state(body: &[u8], target_ms: i64) -> Option<Vec<u8>> {
    let fields = pb_split(body)?;
    let mut out = Vec::with_capacity(body.len() + 16);
    let mut patched = false;
    for (field, wt, ts, _, end) in fields {
        if wt == 0 && matches!(field, 28 | 29 | 36 | 39) {
            out.extend_from_slice(&pb_enc_varint((field << 3) | 0));
            out.extend_from_slice(&pb_enc_varint(target_ms as u64));
            patched = true;
            continue;
        }
        out.extend_from_slice(&body[ts..end]);
    }
    if !patched {
        // 该请求体里没有位置字段（少见）：补一个 39，protobuf 标量字段顺序无关
        out.extend_from_slice(&pb_enc_varint((39 << 3) | 0));
        out.extend_from_slice(&pb_enc_varint(target_ms as u64));
    }
    Some(out)
}

/// 把 SABR 请求体改写成「从 target_ms 毫秒处开始要数据」的形态。
fn sabr_patch_seek(body: &[u8], target_ms: i64) -> Option<Vec<u8>> {
    let fields = pb_split(body)?;
    let mut out = Vec::with_capacity(body.len() + 32);
    for (field, wt, ts, vs, end) in fields {
        if field == 3 && wt == 2 {
            continue; // 删掉已缓冲声明，否则服务端跳过声明区域
        }
        if field == 1 && wt == 2 {
            let sub = sabr_patch_abr_state(&body[vs..end], target_ms)?;
            out.extend_from_slice(&pb_enc_varint((1 << 3) | 2));
            out.extend_from_slice(&pb_enc_varint(sub.len() as u64));
            out.extend_from_slice(&sub);
            continue;
        }
        out.extend_from_slice(&body[ts..end]);
    }
    Some(out)
}

/// 提取 part 43（换 host 重试指令）里的新 URL。
fn ump_redirect_url(body: &[u8]) -> Option<String> {
    let mut off = 0usize;
    while off < body.len() {
        let (ptype, o2) = ump_varint(body, off)?;
        let (psize, o3) = ump_varint(body, o2)?;
        let end = o3.saturating_add(psize as usize);
        if end > body.len() {
            return None;
        }
        if ptype == 43 {
            let p = &body[o3..end];
            if let Some(fields) = pb_split(p) {
                for (f, wt, _, vs, e) in fields {
                    if f == 1 && wt == 2 {
                        if let Ok(s) = std::str::from_utf8(&p[vs..e]) {
                            if s.starts_with("https://") {
                                return Some(s.to_string());
                            }
                        }
                    }
                }
            }
        }
        off = end;
    }
    None
}

/// 用 curl 把一条 SABR 请求直连上游重放（不经自身抓包：不污染 store、不受 4MB 截断影响）。
fn sabr_replay_once(url: &str, body: &[u8], proxy: Option<&str>) -> Result<Vec<u8>, String> {
    let dir = std::env::temp_dir().join(format!(
        "miniproxy-sabr-replay-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let inp = dir.join("req.bin");
    let outp = dir.join("resp.bin");
    let res = (|| -> Result<Vec<u8>, String> {
        std::fs::write(&inp, body).map_err(|e| e.to_string())?;
        let mut cmd = std::process::Command::new("curl");
        cmd.args(["-s", "--max-time", "45", "-X", "POST", "-o"]).arg(&outp);
        // 隔离外部代理环境变量：curl 会读 http_proxy/https_proxy/all_proxy，
        // 若本进程是从带这些变量的 shell 里起来的，补拉会被悄悄改道（甚至劫持到
        // 别的代理上）；显式 --proxy 时同理，避免与 NO_PROXY 相互干扰。
        for k in [
            "http_proxy",
            "https_proxy",
            "all_proxy",
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "no_proxy",
            "NO_PROXY",
        ] {
            cmd.env_remove(k);
        }
        match proxy {
            Some(p) => {
                cmd.args(["--proxy", p, "--noproxy", ""]);
            }
            // 直连：显式关掉 curl 自身的代理发现（默认还会读 ~/.curlrc 里可能存在的 proxy）
            None => {
                cmd.args(["--noproxy", "*"]);
            }
        }
        cmd
            .args([
                "-H",
                "content-type: application/x-protobuf",
                "-H",
                "user-agent: Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36",
                "-H",
                "origin: https://www.youtube.com",
                "-H",
                "referer: https://www.youtube.com/",
                "-H",
                "accept-encoding: identity",
            ])
            .arg("--data-binary")
            .arg(format!("@{}", inp.display()))
            .arg(url)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let st = cmd.status().map_err(|e| format!("启动 curl 失败: {}", e))?;
        let out = std::fs::read(&outp).unwrap_or_default();
        if !st.success() && out.is_empty() {
            return Err(format!("curl exit {}", st));
        }
        Ok(out)
    })();
    let _ = std::fs::remove_dir_all(&dir);
    res
}

/// 视频轨的下一个补拉目标（ms）：主视频轨（段数最多的带 init 视频轨）里
/// 第一个缺口的起点；没有缺口就取最后一段的结束时刻。
fn sabr_next_target(tracks: &std::collections::BTreeMap<u32, SabrTrack>) -> i64 {
    let mut primary: Option<&SabrTrack> = None;
    for (itag, t) in tracks {
        if !itag_is_video(*itag) || t.segs.is_empty() || t.init.is_none() {
            continue;
        }
        match primary {
            Some(p) if p.segs.len() >= t.segs.len() => {}
            _ => primary = Some(t),
        }
    }
    let Some(t) = primary else { return 0 };
    let mut prev_seq: Option<i64> = None;
    for (seq, st) in &t.start {
        if let Some(p) = prev_seq {
            if *seq > p + 1 {
                // 缺口：从前一段的结束时刻继续
                let end = t.start.get(&p).copied().unwrap_or(0)
                    + t.dur.get(&p).copied().unwrap_or(0);
                return end;
            }
        }
        prev_seq = Some(*seq);
    }
    t.last_end_ms().unwrap_or(0)
}

/// 补拉主循环（阻塞，调用方放 spawn_blocking）：按时间轴推进重放，直到补齐/无进展/超时。
/// 返回 `(轨道表, 拉到的字节数, 结束原因)`。
fn sabr_refetch_blocking(
    cands: Vec<(String, Vec<u8>)>,
    proxy: Option<String>,
    video_id: String,
    mut tracks: std::collections::BTreeMap<u32, SabrTrack>,
) -> (std::collections::BTreeMap<u32, SabrTrack>, usize, Option<String>) {
    use std::collections::BTreeMap;
    if cands.is_empty() {
        return (tracks, 0, Some("没有可回放的 SABR 请求".into()));
    }
    // 全片时长：优先 init 段 moov 里的权威值
    let dur_ms: Option<i64> = tracks
        .values()
        .filter_map(|t| t.init.as_deref().and_then(mp4_duration))
        .map(|s| (s * 1000.0) as i64)
        .max();

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(600);
    let (mut url, mut base) = cands[0].clone();
    let mut ci = 0usize;
    let mut verified = false;
    let mut stall = 0usize;
    let mut bump_ms: i64 = 0;
    let mut fetched = 0usize;
    let mut reason: Option<String> = None;

    for _iters in 0..400usize {
        if std::time::Instant::now() > deadline {
            reason = Some("补拉超时".into());
            break;
        }
        let target = sabr_next_target(&tracks) + bump_ms;
        if let Some(d) = dur_ms {
            if target >= d.saturating_sub(1500) {
                break; // 已到片尾
            }
        }
        let Some(patched) = sabr_patch_seek(&base, target) else {
            reason = Some("请求体解析失败".into());
            break;
        };
        let resp = match sabr_replay_once(&url, &patched, proxy.as_deref()) {
            Ok(r) => r,
            Err(e) => {
                ci += 1;
                if ci >= cands.len() {
                    reason = Some(format!("回放请求失败: {}", e));
                    break;
                }
                url = cands[ci].0.clone();
                base = cands[ci].1.clone();
                verified = false;
                continue;
            }
        };
        let mut resp = resp;
        if let Some(new_url) = ump_redirect_url(&resp) {
            // 服务端让换 host 重试：拿同一个 body POST 到新地址
            url = new_url;
            match sabr_replay_once(&url, &patched, proxy.as_deref()) {
                Ok(r) => resp = r,
                Err(_) => {
                    stall += 1;
                    if stall >= 3 {
                        reason = Some("重定向后仍失败".into());
                        break;
                    }
                    continue;
                }
            }
        }
        fetched += resp.len();

        let segs_before: usize = tracks.values().map(|t| t.segs.len()).sum();
        let mut tmp: BTreeMap<u32, SabrTrack> = BTreeMap::new();
        let vid = ump_absorb(&resp, &mut tmp);
        if !verified {
            match vid {
                Some(v) if v == video_id => verified = true,
                Some(_) => {
                    // 候选请求不属于本视频，换下一个
                    ci += 1;
                    if ci >= cands.len() {
                        reason = Some("回放得到的是别的视频".into());
                        break;
                    }
                    url = cands[ci].0.clone();
                    base = cands[ci].1.clone();
                    verified = false;
                    continue;
                }
                None => {
                    // 纯策略响应（没有媒体）：会话可能已结束或被节流
                    stall += 1;
                    if stall >= 4 {
                        reason = Some("服务端不再返回媒体（播放会话可能已结束）".into());
                        break;
                    }
                    bump_ms += 15000;
                    continue;
                }
            }
        }
        sabr_merge_tracks(&mut tracks, tmp);
        let segs_after: usize = tracks.values().map(|t| t.segs.len()).sum();
        if segs_after == segs_before {
            stall += 1;
            bump_ms += 15000; // 该位置要不到新东西，往前跳
            // 换一个候选（不同节点/会话状态可能给得出数据），由 video_id 校验兜底
            ci = (ci + 1) % cands.len();
            url = cands[ci].0.clone();
            base = cands[ci].1.clone();
            verified = false;
            if stall >= 3 {
                break;
            }
        } else {
            stall = 0;
            bump_ms = 0;
        }
    }
    (tracks, fetched, reason)
}


/// 视频下载器：把抓包条目聚合为「可完整获取」的视频列表。
/// 四类来源（下载入口各不相同）：
/// - file：独立音视频文件 → /fullvideo 整文件重拉，不受存储截断影响
/// - hls：VOD m3u8（须见过 #EXT-X-ENDLIST，直播流排除）→ /fullvideo 实时拼段
/// - dash：init 齐全的 fMP4 分段组（推特 .m4s / Range 分块）→ /stitch 拼接已捕获分段
/// - sabr：YouTube UMP 流 → /umpsave 按 itag 重组已捕获分段
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
            // YouTube SABR 流的 Content-Type 是 application/vnd.yt-ump，扩展名也拿不到
            // （URL 是 /videoplayback），必须单独放行，否则整类流量在第一步就被滤掉。
            let is_ump = ct
                .as_deref()
                .map(|c| c.to_lowercase().contains("vnd.yt-ump"))
                .unwrap_or(false);
            let ct_media = ct
                .as_deref()
                .map(|c| c.starts_with("video/") || c.starts_with("audio/") || c.to_lowercase().contains("mpegurl"))
                .unwrap_or(false);
            if !ct_media && !is_ump && !VIDEO_EXTS.contains(&ext.as_str()) && ext != "m4s" {
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
                let need_body = ct_media || is_ump || ext == "m4s" || ext == "ts" || ext == "mp4";
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
        /// 组内存在完整 fMP4 成员（自带 moov，无需独立 init 也能整文件重拉）
        has_complete: bool,
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
                has_complete: false,
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
            if let Some(body) = &s.body {
                if first_box_type(body) == Some(b"ftyp") && has_mdat_box(body) {
                    g.has_complete = true;
                    if g.self_res.is_none() {
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
        // YouTube UMP 流交给下面的 SABR 分支单独聚合，这里不能当普通媒体文件罗列
        if s.ct
            .as_deref()
            .map(|c| c.to_lowercase().contains("vnd.yt-ump"))
            .unwrap_or(false)
        {
            continue;
        }
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
    let mux_available = find_ffmpeg().is_some();
    struct DashOut {
        entry_id: u64,
        name: String,
        host: String,
        url: String,
        size: u64,
        size_exact: bool,
        resolution: Option<String>,
        segments: usize,
        range_group: bool,
        /// 有分辨率 = 视频轨；无 = 音频轨
        is_video: bool,
        /// 配对成功后的音频轨 entryId；0 = 自己是被配走的音频轨（不再单独输出）
        audio_entry_id: Option<u64>,
    }
    let mut dash_items: Vec<DashOut> = Vec::new();
    for ((_, _, base), g) in dash {
        // 有独立 init 或组内自带完整 moov 均可（B 站 m4s 自带 moov，无需 init）
        if g.init_body.is_none() && !g.has_complete {
            continue;
        }
        let range_group = g.urls.len() <= 1; // 所有分段同一 URL = Range 分块组
        let init_res = g.init_body.as_deref().and_then(mp4_resolution);
        let resolution = g
            .self_res
            .or(init_res)
            .map(|(w, h)| format!("{}x{}", w, h));
        let (size, exact) = if range_group {
            match g.total_len {
                Some(t) => (t, true),
                None => (g.size, false),
            }
        } else {
            (g.size, false) // 分段文件组只能按已捕获合计估算
        };
        dash_items.push(DashOut {
            entry_id: g.repr_id,
            name: base,
            host: g.host,
            url: g.url,
            size,
            size_exact: exact,
            resolution,
            segments: g.segments,
            range_group,
            is_video: false,
            audio_entry_id: None,
        });
    }
    for v in dash_items.iter_mut() {
        v.is_video = v.resolution.is_some();
    }
    // 音视频轨配对：基名前缀（去掉最后一段码率/轨道号）相同，一视频一音频。
    // host 判定不能要求完全相等——同一视频的视频/音频轨常被分发到不同 CDN 节点
    // （B 站：estgcos / mirror08c 等）。改判「同 host，或同注册主域且 URL 目录相同」。
    // 仅在 ffmpeg 可用时启用（否则合并下载不可用，两条分开列）。
    let track_prefix = |n: &str| {
        n.rsplit_once('-')
            .map(|(p, _)| p.to_string())
            .unwrap_or_else(|| n.to_string())
    };
    if mux_available {
        let dirs: Vec<String> = dash_items.iter().map(|d| url_path_dir(&d.url)).collect();
        let rdoms: Vec<String> = dash_items.iter().map(|d| reg_domain(&d.host)).collect();
        // 同一视频抓到多档清晰度/多档音频时，优先给画质最高、码率最大的轨配对，
        // 而不是按 BTreeMap 字典序先到先得（否则 4K 轨可能被配上 64K 音频）。
        let area = |i: usize| -> u64 {
            dash_items[i]
                .resolution
                .as_deref()
                .and_then(|r| {
                    let (a, b) = r.split_once('x')?;
                    Some(a.parse::<u64>().ok()? * b.parse::<u64>().ok()?)
                })
                .unwrap_or(0)
        };
        let mut vorder: Vec<usize> = (0..dash_items.len())
            .filter(|&i| dash_items[i].is_video)
            .collect();
        vorder.sort_by(|&a, &b| area(b).cmp(&area(a)).then(dash_items[b].size.cmp(&dash_items[a].size)));
        let mut aorder: Vec<usize> = (0..dash_items.len())
            .filter(|&i| !dash_items[i].is_video)
            .collect();
        aorder.sort_by(|&a, &b| dash_items[b].size.cmp(&dash_items[a].size));
        for i in vorder {
            if dash_items[i].audio_entry_id.is_some() {
                continue;
            }
            let vp = track_prefix(&dash_items[i].name);
            let host = dash_items[i].host.clone();
            let vdir = dirs[i].clone();
            let rdom = rdoms[i].clone();
            if let Some(j) = aorder.iter().copied().find(|&j| {
                dash_items[j].audio_entry_id.is_none()
                    && track_prefix(&dash_items[j].name) == vp
                    && (dash_items[j].host == host
                        || (vdir.len() > 1 && dirs[j] == vdir && rdoms[j] == rdom))
            }) {
                dash_items[i].size += dash_items[j].size;
                dash_items[i].size_exact &= dash_items[j].size_exact;
                dash_items[i].audio_entry_id = Some(dash_items[j].entry_id);
                dash_items[j].audio_entry_id = Some(0); // 标记已被配走
            }
        }
    }
    for v in dash_items {
        if v.audio_entry_id == Some(0) {
            continue; // 已被配进对应视频轨
        }
        items.push(serde_json::json!({
            "entryId": v.entry_id, "kind": "dash", "name": v.name, "host": v.host,
            "url": v.url, "size": v.size, "sizeExact": v.size_exact,
            "resolution": v.resolution, "durationSec": serde_json::Value::Null,
            "segments": v.segments, "rangeGroup": v.range_group,
            "audioEntryId": v.audio_entry_id,
        }));
    }

    // ---- 4. YouTube SABR（UMP）：按 video_id 聚合已捕获的分段 ----
    // 局限：只能重组浏览器已经请求过的分段，没有 manifest 可以枚举全集，
    // 因此无法像 B 站那样整文件重拉；用户播放到哪就抓到哪。
    for v in sabr_scan(&snaps) {
        // 先选轨，再把体积/段数只按「将被下载的那两条轨」统计——同一视频常有
        // 多档清晰度与多档音频，全量累加会把 size 报成虚高好几倍。
        // (itag, 字节数, 分辨率)——视频按像素面积挑最佳，并列再看字节数
        let mut best_video: Option<(u32, u64, Option<String>)> = None;
        // 音频候选：(itag, 字节数, 是否 WebM/opus)
        let mut audio_cands: Vec<(u32, u64, bool)> = Vec::new();
        for (itag, t) in &v.tracks {
            // 轨道必须同时具备 init 段与媒体段才能成片：init 提供解码参数
            // （fMP4 的 moov / WebM 的 EBML 头），缺了它拼出来的文件播放器打不开。
            if t.init.is_none() || t.segs.is_empty() {
                continue;
            }
            let bytes = t.segs.values().map(|b| b.len() as u64).sum::<u64>();
            if itag_is_video(*itag) {
                // 分辨率读自 fMP4 的 init 段（moov→trak→tkhd）；WebM 轨读不到就留空
                let res = t
                    .init
                    .as_deref()
                    .and_then(mp4_resolution)
                    .map(|(w, h)| format!("{}x{}", w, h));
                let better = match &best_video {
                    None => true,
                    Some((_, bbytes, bres)) => {
                        (res_area(&res), bytes) > (res_area(bres), *bbytes)
                    }
                };
                if better {
                    best_video = Some((*itag, bytes, res));
                }
            } else {
                audio_cands.push((*itag, bytes, track_is_webm(t.init.as_ref())));
            }
        }
        let Some((vitag, vbytes, resolution)) = best_video else {
            continue;
        };
        let vtrack = &v.tracks[&vitag];
        let v_webm = track_is_webm(vtrack.init.as_ref());
        // 优先挑与视频同容器的音频：fMP4 视频 + mp4 音频能直接出 .mp4，
        // 混到 WebM/opus 就只装得进 .mkv。同容器再按体积（≈码率）取大的。
        audio_cands.sort_by_key(|(_, b, w)| (if v_webm { *w } else { !*w }, std::cmp::Reverse(*b)));
        let best_audio = audio_cands.first().map(|(i, b, _)| (*i, *b));
        let atrack = best_audio.and_then(|(a, _)| v.tracks.get(&a));
        // 时长优先用 init 段 moov 里的权威总时长（fMP4 才有），读不到就退回已抓时长
        let captured_secs = vtrack.captured_secs();
        let duration = vtrack
            .init
            .as_deref()
            .and_then(mp4_duration)
            .or(if captured_secs > 0.0 {
                Some(captured_secs)
            } else {
                None
            });
        // 实际抓到的时长：选中视频轨各段时长之和（按 sequence_number 去重）。
        // SABR 没有 manifest 可枚举全集，浏览器没播到的段就不会经过代理，
        // 所以「抓到 830s / 全长 1014s」是常态而非 bug。
        let captured = if captured_secs > 0.0 {
            Some(captured_secs)
        } else {
            None
        };
        // 分段连续性：sequence_number 应自 1 起连续（缺号 = 浏览器没请求到那一段）
        let seqs: Vec<i64> = vtrack.segs.keys().copied().collect();
        let complete = match (seqs.first(), seqs.last()) {
            (Some(f), Some(l)) => *f == 1 && (*l - *f + 1) as usize == seqs.len(),
            _ => false,
        };
        items.push(serde_json::json!({
            "entryId": v.first_entry, "kind": "sabr",
            "name": format!("youtube_{}", v.video_id),
            "host": v.host, "url": v.url,
            "size": vbytes + best_audio.map(|(_, b)| b).unwrap_or(0),
            "sizeExact": true,
            "resolution": resolution,
            "durationSec": duration
                .map(|d| serde_json::json!(d))
                .unwrap_or(serde_json::Value::Null),
            "capturedSec": captured
                .map(|d| serde_json::json!(d))
                .unwrap_or(serde_json::Value::Null),
            "segments": seqs.len() + atrack.map(|t| t.segs.len()).unwrap_or(0),
            "rangeGroup": false,
            "videoItag": vitag,
            "audioItag": best_audio.map(|(i, _)| i).unwrap_or(0),
            "complete": complete,
        }));
    }

    // 大的排前面，同尺寸按新记录优先
    items.sort_by(|a, b| {        let sa = a["size"].as_u64().unwrap_or(0);
        let sb = b["size"].as_u64().unwrap_or(0);
        sb.cmp(&sa).then(b["entryId"].as_u64().cmp(&a["entryId"].as_u64()))
    });
    json_response(serde_json::json!({
        "items": items,
        "muxAvailable": mux_available,
    }))
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

#[cfg(test)]
mod sabr_tests {
    use super::*;
    use std::collections::BTreeMap;

    /// 测试用的 UMP 变长整数编码（与 ump_varint 的解码规则互为逆运算）。
    ///
    /// 注意各宽度的位权重很反直觉：2 字节形式是 (首字节低 6 位) + 64×次字节，
    /// 3 字节形式是 (首字节低 5 位) + 32×次字节 + 8192×第三字节。
    fn uv_enc(v: u64) -> Vec<u8> {
        if v < 128 {
            vec![v as u8]
        } else if v < 16384 {
            vec![0x80 | (v & 0x3f) as u8, (v >> 6) as u8]
        } else {
            vec![
                0xc0 | (v & 0x1f) as u8,
                ((v >> 5) & 0xff) as u8,
                (v >> 13) as u8,
            ]
        }
    }

    /// MediaHeader 内部字段用的是标准 protobuf LEB128，别和上面的 UMP 变长整数混用。
    fn leb(mut v: u64) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let c = (v & 0x7f) as u8;
            v >>= 7;
            if v != 0 {
                out.push(c | 0x80);
            } else {
                out.push(c);
                break;
            }
        }
        out
    }

    /// protobuf 长度分隔字段
    fn pb_len(field: u64, val: &[u8]) -> Vec<u8> {
        let mut out = leb((field << 3) | 2);
        out.extend_from_slice(&leb(val.len() as u64));
        out.extend_from_slice(val);
        out
    }

    /// protobuf varint 字段
    fn pb_var(field: u64, val: u64) -> Vec<u8> {
        let mut out = leb(field << 3);
        out.extend_from_slice(&leb(val));
        out
    }

    fn part(ptype: u64, payload: &[u8]) -> Vec<u8> {
        let mut out = uv_enc(ptype);
        out.extend_from_slice(&uv_enc(payload.len() as u64));
        out.extend_from_slice(payload);
        out
    }

    /// 组装一个 MediaHeader。`seq = None` 表示不下发 sequence_number。
    fn header(id: u64, itag: u64, is_init: bool, seq: Option<u64>, dur_ms: u64) -> Vec<u8> {
        let mut h = pb_var(1, id);
        h.extend(pb_len(2, b"tYvu6IpSfiM"));
        h.extend(pb_var(3, itag));
        if is_init {
            h.extend(pb_var(8, 1));
        }
        if let Some(s) = seq {
            h.extend(pb_var(9, s));
            h.extend(pb_var(11, s * 7000));
        }
        h.extend(pb_var(12, dur_ms));
        h
    }

    /// MEDIA part 的载荷 = header_id + 裸媒体字节
    fn media(header_id: u8, bytes: &[u8]) -> Vec<u8> {
        let mut out = vec![header_id];
        out.extend_from_slice(bytes);
        out
    }

    const EBML: [u8; 4] = [0x1a, 0x45, 0xdf, 0xa3]; // WebM 初始化段开头
    const TRACKS: [u8; 4] = [0x16, 0x54, 0xae, 0x6b];
    const CLUSTER: [u8; 4] = [0x1f, 0x43, 0xb6, 0x75]; // WebM 媒体分片开头
    const FTYP: [u8; 8] = [0, 0, 0, 0x1c, b'f', b't', b'y', b'p'];
    const MOOF: [u8; 8] = [0, 0, 0x0d, 0x7c, b'm', b'o', b'o', b'f'];

    #[test]
    fn varint_roundtrip() {
        for v in [0u64, 1, 127, 128, 300, 16383, 16384, 200000, 262143] {
            let e = uv_enc(v);
            let (got, n) = ump_varint(&e, 0).expect("decode");
            assert_eq!(got, v, "值 {} 编码 {:?}", v, e);
            assert_eq!(n, e.len());
        }
    }

    #[test]
    fn itag_kind() {
        // 实测结论：399 是 AV1 视频、251 是 opus 音频，不能用数值大小猜
        assert!(itag_is_video(399) && itag_is_video(137) && itag_is_video(271));
        assert!(itag_is_audio(251) && itag_is_audio(140) && itag_is_audio(249));
    }

    #[test]
    fn init_段识别() {
        let mut webm_init = EBML.to_vec();
        webm_init.extend_from_slice(&[0u8; 60]); // EBML 头 + 段头
        webm_init.extend_from_slice(&TRACKS);
        assert!(looks_like_init(&webm_init), "EBML 头 + Tracks 是 WebM init");

        let mut webm_media = CLUSTER.to_vec();
        webm_media.extend_from_slice(&[0u8; 4000]);
        assert!(!looks_like_init(&webm_media), "Cluster 开头的普通分片不是 init");

        let mut ebml_no_tracks = EBML.to_vec();
        ebml_no_tracks.extend_from_slice(&[0u8; 4000]);
        assert!(
            !looks_like_init(&ebml_no_tracks),
            "缺 Tracks 的载荷不能顶掉真正的初始化段"
        );

        assert!(looks_like_init(&FTYP), "fMP4 init 以 ftyp 开头");
        assert!(!looks_like_init(&MOOF), "moof 开头的普通分片不是 init");
    }

    #[test]
    fn 聚合轨道() {
        let mut webm_init = media(0, &{
            let mut v = EBML.to_vec();
            v.extend_from_slice(&[0u8; 60]);
            v.extend_from_slice(&TRACKS);
            v.extend_from_slice(&[0u8; 1900]);
            v
        });
        let mut body = part(20, &header(0, 251, true, None, 0));
        body.extend(part(21, &webm_init));

        let mut cluster = CLUSTER.to_vec();
        cluster.extend_from_slice(&[7u8; 130_000]);
        let med = media(1, &cluster);
        body.extend(part(20, &header(1, 251, false, Some(1), 10_000)));
        body.extend(part(21, &med));
        body.extend(part(22, &[1]));

        // 视频：init(fMP4) + 一个 moof 分片
        let mut vinit = FTYP.to_vec();
        vinit.extend_from_slice(&[0u8; 2704]);
        body.extend(part(20, &header(2, 399, true, None, 0)));
        body.extend(part(21, &media(2, &vinit)));
        let mut moof = MOOF.to_vec();
        moof.extend_from_slice(&[9u8; 60_000]);
        body.extend(part(20, &header(3, 399, false, Some(1), 7_000)));
        body.extend(part(21, &media(3, &moof)));
        body.extend(part(22, &[3]));

        // 一段「无 sequence_number 但以 EBML 头开头、不含 Tracks」的载荷：
        // 老实现会把它当 init，从而顶掉真正的初始化段
        let trap = media(4, &{
            let mut v = EBML.to_vec();
            v.extend_from_slice(&[3u8; 5000]);
            v
        });
        body.extend(part(20, &header(4, 251, false, None, 0)));
        body.extend(part(21, &trap));

        let mut tracks: BTreeMap<u32, SabrTrack> = BTreeMap::new();
        let vid = ump_absorb(&body, &mut tracks);
        assert_eq!(vid.as_deref(), Some("tYvu6IpSfiM"));

        let a = tracks.get(&251).expect("音频轨");
        let init = a.init.as_ref().expect("音频 init");
        assert!(init.starts_with(&EBML) && init.windows(4).any(|w| w == TRACKS));
        assert_eq!(a.segs.len(), 1, "音频只有 1 个带序号的媒体段");
        assert_eq!(a.captured_secs(), 10.0);

        let v = tracks.get(&399).expect("视频轨");
        assert!(v.init.as_ref().unwrap().starts_with(&FTYP));
        assert_eq!(v.segs.len(), 1);
        assert_eq!(v.captured_secs(), 7.0);
        assert_eq!(v.segs.keys().copied().collect::<Vec<_>>(), vec![1]);
    }

    #[test]
    fn 补丁与编解码() {
        // LEB128 编码/解码往返
        for v in [0u64, 1, 127, 128, 300, 16383, 16384, 1 << 20, u32::MAX as u64] {
            let e = pb_enc_varint(v);
            let (got, n) = pb_varint(&e, 0).expect("decode");
            assert_eq!(got, v);
            assert_eq!(n, e.len());
        }
        // pb_split 解析 + 补丁保真：改位置字段后其它字段原样保留、缓冲声明被删
        let mut msg = Vec::new();
        msg.extend_from_slice(&pb_enc_varint((1 << 3) | 2)); // field1, len-delim
        {
            let mut sub = Vec::new();
            sub.extend_from_slice(&pb_enc_varint((19 << 3) | 0));
            sub.extend_from_slice(&pb_enc_varint(1180));
            sub.extend_from_slice(&pb_enc_varint((29 << 3) | 0));
            sub.extend_from_slice(&pb_enc_varint(332416));
            msg.extend_from_slice(&pb_enc_varint(sub.len() as u64));
            msg.extend_from_slice(&sub);
        }
        msg.extend_from_slice(&pb_enc_varint((3 << 3) | 2)); // field3 缓冲声明
        msg.extend_from_slice(&pb_enc_varint(3));
        msg.extend_from_slice(b"abc");
        msg.extend_from_slice(&pb_enc_varint((5 << 3) | 0)); // field5 varint
        msg.extend_from_slice(&pb_enc_varint(42));

        let patched = sabr_patch_seek(&msg, 600_000).expect("patch");
        let fields = pb_split(&patched).expect("split");
        let kinds: Vec<u64> = fields.iter().map(|f| f.0).collect();
        assert_eq!(kinds, vec![1, 5], "field3（缓冲声明）应被删除");
        // field1 内部：19 原样，29 变成 600000
        let (_, _, _, vs, e) = fields[0];
        let sub_msg = &patched[vs..e];
        let sub = pb_split(sub_msg).expect("sub");
        let mut got19 = None;
        let mut got29 = None;
        for (f, wt, _, vst, e2) in sub {
            if f == 19 && wt == 0 {
                got19 = pb_varint(&sub_msg[vst..e2], 0).map(|x| x.0);
            }
            if f == 29 && wt == 0 {
                got29 = pb_varint(&sub_msg[vst..e2], 0).map(|x| x.0);
            }
        }
        assert_eq!(got19, Some(1180));
        assert_eq!(got29, Some(600_000));
        // field5 原样保留
        let (_, _, _, vst, e2) = fields[1];
        assert_eq!(pb_varint(&patched[vst..e2], 0).unwrap().0, 42);
    }

    #[test]
    fn 下一个补拉目标() {
        let mut tracks = BTreeMap::new();
        let mut t = SabrTrack::default();
        t.init = Some(b"ftypxxxx".to_vec());
        for (seq, st, d) in [(1i64, 0i64, 5000i64), (2, 5000, 5000), (4, 15000, 5000)] {
            t.segs.insert(seq, vec![0u8; 8]);
            t.start.insert(seq, st);
            t.dur.insert(seq, d);
        }
        tracks.insert(399u32, t);
        // seq 3 缺失：应从 seq2 的结束时刻（10000ms）继续
        assert_eq!(sabr_next_target(&tracks), 10_000);
        // 补上 3 之后：目标是最后一段结束（20000）
        let t = tracks.get_mut(&399).unwrap();
        t.segs.insert(3, vec![0u8; 8]);
        t.start.insert(3, 10000);
        t.dur.insert(3, 5000);
        assert_eq!(sabr_next_target(&tracks), 20_000);
    }

    // ---- 补拉链路的离线端到端验证 ----
    //
    // Google 出口不通时没法真机复验，这里用本地假 SABR 服务端复刻服务端的关键行为，
    // 把 sabr_refetch_blocking 的整条循环跑完：
    //   · 请求体里只要还留着 f3（已缓冲声明）→ 一律回空响应（模拟服务端「跳过已声明区域」）
    //   · 删掉 f3 后 → 按 f1 里的位置返回该位置之后的分段
    // 断言：浏览器只缓冲了开头两段时，能把整条视频轨补全。

    fn find_sub(hay: &[u8], needle: &[u8]) -> Option<usize> {
        if needle.is_empty() || hay.len() < needle.len() {
            return None;
        }
        hay.windows(needle.len()).position(|w| w == needle)
    }

    /// 复刻「服务端如何读请求」：带 f3 的一律拒发（返回 None）；
    /// 否则从 f1 的 28/29/36/39 里取出客户端声明的位置。
    fn sabr_body_pos(body: &[u8]) -> Option<i64> {
        let fields = pb_split(body)?;
        if fields.iter().any(|(f, _, _, _, _)| *f == 3) {
            return None; // 还带着缓冲声明 → 服务端会跳过，不给媒体
        }
        for (f, wt, _ts, vs, e) in fields {
            if f != 1 || wt != 2 {
                continue;
            }
            let sub = &body[vs..e];
            for (sf, swt, _a, svs, _b) in pb_split(sub)? {
                if matches!(sf, 28 | 29 | 36 | 39) && swt == 0 {
                    return pb_varint(sub, svs).map(|(v, _)| v as i64);
                }
            }
        }
        None
    }

    /// 测试用 MediaHeader（start_ms 可控，helper `header` 把 start 硬编码成 seq*7000）。
    fn hdr(id: u64, itag: u64, seq: Option<u64>, start_ms: i64, dur_ms: u64) -> Vec<u8> {
        let mut h = pb_var(1, id);
        h.extend(pb_len(2, b"tYvu6IpSfiM"));
        h.extend(pb_var(3, itag));
        if let Some(s) = seq {
            h.extend(pb_var(9, s));
            h.extend(pb_var(11, start_ms as u64));
        }
        h.extend(pb_var(12, dur_ms));
        h
    }

    /// 本地假 SABR 服务端。返回 (回放 URL, 总请求数, 成功返回媒体的次数)。
    fn spawn_fake_sabr(
        segs: Vec<(u64, i64, i64)>,
        itag: u64,
    ) -> (
        String,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) {
        use std::io::{Read, Write};
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().unwrap().port();
        let hits = Arc::new(AtomicUsize::new(0));
        let served = Arc::new(AtomicUsize::new(0));
        let (h2, s2) = (hits.clone(), served.clone());

        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { continue };
                h2.fetch_add(1, Ordering::SeqCst);

                // 读满一个请求（按 content-length 判断正文结束）
                let mut buf: Vec<u8> = Vec::new();
                let mut tmp = [0u8; 8192];
                let mut body_at: Option<usize> = None;
                let mut want = 0usize;
                loop {
                    let n = s.read(&mut tmp).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    if body_at.is_none() {
                        if let Some(p) = find_sub(&buf, b"\r\n\r\n") {
                            let head = String::from_utf8_lossy(&buf[..p]).to_ascii_lowercase();
                            let cl = head
                                .lines()
                                .find_map(|l| {
                                    l.strip_prefix("content-length:")?
                                        .trim()
                                        .parse::<usize>()
                                        .ok()
                                })
                                .unwrap_or(0);
                            body_at = Some(p + 4);
                            want = p + 4 + cl;
                        }
                    }
                    if body_at.is_some() && buf.len() >= want {
                        break;
                    }
                    if buf.len() > 16_000_000 {
                        break;
                    }
                }

                let at = body_at.unwrap_or(buf.len());
                let body: &[u8] = buf.get(at..).unwrap_or(&[]);

                let mut out: Vec<u8> = Vec::new();
                if let Some(pos) = sabr_body_pos(body) {
                    let picked: Vec<(u64, i64, i64)> = segs
                        .iter()
                        .filter(|(_, st, _)| *st >= pos)
                        .take(2)
                        .copied()
                        .collect();
                    if !picked.is_empty() {
                        for (i, (seq, st, dur)) in picked.iter().enumerate() {
                            let id = (i + 1) as u64;
                            out.extend(part(20, &hdr(id, itag, Some(*seq), *st, *dur as u64)));
                        }
                        for i in 0..picked.len() {
                            out.extend(part(21, &media((i + 1) as u8, &MOOF)));
                        }
                        s2.fetch_add(1, Ordering::SeqCst);
                    }
                }

                let head = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/vnd.yt-ump\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    out.len()
                );
                let _ = s.write_all(head.as_bytes());
                let _ = s.write_all(&out);
                let _ = s.flush();
            }
        });

        (
            format!("http://127.0.0.1:{}/videoplayback?sabr=1&ump=1", port),
            hits,
            served,
        )
    }

    #[test]
    fn 补拉能补齐整轨() {
        use std::sync::atomic::Ordering;
        const ITAG: u64 = 399;
        const DUR: i64 = 5000;
        // 服务端手上共有 8 段（seq 1..8，每段 5s）
        let all: Vec<(u64, i64, i64)> =
            (1..=8u64).map(|s| (s, (s as i64 - 1) * DUR, DUR)).collect();
        let (url, hits, served) = spawn_fake_sabr(all.clone(), ITAG);

        // 起点：浏览器只缓冲了 init + 前 2 段 → seq3 起是缺口
        let mut tracks: BTreeMap<u32, SabrTrack> = BTreeMap::new();
        let mut t = SabrTrack::default();
        t.init = Some(FTYP.to_vec());
        for (seq, st, d) in all.iter().take(2) {
            t.segs.insert(*seq as i64, vec![0u8; 16]);
            t.start.insert(*seq as i64, *st);
            t.dur.insert(*seq as i64, *d);
        }
        tracks.insert(ITAG as u32, t);

        // 浏览器那次的请求体：有 f1（位置）也有 f3（已缓冲声明）。
        // f3 必须被删掉——留着的话假服务端（和真服务端一样）什么都不会给。
        let mut f1 = pb_var(1, 0);
        f1.extend(pb_var(29, 0)); // 位置字段
        f1.extend(pb_len(38, b"x")); // 分辨率表，应被原样保留
        let mut body = pb_len(1, &f1);
        let mut f3 = pb_var(1, ITAG);
        f3.extend(pb_var(3, 0));
        body.extend(pb_len(3, &f3));

        let (out, bytes, reason) = sabr_refetch_blocking(
            vec![(url, body)],
            None,
            "tYvu6IpSfiM".to_string(),
            tracks,
        );

        let track = out.get(&(ITAG as u32)).expect("视频轨应当还在");
        let seqs: Vec<i64> = track.segs.keys().copied().collect();
        assert_eq!(
            seqs,
            (1..=8).collect::<Vec<i64>>(),
            "缺口与片尾都该被补上；结束原因 {:?}",
            reason
        );
        assert!(bytes > 0, "应当真的拉到了字节");
        assert!(
            served.load(Ordering::SeqCst) >= 3,
            "至少 3 轮应当拿到媒体，实际 {}（hits={}）",
            served.load(Ordering::SeqCst),
            hits.load(Ordering::SeqCst)
        );
        eprintln!(
            "补拉结束: +{} 字节, {} 次请求, {} 次命中, 原因 {:?}",
            bytes,
            hits.load(Ordering::SeqCst),
            served.load(Ordering::SeqCst),
            reason
        );
    }

    #[test]
    fn 请求体带缓冲声明时服务端不给媒体() {
        // 反向对照：确认上面的假服务端确实复刻了「f3 在 → 拒发」这一关键行为，
        // 否则「补拉能补齐整轨」这个测试就无法证明 sabr_patch_seek 真的删掉了 f3。
        let mut f1 = pb_var(1, 0);
        f1.extend(pb_var(29, 300_000));
        let mut body = pb_len(1, &f1);
        let mut f3 = pb_var(1, 399);
        f3.extend(pb_var(3, 0));
        body.extend(pb_len(3, &f3));
        assert_eq!(sabr_body_pos(&body), None, "带 f3 应当被拒");

        // 删掉 f3（正是 sabr_patch_seek 做的事）之后就能读出位置
        let patched = sabr_patch_seek(&body, 600_000).expect("补丁应当成功");
        assert_eq!(
            sabr_body_pos(&patched),
            Some(600_000),
            "位置应被改写成目标毫秒"
        );
    }
}
