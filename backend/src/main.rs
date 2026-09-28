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
//! - rules   : 出站分流规则（哪些域名/IP 跳过代理、哪些强制走代理）

mod api;
mod attrib;
mod ca;
mod capture;
mod config;
mod dial;
mod peek;
mod proxy;
mod rules;
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
    pub api_port: u16,
    pub lan_ip: Option<String>,
    /// 上游级联配置，运行期可在界面里修改（无需重启）
    pub upstream: dial::SharedUpstream,
    /// 当前上游来源：env / saved / auto / manual / off
    pub upstream_source: std::sync::Mutex<String>,
    /// 各域名 TLS 握手失败计数：达到阈值（proxy::AUTO_BYPASS_THRESHOLD）后自动直通，
    /// 让证书固定（pinning）的 App 在代理下照常工作。
    pub bypass_counts: std::sync::Mutex<std::collections::HashMap<String, u32>>,
    /// 出站 TLS 握手失败：域名 -> (失败次数, 最近失败时间)。对端只支持旧式加密套件
    /// （rustls 无法协商）时，达到阈值后临时直通，避免站点整个不可用。
    pub outbound_tls_failures: std::sync::Mutex<std::collections::HashMap<String, (u32, u128)>>,
    /// 出站分流规则：哪些域名/IP 跳过代理（直连）、哪些强制走代理。界面可改、持久化。
    pub rules: rules::SharedRules,
}

impl App {
    /// 记录某域名一次 TLS 握手失败；达到阈值返回 true（此后该域名自动直通）。
    pub fn record_tls_failure(&self, host: &str) -> bool {
        let mut m = self.bypass_counts.lock().unwrap();
        let c = m.entry(host.to_lowercase()).or_insert(0);
        *c += 1;
        *c >= proxy::AUTO_BYPASS_THRESHOLD
    }

    /// 该域名握手成功：清零失败计数，避免偶发中断被误判为「证书固定」。
    pub fn clear_tls_failure(&self, host: &str) {
        let mut m = self.bypass_counts.lock().unwrap();
        m.remove(&host.to_lowercase());
    }

    /// 记录一次「出站 TLS 握手失败」（MiniProxy 到源站）。
    pub fn record_outbound_tls_failure(&self, host: &str) -> bool {
        let mut m = self.outbound_tls_failures.lock().unwrap();
        let e = m.entry(host.to_lowercase()).or_insert((0, 0));
        e.0 += 1;
        e.1 = crate::capture::now_ms();
        e.0 >= proxy::OUTBOUND_BYPASS_THRESHOLD
    }

    /// 出站请求成功：清掉该域名的出站失败计数（偶发中断可自愈）。
    pub fn clear_outbound_tls_failure(&self, host: &str) {
        let mut m = self.outbound_tls_failures.lock().unwrap();
        m.remove(&host.to_lowercase());
    }

    /// 该域名是否应因「对端 TLS 不兼容」而临时直通（TTL 内有效，过期后重试 MITM）。
    pub fn outbound_bypassed(&self, host: &str) -> bool {
        let now = crate::capture::now_ms();
        let m = match self.outbound_tls_failures.lock() {
            Ok(m) => m,
            Err(_) => return false,
        };
        match m.get(&host.to_lowercase()) {
            Some((c, t)) => {
                *c >= proxy::OUTBOUND_BYPASS_THRESHOLD
                    && now.saturating_sub(*t) < proxy::OUTBOUND_BYPASS_TTL_MS
            }
            None => false,
        }
    }

    /// 当前生效的上游代理（None = 直连）。
    pub fn upstream(&self) -> Option<dial::Upstream> {
        self.upstream.read().ok().and_then(|g| g.clone())
    }

    /// 切换上游代理，并记录来源（下一次拨号起生效）。
    pub fn set_upstream(&self, up: Option<dial::Upstream>, source: &str) {
        if let Ok(mut g) = self.upstream.write() {
            *g = up;
        }
        if let Ok(mut s) = self.upstream_source.lock() {
            *s = source.to_string();
        }
    }

    pub fn upstream_source(&self) -> String {
        self.upstream_source
            .lock()
            .map(|s| s.clone())
            .unwrap_or_else(|_| "unknown".to_string())
    }

    /// 把上游设置持久化到 ~/.miniproxy/config.json，下次启动沿用。
    pub fn save_upstream_config(&self) {
        self.save_config();
    }

    /// 把「上游级联 + 分流规则」一起写盘（两者共用一个配置文件，必须整体写，
    /// 否则保存其一会把另一部分冲掉）。
    pub fn save_config(&self) {
        let up = self.upstream();
        let r = self.rules_snapshot();
        let direct_no_mitm = r.direct_skip_mitm();
        let cfg = config::Config {
            upstream_enabled: Some(up.is_some()),
            upstream_addr: up.as_ref().map(|u| u.addr()),
            direct: r.direct,
            proxied: r.proxied,
            direct_no_mitm: Some(direct_no_mitm),
        };
        if let Err(e) = config::save(&cfg) {
            eprintln!("  保存配置失败: {}", e);
        }
    }

    /// 当前分流规则快照。
    pub fn rules_snapshot(&self) -> rules::Rules {
        self.rules.read().map(|g| g.clone()).unwrap_or_default()
    }

    /// 替换分流规则（运行期立即生效）。
    pub fn set_rules(&self, r: rules::Rules) {
        if let Ok(mut g) = self.rules.write() {
            *g = r;
        }
    }

    /// 该 host 的路由结论（分流规则 + 内置直连段）。
    pub fn route_of(&self, host: &str) -> rules::Route {
        self.rules_snapshot().route(host)
    }

    /// 该 host 实际要用的上游代理：命中「跳过代理」则为 None（直连源站）。
    pub fn upstream_for(&self, host: &str) -> Option<dial::Upstream> {
        self.rules_snapshot().upstream_for(host, self.upstream())
    }
}

/// 探测本机局域网 IP：解析 `ifconfig` 输出里的私网地址。
/// 排除回环 / 链路本地 / Clash TUN 的 fake-IP 网段（198.18.0.0/15）/ 运营商 CGNAT（100.64/10），
/// 优先 192.168.*，其次 10.*，再次 172.16-31.*；失败时回退到路由表探测。
fn detect_lan_ip() -> Option<String> {
    use std::process::Command;

    let priority = |ip: &str| -> Option<u8> {
        if ip.starts_with("192.168.") {
            Some(3)
        } else if ip.starts_with("10.") {
            Some(2)
        } else if let Some(rest) = ip.strip_prefix("172.") {
            let second: u8 = rest.split('.').next()?.parse().ok()?;
            if (16..=31).contains(&second) {
                Some(1)
            } else {
                None
            }
        } else {
            None
        }
    };
    let excluded = |ip: &str| -> bool {
        ip.starts_with("127.")
            || ip.starts_with("169.254.")
            || ip.starts_with("198.18.")
            || ip.starts_with("198.19.")
            || ip.starts_with("100.64.")
    };

    if let Ok(out) = Command::new("ifconfig").output() {
        let text = String::from_utf8_lossy(&out.stdout);
        let mut best: Option<(u8, String)> = None;
        for line in text.lines() {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("inet ") {
                let ip = rest.split_whitespace().next().unwrap_or("");
                if excluded(ip) {
                    continue;
                }
                if let Some(p) = priority(ip) {
                    if best.as_ref().map(|(bp, _)| p > *bp).unwrap_or(true) {
                        best = Some((p, ip.to_string()));
                    }
                }
            }
        }
        if let Some((_, ip)) = best {
            return Some(ip);
        }
    }

    // 回退：路由表探测（不发实际数据包）
    if let Ok(s) = std::net::UdpSocket::bind("0.0.0.0:0") {
        if s.connect("8.8.8.8:80").is_ok() {
            let ip = s.local_addr().ok()?.ip().to_string();
            if !ip.starts_with("127.") && !excluded(&ip) {
                return Some(ip);
            }
        }
    }
    None
}

fn env_or(name: &str, default: u16) -> u16 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[tokio::main]
async fn main() {
    // 看门狗模式：作为独立子进程运行，探测父进程消失后恢复系统代理。
    // 必须在任何初始化（CA、端口绑定）之前拦截。
    let args: Vec<String> = std::env::args().collect();
    if args.len() >= 3 && args[1] == "--sysproxy-watchdog" {
        sysproxy::watchdog_run(&args[2]);
        return;
    }

    let proxy_port = env_or("MINIPROXY_PORT", 34567);
    let api_port = env_or("MINIPROXY_API_PORT", 9000);
    let api_host = std::env::var("MINIPROXY_API_HOST").unwrap_or_else(|_| "127.0.0.1".into());
    let upstream = resolve_upstream();
    let lan_ip = detect_lan_ip();
    let rules_cfg = config::load().rules();

    let store = Arc::new(Store::new(5000));
    let ca = Arc::new(ca::Ca::load_or_create().expect("初始化本地 CA 失败"));
    let rules = Arc::new(std::sync::RwLock::new(rules_cfg.clone()));
    let client = build_http_client(upstream.shared.clone(), rules.clone());

    println!("==============================================");
    println!("  MiniProxy 抓包代理");
    println!("==============================================");
    println!("  代理端口      : 0.0.0.0:{}  (HTTP/HTTPS/WS)", proxy_port);
    println!("  界面 & API    : http://{}:{}", api_host, api_port);
    println!("  CA 证书       : {}", ca.cert_path.display());
    if api_host != "127.0.0.1" {
        if let Some(ip) = &lan_ip {
            println!("  手机抓包      : 代理 {}:{}，CA 证书 http://{}:{}/api/ca.crt", ip, proxy_port, ip, api_port);
        }
    }
    println!("                  (浏览器/系统需信任该证书才能解密 HTTPS)");
    match &upstream.initial {
        Some(up) => println!(
            "  上游级联      : {}:{}  (来源: {})，出站流量经此代理转发",
            up.host, up.port, source_label(&upstream.source)
        ),
        None if upstream.source == "auto-pending" => {
            println!("  上游级联      : 正在后台探测本机代理…（探测到会自动启用，见下方提示）")
        }
        None => println!(
            "  上游级联      : 未启用（界面「上游级联」可一键检测开启，或设 MINIPROXY_UPSTREAM_PROXY）"
        ),
    }
    println!("==============================================");

    let app = Arc::new(App {
        store,
        ca,
        client,
        proxy_port,
        api_port,
        lan_ip,
        upstream: upstream.shared.clone(),
        upstream_source: std::sync::Mutex::new(upstream.source.clone()),
        bypass_counts: std::sync::Mutex::new(std::collections::HashMap::new()),
        outbound_tls_failures: std::sync::Mutex::new(std::collections::HashMap::new()),
        rules,
    });

    if !rules_cfg.direct.is_empty() || !rules_cfg.proxied.is_empty() {
        println!(
            "  分流规则      : 跳过代理 {} 条 / 强制走代理 {} 条（界面「🚦 分流规则」可改）",
            rules_cfg.direct.len(),
            rules_cfg.proxied.len()
        );
        println!("==============================================");
    }

    // API + 静态界面服务
    {
        let app = app.clone();
        let ip = if api_host == "0.0.0.0" || api_host.is_empty() {
            [0, 0, 0, 0]
        } else {
            api_host
                .split('.')
                .filter_map(|p| p.parse::<u8>().ok())
                .collect::<Vec<_>>()
                .try_into()
                .unwrap_or([127, 0, 0, 1])
        };
        let addr = std::net::SocketAddr::from((ip, api_port));
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

    // 代理服务（放入后台任务，主任务等待退出信号）
    let app2 = app.clone();
    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], proxy_port));
    let make_svc = hyper::service::make_service_fn(move |conn: &AddrStream| {
        let app = app2.clone();
        let peer_ip = conn.remote_addr().ip().to_string();
        // 仅本机连接反查进程（远端设备的端口在 lsof 里查不到，跳过省时）
        let local = matches!(peer_ip.as_str(), "127.0.0.1" | "::1");
        let peer_port = conn.remote_addr().port();
        async move {
            let name = if local {
                tokio::task::spawn_blocking(move || attrib::process_for_port(peer_port))
                    .await
                    .ok()
                    .flatten()
            } else {
                None
            };
            let info = capture::ClientInfo { name, ip: peer_ip };
            Ok::<_, std::convert::Infallible>(hyper::service::service_fn(move |req| {
                proxy::handle_proxy(req, app.clone(), info.clone())
            }))
        }
    });

    {
        tokio::spawn(async move {
            if let Err(e) = hyper::Server::bind(&addr).serve(make_svc).await {
                eprintln!("代理服务启动失败: {}", e);
                std::process::exit(1);
            }
        });
    }

    // 从未配置过上游时：后台探测本机常见代理端口（Clash/Charles/Surge/v2ray…），
    // 探测到即自动启用。放后台任务里做，避免拖慢启动、也不阻塞端口监听。
    if upstream.initial.is_none() && upstream.source == "auto-pending" {
        let auto_app = app.clone();
        tokio::spawn(async move {
            let found = dial::detect_local_upstream(&[proxy_port, api_port]).await;
            if let Some(up) = found.into_iter().next() {
                println!(
                    "\n  上游级联      : 自动检测到本机代理 {}:{} 并已启用（界面「上游级联」可关闭或更换）",
                    up.host, up.port
                );
                auto_app.set_upstream(Some(up), "auto");
                auto_app.save_upstream_config();
            }
        });
    }

    // 等待退出信号：Ctrl+C (SIGINT) / SIGTERM / SIGHUP（终端关闭）
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("注册 SIGTERM 失败");
    let mut hup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
        .expect("注册 SIGHUP 失败");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {},
        _ = term.recv() => {},
        _ = hup.recv() => {},
    }

    println!("\n收到退出信号，正在清理…");
    // 若系统代理由本进程开启，恢复用户原有配置
    if sysproxy::is_on() {
        match sysproxy::disable() {
            Ok(()) => println!("  已恢复系统代理设置"),
            Err(e) => eprintln!("  恢复系统代理失败: {}（可手动执行 networksetup 或重启后重开一次代理再关闭）", e),
        }
    }
    println!("MiniProxy 已退出。");
}

fn build_http_client(
    upstream: dial::SharedUpstream,
    rules: rules::SharedRules,
) -> HttpClient {
    let connector = dial::ProxyConnector { upstream, rules };
    Client::builder().build(connector)
}

/// 启动时的初始上游配置（决定于环境变量 / 界面保存的配置）。
struct UpstreamInit {
    initial: Option<dial::Upstream>,
    shared: dial::SharedUpstream,
    /// env（环境变量）/ saved（界面保存）/ off（用户关闭过）/ auto-pending（未配置，待自动探测）
    source: String,
}

/// 上游级联的取值优先级：环境变量 > 界面保存的配置 >（未配置时）后台自动探测。
fn resolve_upstream() -> UpstreamInit {
    let cfg = config::load();
    let (initial, source) = if let Some(up) = dial::Upstream::from_env() {
        (Some(up), "env")
    } else if cfg.upstream_enabled == Some(true) {
        let up = cfg.upstream_addr.as_deref().and_then(dial::Upstream::parse);
        if up.is_none() {
            eprintln!("提示：已保存的上游地址无效，本次回退为直连，请在界面重新设置。");
        }
        (up, "saved")
    } else if cfg.upstream_enabled == Some(false) {
        // 用户显式关闭过，尊重选择，不再自动探测
        (None, "off")
    } else {
        (None, "auto-pending")
    };
    UpstreamInit {
        shared: std::sync::Arc::new(std::sync::RwLock::new(initial.clone())),
        initial,
        source: source.to_string(),
    }
}

fn source_label(source: &str) -> &'static str {
    match source {
        "env" => "环境变量 MINIPROXY_UPSTREAM_PROXY",
        "saved" => "上次在界面中保存",
        "auto" => "启动时自动检测",
        "manual" => "界面手动设置",
        _ => "当前配置",
    }
}
