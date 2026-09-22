//! 一键设置系统代理（macOS 通过 networksetup 实现）。
//!
//! 设计要点：
//! - enable 前把所有网络服务的现有代理配置备份到 ~/.miniproxy/sysproxy-backup.json，
//!   disable 时优先按备份恢复（避免覆盖用户原有的代理，如 Clash 等）。
//! - 关闭用 `-set...proxystate off`，不会留下 Enabled: Yes / Server: off 的脏状态。
//! - 程序退出时自动恢复：
//!   - 优雅退出（Ctrl+C / SIGTERM / SIGHUP）：主进程捕获信号后调用 disable()；
//!   - 强杀 / 崩溃（kill -9）：enable 时孵化一个看门狗子进程
//!     （`miniproxy --sysproxy-watchdog <父进程 PID>`），父进程消失即按备份恢复系统代理，
//!     防止系统代理指向已死端口断网。
//!
//! 看门狗的存活判定（重要）：曾实现为「每秒 ps -p <pid> 查进程名」，实践中脆弱——
//! ps 在受限终端 / 沙箱里会执行失败或被拦截，此时 `output()` 返回 Err，被当成“父进程已死”，
//! 于是刚开启的系统代理在 1 秒内就被误恢复掉。现改为父子各持 socketpair 一端：
//! 父进程无论正常退出、被 kill -9 还是崩溃，其所有 fd 都会关闭，子进程读到 EOF 即为父进程消失。
//! 这条路径完全不依赖外部命令，也不会因 PID 复用误判。

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

/// 本进程当前是否开启了系统代理（用于优雅退出时判断是否需要恢复）。
static SYS_PROXY_ON: AtomicBool = AtomicBool::new(false);

/// 看门狗子进程句柄（None 表示未孵化或已退出）。
static WATCHDOG: Mutex<Option<std::process::Child>> = Mutex::new(None);

/// socketpair 的父进程端：必须长期持有，一旦关闭（含进程退出）子进程就会读到 EOF。
static WATCHDOG_LINK: Mutex<Option<std::os::unix::net::UnixStream>> = Mutex::new(None);

#[derive(Serialize, Deserialize, Clone)]
pub struct ServiceState {
    pub name: String,
    pub http: Option<(String, u16)>,
    pub https: Option<(String, u16)>,
    pub socks: Option<(String, u16)>,
}

#[derive(Serialize, Deserialize)]
struct Backup {
    port: u16,
    services: Vec<ServiceState>,
}

pub fn supported() -> bool {
    cfg!(target_os = "macos")
}

/// 本进程是否已开启系统代理。
pub fn is_on() -> bool {
    SYS_PROXY_ON.load(Ordering::SeqCst)
}

/// 孵化看门狗子进程（已存活则跳过）。父进程被强杀后由它负责恢复系统代理。
fn spawn_watchdog() {
    let mut g = WATCHDOG.lock().unwrap();
    // 已有存活的看门狗则不重复孵化
    if let Some(child) = g.as_mut() {
        if child.try_wait().map(|st| st.is_none()).unwrap_or(true) {
            return;
        }
    }
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(_) => return,
    };

    // 父子各持 socketpair 一端：父进程消失（正常退出 / kill -9 / 崩溃）时，
    // 它持有的那端随之关闭，子进程读到 EOF。相比 ps 轮询，不受命令可用性影响。
    let (parent_end, child_end) = match std::os::unix::net::UnixStream::pair() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("创建看门狗管道失败: {}", e);
            return;
        }
    };

    let child_io = std::process::Stdio::from(std::os::fd::OwnedFd::from(child_end));
    let child = std::process::Command::new(exe)
        .arg("--sysproxy-watchdog")
        .arg(std::process::id().to_string())
        .stdin(child_io)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    match child {
        Ok(c) => {
            // 父端必须一直保存在 static 里；drop 掉会让子进程立刻看到 EOF 从而误恢复
            if let Ok(mut link) = WATCHDOG_LINK.lock() {
                *link = Some(parent_end);
            }
            *g = Some(c);
        }
        Err(e) => eprintln!("孵化系统代理看门狗失败: {}", e),
    }
}

/// 终止看门狗子进程（正常关闭系统代理或优雅退出时调用）。
fn stop_watchdog() {
    // 先关闭父端：即使 kill 失败，子进程也会因读到 EOF 而自行退出
    if let Ok(mut link) = WATCHDOG_LINK.lock() {
        *link = None;
    }
    let mut g = WATCHDOG.lock().unwrap();
    if let Some(mut child) = g.take() {
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// 看门狗主循环：阻塞读父进程持有的 socketpair 端，读到 EOF 即父进程消失，按备份恢复后退出。
pub fn watchdog_run(_parent_pid: &str) {
    let mut stdin = std::io::stdin();
    let mut buf = [0u8; 64];
    loop {
        match std::io::Read::read(&mut stdin, &mut buf) {
            Ok(0) => break, // EOF：父进程已退出/被杀/崩溃，它的 fd 都关了
            Ok(_) => continue,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    // EOF 已足以判定父进程消失（只有它持有另一端），直接恢复，不再用 ps 二次确认，
    // 避免像旧实现那样因 ps 不可用而误判。
    match disable() {
        Ok(()) => println!("[miniproxy] 主进程已退出，系统代理已恢复为开启前的状态"),
        Err(e) => eprintln!("[miniproxy] 主进程已退出，但恢复系统代理失败: {}", e),
    }
}

fn backup_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".miniproxy").join("sysproxy-backup.json")
}

fn run(program: &str, args: &[&str]) -> Result<String, String> {
    let out = std::process::Command::new(program)
        .args(args)
        .output()
        .map_err(|e| format!("执行 {} 失败: {}", program, e))?;
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(format!("{} {}: {}", program, args.join(" "), stderr.trim()));
    }
    Ok(stdout)
}

/// 列出系统网络服务（跳过首行说明行）。
pub fn list_services() -> Result<Vec<String>, String> {
    let out = run("networksetup", &["-listallnetworkservices"])?;
    let mut svcs = Vec::new();
    for (i, line) in out.lines().enumerate() {
        if i == 0 {
            continue;
        }
        let name = line.trim().trim_start_matches('*').trim();
        if !name.is_empty() {
            svcs.push(name.to_string());
        }
    }
    Ok(svcs)
}

/// 解析 `networksetup -getwebproxy <svc>` 样式输出。
fn parse_proxy_output(out: &str) -> Option<(String, u16)> {
    let mut enabled = false;
    let mut host = String::new();
    let mut port = 0u16;
    for line in out.lines() {
        let mut it = line.splitn(2, ':');
        let k = it.next().unwrap_or("").trim().to_lowercase();
        let v = it.next().unwrap_or("").trim().to_string();
        match k.as_str() {
            "enabled" => enabled = v.eq_ignore_ascii_case("yes"),
            "server" => host = v,
            "port" => port = v.parse().unwrap_or(0),
            _ => {}
        }
    }
    if enabled && port > 0 {
        Some((host, port))
    } else {
        None
    }
}

fn get_proxy(service: &str, kind: &str) -> Option<(String, u16)> {
    let out = run("networksetup", &[kind, service]).ok()?;
    parse_proxy_output(&out)
}

pub fn status() -> Result<Vec<ServiceState>, String> {
    let services = list_services()?;
    Ok(services
        .into_iter()
        .map(|name| ServiceState {
            http: get_proxy(&name, "-getwebproxy"),
            https: get_proxy(&name, "-getsecurewebproxy"),
            socks: get_proxy(&name, "-getsocksfirewallproxy"),
            name,
        })
        .collect())
}

/// 将所有网络服务的 HTTP / HTTPS（及 SOCKS）代理指向 127.0.0.1:port。
/// 开启前备份现有配置。
pub fn enable(port: u16) -> Result<(), String> {
    let services = status()?;
    let backup = Backup { port, services };
    let path = backup_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    std::fs::write(&path, serde_json::to_string(&backup).unwrap_or_default())
        .map_err(|e| format!("写入代理配置备份失败: {}", e))?;

    let port_s = port.to_string();
    let mut errs = Vec::new();
    for s in list_services()? {
        if let Err(e) = run("networksetup", &["-setwebproxy", &s, "127.0.0.1", &port_s]) {
            errs.push(format!("{}: {}", s, e));
            continue;
        }
        if let Err(e) = run("networksetup", &["-setsecurewebproxy", &s, "127.0.0.1", &port_s]) {
            errs.push(format!("{}: {}", s, e));
        }
        // SOCKS 失败不视为致命（部分服务不支持）
        let _ = run("networksetup", &["-setsocksfirewallproxy", &s, "127.0.0.1", &port_s]);
    }
    if errs.is_empty() {
        SYS_PROXY_ON.store(true, Ordering::SeqCst);
        spawn_watchdog();
        Ok(())
    } else {
        Err(errs.join("；"))
    }
}

/// 关闭系统代理：优先按备份恢复用户原有配置，否则彻底关闭。
pub fn disable() -> Result<(), String> {
    SYS_PROXY_ON.store(false, Ordering::SeqCst);
    stop_watchdog();
    let path = backup_path();
    let backup: Option<Backup> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok());

    let mut errs = Vec::new();
    match backup {
        Some(b) => {
            // 按备份逐服务恢复
            for s in &b.services {
                restore_service(&s.name, s.http.clone(), "-setwebproxy", "-setwebproxystate", &mut errs);
                restore_service(&s.name, s.https.clone(), "-setsecurewebproxy", "-setsecurewebproxystate", &mut errs);
                restore_service(&s.name, s.socks.clone(), "-setsocksfirewallproxy", "-setsocksfirewallproxystate", &mut errs);
            }
            let _ = std::fs::remove_file(&path);
        }
        None => {
            for s in list_services()? {
                if let Err(e) = run("networksetup", &["-setwebproxystate", &s, "off"]) {
                    errs.push(format!("{}: {}", s, e));
                }
                if let Err(e) = run("networksetup", &["-setsecurewebproxystate", &s, "off"]) {
                    errs.push(format!("{}: {}", s, e));
                }
                let _ = run("networksetup", &["-setsocksfirewallproxystate", &s, "off"]);
            }
        }
    }
    if errs.is_empty() {
        Ok(())
    } else {
        Err(errs.join("；"))
    }
}

#[allow(clippy::too_many_arguments)]
fn restore_service(
    name: &str,
    cfg: Option<(String, u16)>,
    set_flag: &str,
    state_flag: &str,
    errs: &mut Vec<String>,
) {
    match cfg {
        Some((host, port)) => {
            if let Err(e) = run("networksetup", &[set_flag, name, &host, &port.to_string()]) {
                errs.push(format!("{}: {}", name, e));
            }
        }
        None => {
            if let Err(e) = run("networksetup", &[state_flag, name, "off"]) {
                errs.push(format!("{}: {}", name, e));
            }
        }
    }
}
