//! MiniProxy — MITM 抓包代理（Rust 后端）
//!
//! 模块划分：
//! - ca      : 本地 CA 生成/持久化 + 按域名动态签发叶子证书
//! - capture : 抓包记录模型 / 存储 / SSE 广播 / 响应体解码
//! - proxy   : 代理端口（HTTP 明文 + CONNECT 隧道 + TLS MITM + WebSocket）
//! - ws      : WebSocket 帧解析器
//! - tcp     : 纯 TCP 隧道记录
//! - peek    : 支持回吐首字节的流包装（用于协议探测）
//! - api     : REST/SSE/导出/静态文件服务
//! - sysproxy: 一键设置/恢复系统代理

mod api;
mod attrib;
mod ca;
mod capture;
mod dial;
mod peek;
mod proxy;
mod sysproxy;
mod tcp;
mod util;
mod ws;

use std::sync::Arc;

use capture::Store;
use hyper::server::conn::AddrStream;
use hyper::Client;

pub type HttpClient = Client<dial::ProxyConnector>;

pub struct App {
    pub store: Arc<Store>,
    pub ca: Arc<ca::Ca>,
    pub client: HttpClient,
    pub proxy_port: u16,
    pub upstream: Option<dial::Upstream>,
}

fn env_or(name: &str, default: u16) -> u16 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[tokio::main]
async fn main() {
    let proxy_port = env_or("MINIPROXY_PORT", 34567);
    let api_port = env_or("MINIPROXY_API_PORT", 9000);
    let upstream = dial::Upstream::from_env();

    let store = Arc::new(Store::new(5000));
    let ca = Arc::new(ca::Ca::load_or_create().expect("初始化本地 CA 失败"));
    let client = build_http_client(upstream.clone());

    println!("==============================================");
    println!("  MiniProxy 抓包代理");
    println!("==============================================");
    println!("  代理端口      : 0.0.0.0:{}  (HTTP/HTTPS/WS)", proxy_port);
    println!("  界面 & API    : http://127.0.0.1:{}", api_port);
    println!("  CA 证书       : {}", ca.cert_path.display());
    println!("                  (浏览器/系统需信任该证书才能解密 HTTPS)");
    match &upstream {
        Some(up) => println!("  上游级联      : {}:{}  (出站流量经此代理转发)", up.host, up.port),
        None => println!("  上游级联      : 未启用 (设 MINIPROXY_UPSTREAM_PROXY=http://host:port 开启)"),
    }
    println!("==============================================");

    let app = Arc::new(App {
        store,
        ca,
        client,
        proxy_port,
        upstream,
    });

    // API + 静态界面服务
    {
        let app = app.clone();
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], api_port));
        let make_svc = hyper::service::make_service_fn(move |_conn| {
            let app = app.clone();
            async move {
                Ok::<_, std::convert::Infallible>(hyper::service::service_fn(move |req| {
                    api::handle_api(req, app.clone())
                }))
            }
        });
        tokio::spawn(async move {
            if let Err(e) = hyper::Server::bind(&addr).serve(make_svc).await {
                eprintln!("API 服务启动失败: {}", e);
                std::process::exit(1);
            }
        });
    }

    // 代理服务
    let app2 = app.clone();
    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], proxy_port));
    let make_svc = hyper::service::make_service_fn(move |conn: &AddrStream| {
        let app = app2.clone();
        // 客户端本地端口 -> 反查所属进程，用于「按应用分组/筛选」
        let peer_port = conn.remote_addr().port();
        async move {
            let client = tokio::task::spawn_blocking(move || attrib::process_for_port(peer_port))
                .await
                .ok()
                .flatten();
            Ok::<_, std::convert::Infallible>(hyper::service::service_fn(move |req| {
                proxy::handle_proxy(req, app.clone(), client.clone())
            }))
        }
    });

    if let Err(e) = hyper::Server::bind(&addr).serve(make_svc).await {
        eprintln!("代理服务启动失败: {}", e);
        std::process::exit(1);
    }
}

fn build_http_client(upstream: Option<dial::Upstream>) -> HttpClient {
    let connector = dial::ProxyConnector { upstream };
    Client::builder().build(connector)
}
