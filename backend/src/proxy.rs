//! 代理端口处理：明文 HTTP、CONNECT 隧道（TLS MITM / 纯 TCP）、WebSocket 升级。

use std::sync::Arc;

use hyper::header;
use hyper::server::conn::Http;
use hyper::service::service_fn;
use hyper::{Body, Method, Request, Response};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::capture::{self, Entry, Store};
use crate::dial;
use crate::peek::Peeked;
use crate::util;
use crate::ws;
use crate::App;

/// 代理端口入口：CONNECT -> 隧道；其余 -> 明文 HTTP / WebSocket。
pub async fn handle_proxy(
    req: Request<Body>,
    app: Arc<App>,
    client: Option<String>,
) -> Result<Response<Body>, std::convert::Infallible> {
    if req.method() == Method::CONNECT {
        let authority = req.uri().to_string();
        tokio::spawn(async move {
            match hyper::upgrade::on(req).await {
                Ok(io) => handle_tunnel(io, authority, app, client).await,
                Err(_) => {}
            }
        });
        return Ok(Response::builder().status(200).body(Body::empty()).unwrap());
    }

    if util::is_upgrade_request(req.method(), req.headers()) {
        return Ok(handle_ws_upgrade(req, app, None, client).await);
    }
    Ok(capture::capture_and_forward(req, app, None, client).await)
}

/// 标记条目出错并入库。
fn finish_err(entry: &Arc<Entry>, store: &Arc<Store>, msg: String) {
    {
        let mut inner = entry.inner.lock().unwrap();
        inner.error = Some(msg);
        inner.done = true;
    }
    store.push(entry.clone());
}

/// CONNECT 隧道：探测首字节决定 TLS MITM 还是纯 TCP 记录。
async fn handle_tunnel(
    io: hyper::upgrade::Upgraded,
    authority: String,
    app: Arc<App>,
    client: Option<String>,
) {
    let mut peeked = Peeked::new(io);
    let mut first = [0u8; 1];
    let n = peeked.read(&mut first).await.unwrap_or(0);
    if n == 0 {
        return;
    }

    let (host, port) = util::split_authority(&authority, 443);

    // 关键：把探测用的首字节放回流中，否则 ClientHello 会缺失首字节
    peeked.peeked = Some(first[0]);

    if first[0] == 0x16 && mitm_allowed(&host) {
        // TLS 握手 -> MITM
        let acceptor = tokio_rustls::TlsAcceptor::from(app.ca.server_config_for(&host));
        if let Ok(tls_stream) = acceptor.accept(peeked).await {
            let svc = service_fn(move |req| {
                handle_inner(req, host.clone(), app.clone(), client.clone())
            });
            let _ = Http::new()
                .serve_connection(tls_stream, svc)
                .with_upgrades()
                .await;
            return;
        }
        // TLS 握手失败：无法恢复原始流，仅记录
        let entry = app.store.new_entry(
            "tcp",
            "TUNNEL",
            &format!("tcp://{}:{}", host, port),
            &host,
            vec![("Authority".into(), authority.clone())],
            client,
        );
        finish_err(&entry, &app.store, "TLS 握手失败（客户端拒绝 MITM 证书或提前断开）".into());
    } else {
        // 非 TLS，或 TLS 但命中 MITM 白名单：纯 TCP 隧道记录
        let note = if first[0] == 0x16 {
            "TLS 直通（MITM 白名单）"
        } else {
            "TCP"
        };
        let entry = app.store.new_entry(
            "tcp",
            "TUNNEL",
            &format!("tcp://{}:{}", host, port),
            &host,
            vec![
                ("Authority".into(), authority.clone()),
                ("Mode".into(), note.into()),
            ],
            client,
        );
        app.store.push(entry.clone());
        match dial::tcp_dial(app.upstream.as_ref(), host.as_str(), port).await {
            Ok(remote) => crate::tcp::splice_tcp(peeked, remote, entry).await,
            Err(e) => finish_err(&entry, &app.store, format!("连接目标失败: {}", e)),
        }
    }
}

/// 判断目标 host 是否允许 MITM 解密。
///
/// 默认放行（不解密）Apple/iCloud 等已知证书固定或不信任用户 CA 的系统服务域名；
/// 可用 `MINIPROXY_NO_MITM=a.com,b.org` 追加白名单后缀。
fn mitm_allowed(host: &str) -> bool {
    const DEFAULT_BYPASS: &[&str] = &[
        "icloud.com",
        "icloud.com.cn",
        "icloud-content.com",
        "apple.com",
        "mzstatic.com",
    ];
    let extra: Vec<String> = std::env::var("MINIPROXY_NO_MITM")
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    let host = host.to_lowercase();
    let bypassed = DEFAULT_BYPASS
        .iter()
        .map(|s| s.to_string())
        .chain(extra)
        .any(|suffix| host == suffix || host.ends_with(&format!(".{}", suffix)));
    !bypassed
}

/// MITM 内层 HTTP 处理：普通请求捕获转发，WebSocket 升级走专用路径。
async fn handle_inner(
    req: Request<Body>,
    host: String,
    app: Arc<App>,
    client: Option<String>,
) -> Result<Response<Body>, std::convert::Infallible> {
    if util::is_upgrade_request(req.method(), req.headers()) {
        return Ok(handle_ws_upgrade(req, app, Some(host), client).await);
    }
    Ok(capture::capture_and_forward(req, app, Some(host), client).await)
}

/// WebSocket 升级处理：
/// 1. 记录条目 -> 2. 回复 101 让 hyper 交出客户端流 ->
/// 3. 向源站转发升级请求 -> 4. 双向泵 + 帧解析记录。
pub async fn handle_ws_upgrade(
    req: Request<Body>,
    app: Arc<App>,
    inner_authority: Option<String>,
    client: Option<String>,
) -> Response<Body> {
    let method = req.method().clone();
    let uri = req.uri().clone();
    let headers = req.headers().clone();

    let (scheme, authority) = if let Some(a) = &inner_authority {
        ("https".to_string(), a.clone())
    } else {
        let scheme = uri.scheme_str().unwrap_or("http").to_string();
        let authority = uri
            .authority()
            .map(|a| a.to_string())
            .or_else(|| capture::header_map_value(&headers, "host"))
            .unwrap_or_default();
        (scheme, authority)
    };
    let path = uri
        .path_and_query()
        .map(|p| p.to_string())
        .unwrap_or_else(|| "/".to_string());
    let host = authority.split(':').next().unwrap_or("").to_string();
    let url = format!("{}://{}{}", scheme, authority, path);

    let entry = app.store.new_entry(
        "ws",
        method.as_str(),
        &url,
        &host,
        util::header_pairs(&headers),
        client,
    );

    // 序列化升级请求发往源站（必须保留 Connection/Upgrade 头，源站依赖其完成升级）
    let mut req_bytes = format!("{} {} HTTP/1.1\r\n", method, path).into_bytes();
    for (k, v) in util::header_pairs(&headers) {
        let kl = k.to_lowercase();
        if matches!(kl.as_str(), "proxy-connection" | "transfer-encoding" | "te") {
            continue;
        }
        req_bytes.extend_from_slice(format!("{}: {}\r\n", k, v).as_bytes());
    }
    if !headers.contains_key(header::HOST) {
        req_bytes.extend_from_slice(format!("Host: {}\r\n", authority).as_bytes());
    }
    req_bytes.extend_from_slice(b"\r\n");

    let tls = scheme == "https" || scheme == "wss";
    let (ohost, oport) = util::split_authority(&authority, if tls { 443 } else { 80 });

    // 在返回 101 之前先连接源站并拿到其响应头：
    // 这样客户端只会收到一个 101（携带真实的 Sec-WebSocket-Accept）。
    let upstream_result = async {
        let mut upstream = connect_origin(&ohost, oport, tls).await?;
        upstream.write_all(&req_bytes).await.map_err(|e| format!("向源站发送升级请求失败: {}", e))?;
        let head = read_http_head(&mut upstream)
            .await
            .map_err(|e| format!("读取源站升级响应失败: {}", e))?;
        Ok::<_, String>((upstream, head))
    }
    .await;

    let (mut upstream, head) = match upstream_result {
        Ok(v) => v,
        Err(e) => {
            finish_err(&entry, &app.store, e);
            return Response::builder()
                .status(502)
                .body(Body::from("MiniProxy: WebSocket 源站连接失败\n"))
                .unwrap();
        }
    };

    let status = parse_status_code(&head).unwrap_or(502);
    let head_headers = parse_head_headers(&head);
    {
        let mut inner = entry.inner.lock().unwrap();
        inner.resp_status = Some(status);
        inner.resp_headers = Some(head_headers.clone());
    }

    if status != 101 {
        finish_err(&entry, &app.store, format!("源站未接受升级（HTTP {}）", status));
        // 源站响应原样回给客户端（普通响应，不做升级）
        let mut builder = Response::builder().status(status);
        for (k, v) in &head_headers {
            if let (Ok(kk), Ok(vv)) = (
                hyper::header::HeaderName::from_bytes(k.as_bytes()),
                hyper::header::HeaderValue::from_str(v),
            ) {
                builder = builder.header(kk, vv);
            }
        }
        return builder.body(Body::empty()).unwrap();
    }

    // 101：将源站的升级响应头透传给客户端（hyper 负责写出）
    let mut builder = Response::builder().status(101);
    for (k, v) in &head_headers {
        if let (Ok(kk), Ok(vv)) = (
            hyper::header::HeaderName::from_bytes(k.as_bytes()),
            hyper::header::HeaderValue::from_str(v),
        ) {
            builder = builder.header(kk, vv);
        }
    }
    let resp = builder.body(Body::empty()).unwrap();

    app.store.push(entry.clone());
    let app2 = app.clone();
    tokio::spawn(async move {
        let client_stream = match hyper::upgrade::on(req).await {
            Ok(c) => c,
            Err(e) => {
                finish_err(&entry, &app2.store, format!("获取客户端升级流失败: {}", e));
                return;
            }
        };
        ws::splice_ws(client_stream, upstream, entry).await;
    });
    resp
}

pub trait DynStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> DynStream for T {}

/// 连接源站（可选 TLS）。
async fn connect_origin(
    host: &str,
    port: u16,
    tls: bool,
) -> Result<Box<dyn DynStream>, String> {
    let tcp = TcpStream::connect((host, port))
        .await
        .map_err(|e| format!("连接源站失败: {}", e))?;
    if !tls {
        return Ok(Box::new(tcp));
    }
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_native_certs::load_native_certs().map_err(|e| e.to_string())? {
        let _ = roots.add(&rustls::Certificate(cert.0));
    }
    let cfg = rustls::ClientConfig::builder()
        .with_safe_defaults()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(cfg));
    let name =
        rustls::ServerName::try_from(host.to_string().as_str()).map_err(|e| e.to_string())?;
    let stream = connector
        .connect(name, tcp)
        .await
        .map_err(|e| format!("TLS 连接源站失败: {}", e))?;
    Ok(Box::new(stream))
}

/// 逐字节读取直到出现空行（HTTP 头结束）。
async fn read_http_head<S: AsyncRead + Unpin>(io: &mut S) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let n = io.read(&mut byte).await?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "源站提前关闭连接",
            ));
        }
        out.push(byte[0]);
        if out.ends_with(b"\r\n\r\n") || out.ends_with(b"\n\n") {
            return Ok(out);
        }
        if out.len() > 128 * 1024 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "响应头过大",
            ));
        }
    }
}

fn parse_status_code(head: &[u8]) -> Option<u16> {
    let s = std::str::from_utf8(head).ok()?;
    let line = s.lines().next()?;
    let mut it = line.split_whitespace();
    it.next()?; // HTTP/1.1
    it.next()?.parse().ok()
}

/// 从原始响应头字节提取 (k, v) 对。
fn parse_head_headers(head: &[u8]) -> Vec<(String, String)> {
    let s = String::from_utf8_lossy(head);
    s.lines()
        .skip(1)
        .take_while(|l| !l.is_empty())
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect()
}
