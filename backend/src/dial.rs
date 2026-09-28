//! 出站拨号统一入口：直连 或 经上游代理（HTTP CONNECT 级联）。
//!
//! 配置 `MINIPROXY_UPSTREAM_PROXY=http://127.0.0.1:7890` 后，
//! MiniProxy 到源站的所有出站连接（HTTP/HTTPS/WS/TCP）都会先经上游代理，
//! 用于「抓包 + 走代理访问被墙站点」组合场景。

use std::io;
use std::pin::Pin;
use std::sync::{Arc, OnceLock, RwLock};
use std::task::{Context, Poll};
use std::time::Duration;

use hyper::Uri;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

/// 出站 TCP 连接（直连目标 或 连上游代理）的超时。
const TCP_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// 「上游 CONNECT 响应」与「出站 TLS 握手」的超时。
///
/// 这两个阶段以前是裸 await：上游节点丢包时（既不发响应也不发 RST）代理会无限期等待，
/// 客户端只能干等到自己超时——表现为「开了代理后整页卡住转圈」。默认 10s，
/// 可用 `MINIPROXY_DIAL_TIMEOUT_MS` 调整。
fn dial_timeout() -> Duration {
    static T: OnceLock<Duration> = OnceLock::new();
    *T.get_or_init(|| {
        std::env::var("MINIPROXY_DIAL_TIMEOUT_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|v| *v > 0)
            .map(Duration::from_millis)
            .unwrap_or(Duration::from_secs(10))
    })
}

fn timed_out(what: String) -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        format!(
            "{} 超时（{}ms，可用 MINIPROXY_DIAL_TIMEOUT_MS 调整）",
            what,
            dial_timeout().as_millis()
        ),
    )
}

/// 运行期可变的上游配置：界面里随时开关/换地址，无需重启进程。
pub type SharedUpstream = Arc<RwLock<Option<Upstream>>>;

/// 上游代理地址（暂支持 http://host:port，无认证）。
#[derive(Clone, Debug)]
pub struct Upstream {
    pub host: String,
    pub port: u16,
}

impl Upstream {
    pub fn from_env() -> Option<Upstream> {
        let v = std::env::var("MINIPROXY_UPSTREAM_PROXY").ok()?;
        Upstream::parse(&v)
    }

    /// 解析 `127.0.0.1:7890` / `http://127.0.0.1:7890`（暂不支持 socks，返回 None）。
    pub fn parse(v: &str) -> Option<Upstream> {
        let v = v.trim();
        if v.is_empty() || v.starts_with("socks") {
            return None;
        }
        let s = v
            .strip_prefix("http://")
            .or_else(|| v.strip_prefix("https://"))
            .unwrap_or(v);
        let s = s.trim_end_matches('/');
        let (host, port) = crate::util::split_authority(s, 80);
        if host.is_empty() {
            return None;
        }
        Some(Upstream { host, port })
    }

    pub fn addr(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    /// 连通性测试：TCP 握手 + 一次 CONNECT 探针，确认对方确实是 HTTP 代理。
    /// 用于「保存前先验证」，避免填错地址后所有请求失败却不知道原因。
    pub async fn probe(&self) -> Result<(), String> {
        let addr = self.addr();
        let connect = TcpStream::connect((self.host.as_str(), self.port));
        let mut tcp = match tokio::time::timeout(Duration::from_millis(800), connect).await {
            Ok(Ok(t)) => t,
            Ok(Err(e)) => return Err(format!("无法连接 {}（{}）", addr, e)),
            Err(_) => return Err(format!("连接 {} 超时（800ms）", addr)),
        };
        let req = format!(
            "CONNECT www.gstatic.com:443 HTTP/1.1\r\nHost: www.gstatic.com:443\r\n\r\n"
        );
        if let Err(e) = tcp.write_all(req.as_bytes()).await {
            return Err(format!("向 {} 发送 CONNECT 失败（{}）", addr, e));
        }
        let read = read_connect_response(&mut tcp);
        match tokio::time::timeout(Duration::from_millis(2500), read).await {
            Ok(Ok(code)) if (200..300).contains(&code) => Ok(()),
            Ok(Ok(code)) => Err(format!("{} 不是可用的 HTTP 代理（CONNECT 返回 {}）", addr, code)),
            Ok(Err(e)) => Err(format!("读取 {} 响应失败（{}）", addr, e)),
            Err(_) => Err(format!("等待 {} 的 CONNECT 响应超时", addr)),
        }
    }
}

/// 本机常见代理软件的 HTTP 代理端口（按常见程度排序），用于「自动检测」。
const CANDIDATE_PORTS: [u16; 14] = [
    7890, 7897, 7891, 9090, 8888, 10809, 6152, 1087, 2080, 2081, 3128, 8889, 8080, 20171,
];

/// 自动探测本机是否已运行可用的 HTTP 代理（Clash/Charles/Surge/v2ray…）。
/// 并发探测全部候选端口，返回清单中最靠前的可用项；skip_ports 用于排除本实例端口避免自连成环。
pub async fn detect_local_upstream(skip_ports: &[u16]) -> Vec<Upstream> {
    let mut tasks = Vec::new();
    for port in CANDIDATE_PORTS {
        if skip_ports.contains(&port) {
            continue;
        }
        let up = Upstream {
            host: "127.0.0.1".to_string(),
            port,
        };
        tasks.push(tokio::spawn(async move {
            match up.probe().await {
                Ok(()) => Some(up),
                Err(_) => None,
            }
        }));
    }
    let mut found: Vec<Upstream> = Vec::new();
    for t in tasks {
        if let Ok(Some(up)) = t.await {
            found.push(up);
        }
    }
    // 按候选清单顺序排列（并发完成顺序不稳定）
    found.sort_by_key(|u| CANDIDATE_PORTS.iter().position(|p| *p == u.port).unwrap_or(usize::MAX));
    found
}

/// 到目标 host:port 的 TCP 连接：有上游则 CONNECT 隧道，否则直连。
pub async fn tcp_dial(
    upstream: Option<&Upstream>,
    host: &str,
    port: u16,
) -> io::Result<TcpStream> {
    // 回环目标永远直连：本机服务经外部代理转发没有意义，还会被上游拒绝（表现为 502）
    if host == "localhost" || host == "::1" || host.starts_with("127.") {
        return connect_timed(host, port).await;
    }
    match upstream {
        None => connect_timed(host, port).await,
        Some(up) => {
            let mut tcp = connect_timed(&up.host, up.port).await?;
            let req = format!(
                "CONNECT {h}:{p} HTTP/1.1\r\nHost: {h}:{p}\r\nProxy-Connection: keep-alive\r\n\r\n",
                h = host,
                p = port
            );
            tcp.write_all(req.as_bytes()).await?;
            let code = match tokio::time::timeout(dial_timeout(), read_connect_response(&mut tcp)).await
            {
                Ok(r) => r?,
                Err(_) => {
                    return Err(timed_out(format!(
                        "等待上游代理 {} 对 {}:{} 的 CONNECT 响应",
                        up.addr(),
                        host,
                        port
                    )))
                }
            };
            if code != 200 {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionRefused,
                    format!("上游代理拒绝 CONNECT {}:{}（HTTP {}）", host, port, code),
                ));
            }
            Ok(tcp)
        }
    }
}

/// TCP 连接 + 超时（超时后 `TcpStream::connect` 的 future 被丢弃，连接不会建立）。
async fn connect_timed(host: &str, port: u16) -> io::Result<TcpStream> {
    match tokio::time::timeout(TCP_CONNECT_TIMEOUT, TcpStream::connect((host, port))).await {
        Ok(r) => r,
        Err(_) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!(
                "连接 {}:{} 超时（{}ms）",
                host,
                port,
                TCP_CONNECT_TIMEOUT.as_millis()
            ),
        )),
    }
}

/// 读取 CONNECT 响应头并解析状态码。
async fn read_connect_response(tcp: &mut TcpStream) -> io::Result<u16> {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let n = tcp.read(&mut byte).await?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "上游代理提前关闭连接",
            ));
        }
        buf.push(byte[0]);
        if buf.ends_with(b"\r\n\r\n") || buf.ends_with(b"\n\n") {
            break;
        }
        if buf.len() > 16 * 1024 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "上游响应头过大"));
        }
    }
    let head = String::from_utf8_lossy(&buf);
    Ok(head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse::<u16>().ok())
        .unwrap_or(0))
}

/// 系统根证书库：进程内只加载一次。
///
/// 以前每次拨号都调 `load_native_certs()`（macOS 上要走钥匙串）+ 重建 `RootCertStore`
/// （逐张校验上百张系统证书），而每个新出站连接都会走一次。一个页面几十个域名就是
/// 几十次重复劳动，白白吃掉几十毫秒 × N。
fn roots() -> Arc<rustls::RootCertStore> {
    static R: OnceLock<Arc<rustls::RootCertStore>> = OnceLock::new();
    R.get_or_init(|| {
        let mut store = rustls::RootCertStore::empty();
        match rustls_native_certs::load_native_certs() {
            Ok(certs) => {
                let mut n = 0usize;
                for c in certs {
                    if store.add(&rustls::Certificate(c.0)).is_ok() {
                        n += 1;
                    }
                }
                eprintln!("  出站 TLS 根证书: 已加载并缓存 {} 张系统证书", n);
            }
            Err(e) => eprintln!("  出站 TLS 根证书: 加载失败（{}），HTTPS 源站校验将失败", e),
        }
        Arc::new(store)
    })
    .clone()
}

fn build_client_config(alpn: Vec<Vec<u8>>) -> rustls::ClientConfig {
    let mut cfg = rustls::ClientConfig::builder()
        .with_safe_defaults()
        .with_root_certificates((*roots()).clone())
        .with_no_client_auth();
    cfg.alpn_protocols = alpn;
    cfg
}

/// 供 hyper 客户端使用：ALPN 协商 h2，让出站也能享受 HTTP/2 多路复用。
pub fn client_config_h2_ok() -> Arc<rustls::ClientConfig> {
    static C: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    C.get_or_init(|| {
        Arc::new(build_client_config(vec![
            b"h2".to_vec(),
            b"http/1.1".to_vec(),
        ]))
    })
    .clone()
}

/// 供「自己手写 HTTP/1.1 报文」的场景（WebSocket 升级）使用：ALPN 只允许 http/1.1。
/// 若这里协商出 h2，源站会按 h2 帧来期待数据，手写的升级请求根本发不出去。
fn client_config_http1_only() -> Arc<rustls::ClientConfig> {
    static C: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    C.get_or_init(|| Arc::new(build_client_config(vec![b"http/1.1".to_vec()])))
        .clone()
}

/// 在已建立的 TCP 连接上做 TLS 客户端握手（系统根证书校验），ALPN 只给 http/1.1。
pub async fn tls_wrap(
    host: &str,
    tcp: TcpStream,
) -> io::Result<tokio_rustls::client::TlsStream<TcpStream>> {
    tls_wrap_alpn(host, tcp, false).await
}

/// 同上，`h2_ok = true` 时 ALPN 额外提供 h2（供 hyper 客户端用）。
pub async fn tls_wrap_alpn(
    host: &str,
    tcp: TcpStream,
    h2_ok: bool,
) -> io::Result<tokio_rustls::client::TlsStream<TcpStream>> {
    let cfg = if h2_ok {
        client_config_h2_ok()
    } else {
        client_config_http1_only()
    };
    let connector = tokio_rustls::TlsConnector::from(cfg);
    let name = rustls::ServerName::try_from(host.to_string().as_str())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("无效主机名: {}", e)))?;
    match tokio::time::timeout(dial_timeout(), connector.connect(name, tcp)).await {
        Ok(r) => r,
        Err(_) => Err(timed_out(format!("与 {} 完成 TLS 握手", host))),
    }
}

/// hyper 客户端连接器返回的流：明文 TCP 或 TLS。
pub enum OriginStream {
    Plain(TcpStream),
    Tls(tokio_rustls::client::TlsStream<TcpStream>),
}

impl AsyncRead for OriginStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            OriginStream::Plain(s) => Pin::new(s).poll_read(cx, buf),
            OriginStream::Tls(s) => Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for OriginStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            OriginStream::Plain(s) => Pin::new(s).poll_write(cx, buf),
            OriginStream::Tls(s) => Pin::new(s).poll_write(cx, buf),
        }
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            OriginStream::Plain(s) => Pin::new(s).poll_flush(cx),
            OriginStream::Tls(s) => Pin::new(s).poll_flush(cx),
        }
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            OriginStream::Plain(s) => Pin::new(s).poll_shutdown(cx),
            OriginStream::Tls(s) => Pin::new(s).poll_shutdown(cx),
        }
    }
}

/// hyper 客户端连接器：按 URI scheme 拨号（可选上游级联 + TLS）。
#[derive(Clone)]
pub struct ProxyConnector {
    pub upstream: SharedUpstream,
    /// 分流规则：命中「跳过代理」的目标直连源站（不进上游）
    pub rules: crate::rules::SharedRules,
}

impl OriginStream {
    /// 本连接是否通过 ALPN 协商到了 h2。
    pub fn is_h2(&self) -> bool {
        match self {
            OriginStream::Plain(_) => false,
            OriginStream::Tls(s) => s.get_ref().1.alpn_protocol() == Some(b"h2"),
        }
    }
}

impl hyper::client::connect::Connection for OriginStream {
    fn connected(&self) -> hyper::client::connect::Connected {
        // 告知 hyper 这条连接协商到了 h2：hyper 客户端据此改用 HTTP/2 协议栈
        // （见 hyper client.rs 里 `connected.alpn == Alpn::H2` 的分支），
        // 否则同一个 TLS 会话上会按 HTTP/1.1 发请求，源站直接判为协议错误。
        let c = hyper::client::connect::Connected::new();
        if self.is_h2() {
            c.negotiated_h2()
        } else {
            c
        }
    }
}

impl hyper::service::Service<Uri> for ProxyConnector {
    type Response = OriginStream;
    type Error = io::Error;
    type Future = Pin<Box<dyn std::future::Future<Output = io::Result<OriginStream>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, uri: Uri) -> Self::Future {
        // 每次拨号都读取当前配置：界面里切换上游后，新连接立即生效（已建立的连接不受影响）
        let upstream = self.upstream.read().ok().and_then(|g| g.clone());
        // 分流规则同样每次读取：命中「跳过代理」的域名/IP 直连源站
        let rules = self.rules.read().map(|g| g.clone()).unwrap_or_default();
        let host = uri.host().unwrap_or("").to_string();
        let upstream = rules.upstream_for(&host, upstream);
        Box::pin(async move { dial_for_uri(upstream.as_ref(), &uri).await })
    }
}

pub async fn dial_for_uri(
    upstream: Option<&Upstream>,
    uri: &Uri,
) -> io::Result<OriginStream> {
    let host = uri
        .host()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "URI 缺少主机"))?
        .to_string();
    let https = uri.scheme_str() == Some("https");
    let port = uri.port_u16().unwrap_or(if https { 443 } else { 80 });
    let tcp = tcp_dial(upstream, &host, port).await?;
    if https {
        // 出站也协商 h2：Google 等站点全站 HTTP/2，降级到 H1 会失去多路复用
        Ok(OriginStream::Tls(tls_wrap_alpn(&host, tcp, true).await?))
    } else {
        Ok(OriginStream::Plain(tcp))
    }
}
