//! 出站拨号统一入口：直连 或 经上游代理（HTTP CONNECT 级联）。
//!
//! 配置 `MINIPROXY_UPSTREAM_PROXY=http://127.0.0.1:7890` 后，
//! MiniProxy 到源站的所有出站连接（HTTP/HTTPS/WS/TCP）都会先经上游代理，
//! 用于「抓包 + 走代理访问被墙站点」组合场景。

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use hyper::Uri;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

/// 上游代理地址（暂支持 http://host:port，无认证）。
#[derive(Clone, Debug)]
pub struct Upstream {
    pub host: String,
    pub port: u16,
}

impl Upstream {
    pub fn from_env() -> Option<Upstream> {
        let v = std::env::var("MINIPROXY_UPSTREAM_PROXY").ok()?;
        let v = v.trim().to_string();
        if v.is_empty() {
            return None;
        }
        let s = v
            .strip_prefix("http://")
            .or_else(|| v.strip_prefix("https://"))
            .unwrap_or(&v);
        let (host, port) = crate::util::split_authority(s, 80);
        if host.is_empty() {
            return None;
        }
        Some(Upstream { host, port })
    }
}

/// 到目标 host:port 的 TCP 连接：有上游则 CONNECT 隧道，否则直连。
pub async fn tcp_dial(
    upstream: Option<&Upstream>,
    host: &str,
    port: u16,
) -> io::Result<TcpStream> {
    match upstream {
        None => TcpStream::connect((host, port)).await,
        Some(up) => {
            let mut tcp = TcpStream::connect((up.host.as_str(), up.port)).await?;
            let req = format!(
                "CONNECT {h}:{p} HTTP/1.1\r\nHost: {h}:{p}\r\nProxy-Connection: keep-alive\r\n\r\n",
                h = host,
                p = port
            );
            tcp.write_all(req.as_bytes()).await?;
            let code = read_connect_response(&mut tcp).await?;
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

/// 在已建立的 TCP 连接上做 TLS 客户端握手（系统根证书校验）。
pub async fn tls_wrap(
    host: &str,
    tcp: TcpStream,
) -> io::Result<tokio_rustls::client::TlsStream<TcpStream>> {
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_native_certs::load_native_certs()? {
        let _ = roots.add(&rustls::Certificate(cert.0));
    }
    let cfg = rustls::ClientConfig::builder()
        .with_safe_defaults()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(std::sync::Arc::new(cfg));
    let name = rustls::ServerName::try_from(host.to_string().as_str())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("无效主机名: {}", e)))?;
    connector.connect(name, tcp).await
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
    pub upstream: Option<Upstream>,
}

impl hyper::client::connect::Connection for OriginStream {
    fn connected(&self) -> hyper::client::connect::Connected {
        hyper::client::connect::Connected::new()
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
        let upstream = self.upstream.clone();
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
        Ok(OriginStream::Tls(tls_wrap(&host, tcp).await?))
    } else {
        Ok(OriginStream::Plain(tcp))
    }
}
