//! 抓包记录：模型 / 存储 / SSE 广播 / 内容解码 / HTTP 转发与捕获。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use hyper::body::{Body, Bytes, HttpBody};
use hyper::{header, HeaderMap, Request, Response, StatusCode};

use crate::util;
use crate::App;

/// 存储请求/响应正文的最大字节数（超出截断）。
pub const BODY_CAP: usize = 4 * 1024 * 1024;

/// 参与关键词搜索的单侧正文上限（请求体 / 响应体各自）。避免超大响应把内存和单次匹配拖垮。
pub const SEARCH_BODY_CAP: usize = 128 * 1024;

/// 详情接口附带「原始数据」视图（hex / base64 / 下载）的单侧上限：
/// 超出只回传前 N 字节并置 truncated（原始大小仍在 size 字段里）。
pub const RAW_VIEW_CAP: usize = 256 * 1024;

/// 原始字节视图：`{ b64, size, truncated }`，无正文时为 null。
/// 前端据此渲染十六进制 / Base64 / 下载，解决二进制或压缩内容在文本视图里显示为乱码的问题。
fn raw_json(b: &Option<Vec<u8>>) -> serde_json::Value {
    use base64::Engine as _;
    match b {
        None => serde_json::Value::Null,
        Some(bytes) => {
            let truncated = bytes.len() > RAW_VIEW_CAP;
            let slice = if truncated {
                &bytes[..RAW_VIEW_CAP]
            } else {
                &bytes[..]
            };
            serde_json::json!({
                "b64": base64::engine::general_purpose::STANDARD.encode(slice),
                "size": bytes.len(),
                "truncated": truncated,
            })
        }
    }
}

#[derive(Clone, serde::Serialize)]
pub struct WsMessage {
    pub dir: &'static str,     // c2s / s2c
    pub kind: String,          // text / binary / ping / pong / close
    pub size: usize,           // payload 原始字节数
    pub data: Option<String>,  // 文本内容（超出连接总额度时截断）
    /// 文本内容是否被截断（`data` 不完整，仅前若干字节）
    pub truncated: bool,
    pub ts: u128,
}

/// 连接来源：客户端进程名（仅本机可识别）+ 对端 IP。
#[derive(Clone, Default)]
pub struct ClientInfo {
    pub name: Option<String>,
    pub ip: String,
}

impl ClientInfo {
    pub fn is_local(&self) -> bool {
        matches!(self.ip.as_str(), "127.0.0.1" | "::1" | "")
    }
}

pub struct EntryInner {
    pub resp_status: Option<u16>,
    pub resp_headers: Option<Vec<(String, String)>>,
    pub req_body: Option<Vec<u8>>,
    pub resp_body: Option<Vec<u8>>,
    pub resp_decoded: Option<Vec<u8>>,
    pub content_encoding: Option<String>,
    pub content_type: Option<String>,
    pub req_truncated: bool,
    pub resp_truncated: bool,
    pub done: bool,
    pub ws_messages: Vec<WsMessage>,
    pub ws_closed: bool,
    /// 本连接已保留的 WS 文本字节数（用于总额度控制，见 `ws::WS_TEXT_BUDGET`）
    pub ws_text_used: usize,
    pub tcp_hex: Option<String>,
    pub bytes_up: u64,
    pub bytes_down: u64,
    pub error: Option<String>,
    pub started_at: u128,
    /// 关键词搜索用的「正文小写缓存」：`(内容指纹, 小写文本)`。
    /// 首次搜索时按需构建，内容变化（指纹变化）后自动重建，避免每次按键全量转码。
    pub search_cache: Option<(u64, Arc<String>)>,
}

impl EntryInner {
    /// 搜索内容指纹：只取各来源的长度/状态等廉价特征。
    /// 正文各字段在生命周期内只写入一次（请求体在建立条目时、响应体在流结束时），
    /// 因此长度 + 状态 + WS 条数足以可靠地判定缓存是否过期。
    fn search_sig(&self) -> u64 {
        let mut h: u64 = 0xcbf29ce484222325;
        let mut mix = |v: u64| {
            h ^= v;
            h = h.wrapping_mul(0x100000001b3);
        };
        mix(self.req_body.as_ref().map(|b| b.len()).unwrap_or(0) as u64);
        mix(self.resp_body.as_ref().map(|b| b.len()).unwrap_or(0) as u64);
        mix(self.resp_decoded.as_ref().map(|b| b.len()).unwrap_or(0) as u64);
        mix(self.resp_status.unwrap_or(0) as u64);
        mix(self.resp_headers.as_ref().map(|h| h.len()).unwrap_or(0) as u64);
        mix(self.ws_messages.len() as u64);
        mix(self.done as u64);
        h
    }
}

pub struct Entry {
    pub id: u64,
    pub ts: u128,
    pub kind: &'static str, // http / ws / tcp
    pub method: String,
    pub url: String,
    pub host: String,
    /// 站点（主域名，用于「按站点分组/筛选」）
    pub site: String,
    /// 发起请求的客户端进程名（用于「按应用分组/筛选」）
    pub client: Option<String>,
    /// 对端 IP（识别局域网设备，如手机抓包）
    pub client_ip: String,
    pub req_headers: Vec<(String, String)>,
    pub inner: Mutex<EntryInner>,
}

pub struct Store {
    pub entries: Mutex<VecDeque<Arc<Entry>>>,
    pub tx: tokio::sync::broadcast::Sender<Arc<Entry>>,
    pub cap: usize,
    next_id: AtomicU64,
}

impl Store {
    pub fn new(cap: usize) -> Self {
        let (tx, _) = tokio::sync::broadcast::channel(1024);
        Self {
            entries: Mutex::new(VecDeque::new()),
            tx,
            cap,
            next_id: AtomicU64::new(1),
        }
    }

    pub fn new_entry(
        &self,
        kind: &'static str,
        method: &str,
        url: &str,
        host: &str,
        req_headers: Vec<(String, String)>,
        client: ClientInfo,
    ) -> Arc<Entry> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let ClientInfo { name, ip } = client;
        Arc::new(Entry {
            id,
            ts: now_ms(),
            kind,
            method: method.to_string(),
            url: url.to_string(),
            host: host.to_string(),
            site: crate::attrib::site_of(host),
            client: name,
            client_ip: ip,
            req_headers,
            inner: Mutex::new(EntryInner {
                resp_status: None,
                resp_headers: None,
                req_body: None,
                resp_body: None,
                resp_decoded: None,
                content_encoding: None,
                content_type: None,
                req_truncated: false,
                resp_truncated: false,
                done: false,
                ws_messages: Vec::new(),
                ws_closed: false,
                ws_text_used: 0,
                tcp_hex: None,
                bytes_up: 0,
                bytes_down: 0,
                error: None,
                started_at: now_ms(),
                search_cache: None,
            }),
        })
    }

    pub fn push(&self, entry: Arc<Entry>) {
        {
            let mut list = self.entries.lock().unwrap();
            list.push_back(entry.clone());
            while list.len() > self.cap {
                list.pop_front();
            }
        }
        let _ = self.tx.send(entry);
    }

    pub fn clear(&self) {
        self.entries.lock().unwrap().clear();
    }

    pub fn find(&self, id: u64) -> Option<Arc<Entry>> {
        self.entries
            .lock()
            .unwrap()
            .iter()
            .find(|e| e.id == id)
            .cloned()
    }
}

pub fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// 摘要 JSON（列表 / SSE）。
pub fn summary_json(e: &Entry) -> serde_json::Value {
    let rtype = resource_type(e);
    let inner = e.inner.lock().unwrap();
    serde_json::json!({
        "id": e.id,
        "ts": e.ts,
        "kind": e.kind,
        "resourceType": rtype,
        "method": e.method,
        "url": e.url,
        "host": e.host,
        "site": e.site,
        "client": e.client,
        "clientIp": e.client_ip,
        "status": inner.resp_status,
        "contentType": inner.content_type,
        "encoding": inner.content_encoding,
        "reqSize": inner.req_body.as_ref().map(|b| b.len()).unwrap_or(0),
        "respSize": inner.resp_body.as_ref().map(|b| b.len()).unwrap_or(0),
        "bytesUp": inner.bytes_up,
        "bytesDown": inner.bytes_down,
        "wsMessages": inner.ws_messages.len(),
        "wsClosed": inner.ws_closed,
        "done": inner.done,
        "error": inner.error,
    })
}

/// 详情 JSON（右侧面板 / 单条查询）。
pub fn detail_json(e: &Entry) -> serde_json::Value {
    let rtype = resource_type(e);
    let inner = e.inner.lock().unwrap();
    let body_text = |b: &Option<Vec<u8>>| -> Option<String> {
        b.as_ref().map(|b| String::from_utf8_lossy(b).to_string())
    };
    serde_json::json!({
        "id": e.id,
        "ts": e.ts,
        "kind": e.kind,
        "resourceType": rtype,
        "method": e.method,
        "url": e.url,
        "host": e.host,
        "site": e.site,
        "client": e.client,
        "reqHeaders": e.req_headers,
        "reqBody": body_text(&inner.req_body),
        "reqBodyRaw": raw_json(&inner.req_body),
        "reqTruncated": inner.req_truncated,
        "status": inner.resp_status,
        "respHeaders": inner.resp_headers,
        "respBody": body_text(&inner.resp_body),
        "respBodyRaw": raw_json(&inner.resp_body),
        "respDecoded": body_text(&inner.resp_decoded),
        "respDecodedRaw": raw_json(&inner.resp_decoded),
        "decoded": inner.content_encoding,
        "contentType": inner.content_type,
        "respTruncated": inner.resp_truncated,
        "wsMessages": inner.ws_messages,
        "wsClosed": inner.ws_closed,
        "tcpHex": inner.tcp_hex,
        "bytesUp": inner.bytes_up,
        "bytesDown": inner.bytes_down,
        "done": inner.done,
        "error": inner.error,
        "durationMs": (inner.started_at.saturating_sub(e.ts)),
    })
}

/// 按条件过滤条目（ newest first ）。
/// 除关键词外均为多值：同一字段内为「或」，字段之间为「且」。
pub struct Filters {
    pub q: Option<String>,
    pub hosts: Vec<String>,
    pub sites: Vec<String>,
    pub apps: Vec<String>,
    pub kinds: Vec<String>,
    pub methods: Vec<String>,
    pub statuses: Vec<String>,
    pub rtypes: Vec<String>,
    pub limit: usize,
    pub offset: usize,
}

impl Filters {
    pub fn from_query(query: Option<&str>) -> Self {
        let get = |key: &str| -> Option<String> {
            query.and_then(|q| {
                q.split('&').find_map(|kv| {
                    let mut it = kv.splitn(2, '=');
                    let k = it.next()?;
                    if k == key {
                        Some(percent_encoding::percent_decode_str(it.next().unwrap_or(""))
                            .decode_utf8_lossy()
                            .replace('+', " "))
                    } else {
                        None
                    }
                })
            })
        };
        // 多值：逗号分隔（前端 URLSearchParams 会把逗号编码为 %2C，此处已解码）
        let multi = |key: &str| -> Vec<String> {
            get(key)
                .map(|v| {
                    v.split(',')
                        .map(|s| s.trim().to_lowercase())
                        .filter(|s| !s.is_empty())
                        .collect()
                })
                .unwrap_or_default()
        };
        Self {
            q: get("q").filter(|s| !s.is_empty()),
            hosts: multi("host"),
            sites: multi("site"),
            apps: multi("app"),
            kinds: multi("kind"),
            methods: multi("method"),
            statuses: multi("status"),
            rtypes: multi("type"),
            limit: get("limit").and_then(|v| v.parse().ok()).unwrap_or(500),
            offset: get("offset").and_then(|v| v.parse().ok()).unwrap_or(0),
        }
    }

    /// 带条件判断：`is_empty` 未命中时让调用方决定放行。
    fn sub_match(list: &[String], value: &str) -> Option<bool> {
        if list.is_empty() {
            None
        } else {
            Some(list.iter().any(|v| v == value))
        }
    }

    pub fn matches(&self, e: &Entry) -> bool {
        if let Some(ok) = Self::sub_match(&self.kinds, e.kind) {
            if !ok {
                return false;
            }
        }
        if let Some(ok) = Self::sub_match(&self.rtypes, resource_type(e)) {
            if !ok {
                return false;
            }
        }
        if let Some(ok) = Self::sub_match(&self.hosts, &e.host.to_lowercase()) {
            if !ok {
                return false;
            }
        }
        if let Some(ok) = Self::sub_match(&self.sites, &e.site) {
            if !ok {
                return false;
            }
        }
        if !self.apps.is_empty() {
            let cur = e.client.as_deref().unwrap_or("").to_lowercase();
            if !self.apps.iter().any(|a| *a == cur) {
                return false;
            }
        }
        if let Some(ok) = Self::sub_match(&self.methods, &e.method.to_lowercase()) {
            if !ok {
                return false;
            }
        }
        if !self.statuses.is_empty() {
            let cur = e.inner.lock().unwrap().resp_status;
            let ok = cur
                .map(|s| {
                    self.statuses.iter().any(|st| match st.as_str() {
                        "2" | "3" | "4" | "5" => s.to_string().starts_with(st.as_str()),
                        exact => s.to_string() == *exact,
                    })
                })
                .unwrap_or(false);
            if !ok {
                return false;
            }
        }
        if !self.search_hit(e) {
            return false;
        }
        true
    }

    /// 关键词是否命中：先看 URL 系列字段（廉价），未命中再扫请求/响应内容（带缓存）。
    fn search_hit(&self, e: &Entry) -> bool {
        let q = match &self.q {
            Some(q) => q.to_lowercase(),
            None => return true,
        };
        if url_hay(e).contains(&q) {
            return true;
        }
        let mut inner = e.inner.lock().unwrap();
        let sig = inner.search_sig();
        let stale = !matches!(inner.search_cache.as_ref(), Some((s, _)) if *s == sig);
        if stale {
            let text = build_content_text(e, &inner);
            inner.search_cache = Some((sig, Arc::new(text)));
        }
        inner
            .search_cache
            .as_ref()
            .map(|(_, t)| t.contains(&q))
            .unwrap_or(false)
    }

    /// 关键词是否「只在请求/响应内容里」命中（URL 未命中）。
    /// 供界面标注「内容匹配」，让用户知道这条为什么被搜出来。
    pub fn q_matched_in_content(&self, e: &Entry) -> bool {
        match &self.q {
            None => false,
            Some(q) => !url_hay(e).contains(&q.to_lowercase()) && self.search_hit(e),
        }
    }
}

/// URL 系列字段的检索串（小写）。
fn url_hay(e: &Entry) -> String {
    format!(
        "{} {} {} {} {}",
        e.url,
        e.host,
        e.site,
        e.client.as_deref().unwrap_or(""),
        e.method,
    )
    .to_lowercase()
}

/// 请求/响应内容的检索串（小写）：请求头 + 请求体 + 响应头 + 响应体（优先解压后）+ WS 消息文本。
/// 非 UTF-8（图片、字体等二进制）正文自动跳过，避免把噪声塞进索引。
fn build_content_text(e: &Entry, inner: &EntryInner) -> String {
    let mut s = String::new();

    let mut push_headers = |hs: &[(String, String)]| {
        for (k, v) in hs {
            s.push_str(k);
            s.push_str(": ");
            s.push_str(v);
            s.push('\n');
        }
    };
    push_headers(&e.req_headers);
    if let Some(hs) = inner.resp_headers.as_deref() {
        push_headers(hs);
    }
    if let Some(st) = inner.resp_status {
        s.push_str(&st.to_string());
        s.push('\n');
    }
    if let Some(err) = &inner.error {
        s.push_str(err);
        s.push('\n');
    }

    push_body(&mut s, inner.req_body.as_deref());
    // 有解压结果时用解压内容（压缩态是二进制，搜不到有意义的东西）
    push_body(
        &mut s,
        inner
            .resp_decoded
            .as_deref()
            .or(inner.resp_body.as_deref()),
    );

    for m in &inner.ws_messages {
        if let Some(d) = &m.data {
            s.push_str(d);
            s.push('\n');
        }
    }
    s.to_lowercase()
}

/// 把正文按上限追加进检索串；仅接受 UTF-8 文本（截断/二进制自动降级）。
fn push_body(out: &mut String, body: Option<&[u8]>) {
    let b = match body {
        Some(b) if !b.is_empty() => b,
        _ => return,
    };
    let slice = &b[..b.len().min(SEARCH_BODY_CAP)];
    match std::str::from_utf8(slice) {
        Ok(t) => {
            out.push_str(t);
            out.push('\n');
        }
        Err(err) => {
            // 截断可能切在多字节字符中间：取合法前缀即可（前缀太短说明本身是二进制，直接放弃）
            let n = err.valid_up_to();
            if n >= 64 {
                if let Ok(t) = std::str::from_utf8(&slice[..n]) {
                    out.push_str(t);
                    out.push('\n');
                }
            }
        }
    }
}

pub fn filtered_entries(store: &Store, filters: &Filters) -> (Vec<Arc<Entry>>, usize) {
    let list = store.entries.lock().unwrap();
    let all: Vec<Arc<Entry>> = list.iter().rev().filter(|e| filters.matches(e)).cloned().collect();
    let total = all.len();
    let page: Vec<Arc<Entry>> = all
        .into_iter()
        .skip(filters.offset)
        .take(filters.limit)
        .collect();
    (page, total)
}

/// 归类资源类型（用于「文件类型」过滤与列表标签）。
/// 优先看响应 Content-Type，缺失（进行中/失败/无响应头）时按 URL 扩展名兜底。
pub fn resource_type(e: &Entry) -> &'static str {
    match e.kind {
        "ws" => return "ws",
        "tcp" => return "tcp",
        _ => {}
    }
    let ct = {
        let inner = e.inner.lock().unwrap();
        inner
            .content_type
            .clone()
            .or_else(|| {
                inner.resp_headers.as_ref().and_then(|hs| {
                    hs.iter()
                        .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
                        .map(|(_, v)| v.clone())
                })
            })
            .unwrap_or_default()
    };
    classify_content_type(&ct, &e.url)
}

fn classify_content_type(content_type: &str, url: &str) -> &'static str {
    let ct = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_lowercase();
    if !ct.is_empty() {
        if ct.contains("json") {
            return "json";
        }
        if ct.contains("xhtml") || ct.contains("html") {
            return "document";
        }
        if ct.contains("xml") {
            return "xml";
        }
        if ct.contains("javascript") || ct.contains("ecmascript") {
            return "script";
        }
        if ct == "text/css" {
            return "css";
        }
        if ct.starts_with("image/") {
            return "image";
        }
        if ct.starts_with("font/") || ct.contains("font") {
            return "font";
        }
        if ct.starts_with("audio/") || ct.starts_with("video/") {
            return "media";
        }
        if ct.contains("wasm") {
            return "wasm";
        }
        if ct.contains("x-www-form-urlencoded") || ct.contains("multipart/form-data") {
            return "form";
        }
        if ct.starts_with("text/") {
            return "text";
        }
    }
    classify_ext(url)
}

/// 按 URL 扩展名兜底分类。
fn classify_ext(url: &str) -> &'static str {
    let path = url.split(['?', '#']).next().unwrap_or("");
    let last = path.rsplit('/').next().unwrap_or("");
    let ext = match last.rsplit_once('.') {
        Some((_, e)) => e.to_lowercase(),
        None => return "other",
    };
    match ext.as_str() {
        "html" | "htm" | "xhtml" => "document",
        "js" | "mjs" | "cjs" => "script",
        "css" => "css",
        "json" | "map" => "json",
        "xml" => "xml",
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" | "ico" | "bmp" | "avif" | "heic" => "image",
        "woff" | "woff2" | "ttf" | "otf" | "eot" => "font",
        "mp3" | "mp4" | "webm" | "m4a" | "m4v" | "ogg" | "wav" | "mov" | "m3u8" | "mkv" => "media",
        "wasm" => "wasm",
        _ => "other",
    }
}

/// 读取请求体（带截断上限）。
pub async fn read_body_capped(body: Body, cap: usize) -> (Vec<u8>, bool) {
    let mut body = body;
    let mut out = Vec::new();
    let mut trunc = false;
    while let Some(chunk) = body.data().await {
        match chunk {
            Ok(b) => {
                if out.len() + b.len() <= cap {
                    out.extend_from_slice(&b);
                } else {
                    let take = cap.saturating_sub(out.len());
                    out.extend_from_slice(&b[..take]);
                    trunc = true;
                }
            }
            Err(_) => break,
        }
    }
    (out, trunc)
}

const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-connection",
    "transfer-encoding",
    "content-length",
    "te",
    "upgrade",
    "proxy-authorization",
    "proxy-authenticate",
];

/// HTTP 明文 / MITM 请求的统一处理：捕获 -> 转发 -> 流式回传 + 响应捕获。
pub async fn capture_and_forward(
    req: Request<Body>,
    app: Arc<App>,
    inner_authority: Option<String>,
    client: ClientInfo,
) -> Response<Body> {
    let (parts, body) = req.into_parts();

    let scheme = if inner_authority.is_some() {
        "https".to_string()
    } else {
        parts
            .uri
            .scheme_str()
            .unwrap_or("http")
            .to_string()
    };
    let authority = inner_authority
        .or_else(|| parts.uri.authority().map(|a| a.to_string()))
        .or_else(|| {
            parts
                .headers
                .get(header::HOST)
                .and_then(|h| h.to_str().ok())
                .map(|s| s.to_string())
        })
        .unwrap_or_default();
    let path = parts
        .uri
        .path_and_query()
        .map(|p| p.to_string())
        .unwrap_or_else(|| "/".to_string());
    let url = format!("{}://{}{}", scheme, authority, path);
    let host = authority.split(':').next().unwrap_or("").to_string();

    let (req_body, req_trunc) = read_body_capped(body, BODY_CAP).await;
    let entry = app.store.new_entry(
        "http",
        parts.method.as_str(),
        &url,
        &host,
        util::header_pairs(&parts.headers),
        client,
    );
    {
        let mut inner = entry.inner.lock().unwrap();
        inner.req_body = Some(req_body.clone());
        inner.req_truncated = req_trunc;
    }

    // 构造转发请求
    let mut builder = Request::builder()
        .method(parts.method.clone())
        .uri(&url);
    for (k, v) in util::header_pairs(&parts.headers) {
        if HOP_BY_HOP.contains(&k.to_lowercase().as_str()) {
            continue;
        }
        if let (Ok(kk), Ok(vv)) = (
            hyper::header::HeaderName::from_bytes(k.as_bytes()),
            hyper::header::HeaderValue::from_str(&v),
        ) {
            builder = builder.header(kk, vv);
        }
    }
    let fwd = match builder.body(Body::from(req_body)) {
        Ok(r) => r,
        Err(e) => return error_response(entry, format!("构造转发请求失败: {}", e)),
    };

    let resp = match app.client.request(fwd).await {
        Ok(r) => r,
        Err(e) => {
            {
                let mut inner = entry.inner.lock().unwrap();
                inner.error = Some(e.to_string());
                inner.done = true;
            }
            app.store.push(entry.clone());
            return Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(Body::from(format!(
                    "MiniProxy: 上游请求失败: {}\n",
                    e
                )))
                .unwrap();
        }
    };

    let (rp, rb) = resp.into_parts();
    let status = rp.status.as_u16();
    let content_encoding = rp
        .headers
        .get(header::CONTENT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let content_type = rp
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    {
        let mut inner = entry.inner.lock().unwrap();
        inner.resp_status = Some(status);
        inner.resp_headers = Some(util::header_pairs(&rp.headers));
        inner.content_encoding = content_encoding.clone();
        inner.content_type = content_type.clone();
    }

    // 流式回传给客户端，同时累积响应体用于展示
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(64);
    let e2 = entry.clone();
    let store2 = app.store.clone();
    tokio::spawn(async move {
        let mut body = rb;
        let mut acc: Vec<u8> = Vec::new();
        let mut trunc = false;
        while let Some(chunk) = body.data().await {
            match chunk {
                Ok(b) => {
                    if acc.len() < BODY_CAP {
                        let room = BODY_CAP - acc.len();
                        let take = b.len().min(room);
                        acc.extend_from_slice(&b[..take]);
                        if take < b.len() {
                            trunc = true;
                        }
                    } else {
                        trunc = true;
                    }
                    if tx.send(Ok(b)).await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        let decoded = if let Some(enc) = &content_encoding {
            decode_body(&acc, enc).await
        } else {
            None
        };
        {
            let mut inner = e2.inner.lock().unwrap();
            inner.resp_body = Some(acc);
            inner.resp_decoded = decoded;
            inner.resp_truncated = trunc;
            inner.done = true;
        }
        store2.push(e2);
    });

    Response::from_parts(
        rp,
        Body::wrap_stream(tokio_stream::wrappers::ReceiverStream::new(rx)),
    )
}

fn error_response(entry: Arc<Entry>, msg: String) -> Response<Body> {
    {
        let mut inner = entry.inner.lock().unwrap();
        inner.error = Some(msg.clone());
        inner.done = true;
    }
    Response::builder()
        .status(StatusCode::BAD_GATEWAY)
        .body(Body::from(format!("MiniProxy: {}\n", msg)))
        .unwrap()
}

/// 解压响应体：gzip / deflate(zlib+raw) / br / zstd。
pub async fn decode_body(data: &[u8], enc: &str) -> Option<Vec<u8>> {
    use async_compression::tokio::bufread::*;
    use tokio::io::AsyncReadExt;

    match enc.to_ascii_lowercase().as_str() {
        "gzip" => {
            let mut d = GzipDecoder::new(std::io::Cursor::new(data));
            let mut out = Vec::new();
            d.read_to_end(&mut out).await.ok().map(|_| out)
        }
        "br" => {
            let mut d = BrotliDecoder::new(std::io::Cursor::new(data));
            let mut out = Vec::new();
            d.read_to_end(&mut out).await.ok().map(|_| out)
        }
        "zstd" => {
            let mut d = ZstdDecoder::new(std::io::Cursor::new(data));
            let mut out = Vec::new();
            d.read_to_end(&mut out).await.ok().map(|_| out)
        }
        "deflate" | "zlib" => {
            // 优先 zlib 包装，失败则按 raw deflate 重试
            let mut d = ZlibDecoder::new(std::io::Cursor::new(data));
            let mut out = Vec::new();
            if d.read_to_end(&mut out).await.is_ok() {
                return Some(out);
            }
            let mut d = DeflateDecoder::new(std::io::Cursor::new(data));
            let mut out = Vec::new();
            d.read_to_end(&mut out).await.ok().map(|_| out)
        }
        _ => None,
    }
}

pub fn header_map_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

pub fn _unused(_h: &HeaderMap) {}
