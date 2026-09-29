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
    /// 开启前的 bypass 域名列表（关闭时原样恢复）
    #[serde(default)]
    pub bypass: Vec<String>,
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

/// 取看门狗句柄锁。不用 unwrap：一旦某个线程在持锁时 panic，unwrap 会二次 panic，
/// 让调用方（disable / 退出清理）半途中断，反而把系统代理留在脏状态。
fn lock_watchdog() -> std::sync::MutexGuard<'static, Option<std::process::Child>> {
    match WATCHDOG.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// 孵化看门狗子进程（已存活则跳过）。父进程被强杀后由它负责恢复系统代理。
///
/// 返回 Err 表示"看门狗没起来" —— 此时强杀本进程不会有人恢复系统代理，
/// 调用方必须把这件事告诉用户，不能静默降级。
fn spawn_watchdog() -> Result<(), String> {
    let mut g = lock_watchdog();
    // 已有存活的看门狗则不重复孵化
    if let Some(child) = g.as_mut() {
        if child.try_wait().map(|st| st.is_none()).unwrap_or(true) {
            return Ok(());
        }
    }
    let exe = std::env::current_exe().map_err(|e| format!("获取自身可执行文件路径失败: {}", e))?;

    // 父子各持 socketpair 一端：父进程消失（正常退出 / kill -9 / 崩溃）时，
    // 它持有的那端随之关闭，子进程读到 EOF。相比 ps 轮询，不受命令可用性影响。
    let (parent_end, child_end) = std::os::unix::net::UnixStream::pair()
        .map_err(|e| format!("创建看门狗管道失败: {}", e))?;

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
            Ok(())
        }
        Err(e) => Err(format!("孵化看门狗子进程失败: {}", e)),
    }
}

/// 终止看门狗子进程（正常关闭系统代理或优雅退出时调用）。
///
/// 顺序很重要：**必须先 kill 子进程、再关闭父端 socketpair**。
/// 反过来（旧实现）会给子进程留一个抢跑窗口：它读到 EOF 后也开始执行 disable()，
/// 于是父子两个进程并发改系统代理 —— 子进程还会把备份文件删掉，
/// 导致父进程读到 None 走"全部关闭"兜底分支，而不是恢复用户原本的代理设置。
fn stop_watchdog() {
    {
        let mut g = lock_watchdog();
        if let Some(mut child) = g.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
    // 子进程已回收，此时再关父端就不会引发抢跑
    if let Ok(mut link) = WATCHDOG_LINK.lock() {
        *link = None;
    }
}

/// 系统代理配置是否指向 127.0.0.1:port。
fn points_to_port(v: &Option<(String, u16)>, port: u16) -> bool {
    matches!(v, Some((h, p)) if *p == port && (h == "127.0.0.1" || h == "localhost"))
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

/// 读取某网络服务当前的 bypass 域名列表。
/// `networksetup` 在列表为空时输出一句说明文案，需要过滤掉。
pub fn get_bypass(service: &str) -> Vec<String> {
    let out = match run("networksetup", &["-getproxybypassdomains", service]) {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };
    let mut list = Vec::new();
    for line in out.lines() {
        let l = line.trim();
        if l.is_empty() {
            continue;
        }
        let lc = l.to_lowercase();
        if lc.starts_with("there aren't any") || lc.contains("bypass domains") {
            continue;
        }
        list.push(l.to_string());
    }
    list
}

/// 写入某网络服务的 bypass 域名列表（空列表用 macOS 规定的占位符 `Empty` 清空）。
fn set_bypass(service: &str, entries: &[String]) -> Result<(), String> {
    if entries.is_empty() {
        return run(
            "networksetup",
            &["-setproxybypassdomains", service, "Empty"],
        )
        .map(|_| ());
    }
    let mut args: Vec<&str> = vec!["-setproxybypassdomains", service];
    for e in entries {
        args.push(e.as_str());
    }
    run("networksetup", &args).map(|_| ())
}

/// 把「跳过代理」规则同步进系统代理的 bypass 列表（系统代理已开启时调用）。
///
/// 以开启时的备份为基准：`原始 bypass ∪ 规则条目`，避免多次保存规则后条目不断累积。
pub fn sync_bypass(entries: &[String]) -> Result<(), String> {
    let path = backup_path();
    let backup: Option<Backup> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok());
    let originals: Vec<(String, Vec<String>)> = match backup {
        Some(b) => b.services.into_iter().map(|s| (s.name, s.bypass)).collect(),
        None => list_services()?
            .into_iter()
            .map(|n| {
                let b = get_bypass(&n);
                (n, b)
            })
            .collect(),
    };

    let mut errs = Vec::new();
    for (name, orig) in originals {
        let mut list = orig.clone();
        for e in entries {
            if !list.iter().any(|x| x.eq_ignore_ascii_case(e)) {
                list.push(e.clone());
            }
        }
        if let Err(e) = set_bypass(&name, &list) {
            errs.push(format!("{}: {}", name, e));
        }
    }
    if errs.is_empty() {
        Ok(())
    } else {
        Err(errs.join("；"))
    }
}

pub fn status() -> Result<Vec<ServiceState>, String> {
    let services = list_services()?;
    Ok(services
        .into_iter()
        .map(|name| ServiceState {
            http: get_proxy(&name, "-getwebproxy"),
            https: get_proxy(&name, "-getsecurewebproxy"),
            socks: get_proxy(&name, "-getsocksfirewallproxy"),
            bypass: get_bypass(&name),
            name,
        })
        .collect())
}

/// 启动自愈：上一次运行若没有正常收尾（被强杀、看门狗也没活下来、enable 中途失败），
/// 会留下一份备份文件，而系统代理可能仍然指向那个已经停掉的端口 —— 表现就是"莫名断网"。
///
/// 判定与处置：
/// - 系统代理**没**指向备份里的端口：说明已被手动或其他工具改回，只提示、不动它，
///   也保留备份文件（可能是别的实例正在用的）。
/// - 系统代理**仍**指向备份里的端口，且该端口当前空闲（没有别的实例在监听）：
///   说明属主进程确实已经死了，按备份自动恢复。
/// - 系统代理指向该端口但端口被占用：有实例正在服务，不动。
///
/// 返回给调用方打印的提示文本。
pub fn check_stale_backup(port: u16) -> Option<String> {
    let path = backup_path();
    let raw = std::fs::read_to_string(&path).ok()?;
    let backup: Backup = match serde_json::from_str(&raw) {
        Ok(b) => b,
        Err(_) => {
            let _ = std::fs::remove_file(&path);
            return Some(format!(
                "发现损坏的系统代理备份文件，已删除：{}",
                path.display()
            ));
        }
    };

    // 备份对不上本实例的端口 → 不是本实例留下的，不处理
    if backup.port != port {
        return Some(format!(
            "发现一份系统代理备份（对应 127.0.0.1:{}，本实例端口 {}），不是本实例留下的，跳过",
            backup.port, port
        ));
    }

    let svcs = match status() {
        Ok(s) => s,
        Err(e) => return Some(format!("检查系统代理状态失败: {}", e)),
    };
    let pointing: Vec<&ServiceState> = svcs
        .iter()
        .filter(|s| points_to_port(&s.http, backup.port) || points_to_port(&s.https, backup.port))
        .collect();

    if pointing.is_empty() {
        return Some(format!(
            "上次运行留下了系统代理备份（记录 {} 个网络服务的原配置），但系统代理当前并未指向 127.0.0.1:{}，可能已被手动或由其他工具改回。为不覆盖你现在的设置，本次不动系统代理（备份文件保留在 {}）",
            backup.services.len(),
            backup.port,
            path.display()
        ));
    }

    // 端口还被人监听 → 有实例在服务，本次启动大概率会因为端口占用而退出，别动
    if std::net::TcpListener::bind(("127.0.0.1", backup.port)).is_err() {
        return Some(format!(
            "系统代理仍指向 127.0.0.1:{}，且该端口有进程在监听（可能是另一个 MiniProxy 实例），跳过自动恢复",
            backup.port
        ));
    }

    match disable() {
        Ok(()) => Some(format!(
            "上次退出未恢复系统代理（{} 个网络服务仍指向已停止的 127.0.0.1:{}），已按备份自动恢复为开启前的配置",
            pointing.len(),
            backup.port
        )),
        Err(e) => Some(format!(
            "上次退出未恢复系统代理，自动恢复失败: {}。请到「系统设置 → 网络 → 代理」手动检查",
            e
        )),
    }
}

/// 将所有网络服务的 HTTP / HTTPS（及 SOCKS）代理指向 127.0.0.1:port，
/// 并把 `extra_bypass`（分流规则里「跳过代理」的条目）追加进 bypass 列表。
/// 开启前备份现有配置（含原 bypass 列表），关闭时原样恢复。
pub fn enable(port: u16, extra_bypass: &[String]) -> Result<(), String> {
    let path = backup_path();
    let services = status()?;

    // 重复点击「开启系统代理」（系统代理已指向本实例）时，绝不能拿现状覆盖备份：
    // 否则备份里记的是"指向自己"，关闭时会按它恢复成已经失效的端口 → 断网。
    let existing: Option<Backup> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok());
    let already_ours = existing.as_ref().map(|b| b.port == port).unwrap_or(false);
    // 回滚基准：优先用最早那份备份（真正的"开启前"配置），没有才用现场值
    let rollback_base: Vec<ServiceState> = match &existing {
        Some(b) => b.services.clone(),
        None => services.clone(),
    };

    if !already_ours {
        let backup = Backup {
            port,
            services: services.clone(),
        };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        std::fs::write(&path, serde_json::to_string(&backup).unwrap_or_default())
            .map_err(|e| format!("写入代理配置备份失败: {}", e))?;
    }

    let port_s = port.to_string();
    let mut errs = Vec::new();
    let mut changed: Vec<String> = Vec::new();
    for s in list_services()? {
        let mut svc_ok = true;
        if let Err(e) = run("networksetup", &["-setwebproxy", &s, "127.0.0.1", &port_s]) {
            errs.push(format!("{}: {}", s, e));
            svc_ok = false;
        }
        if let Err(e) = run("networksetup", &["-setsecurewebproxy", &s, "127.0.0.1", &port_s]) {
            errs.push(format!("{}: {}", s, e));
            svc_ok = false;
        }
        // SOCKS 失败不视为致命（部分服务不支持）
        let _ = run("networksetup", &["-setsocksfirewallproxy", &s, "127.0.0.1", &port_s]);
        if svc_ok {
            changed.push(s.clone());
        }

        // bypass：保留用户原有条目 + 追加分流规则里「跳过代理」的条目
        let mut list = services
            .iter()
            .find(|x| x.name == s)
            .map(|x| x.bypass.clone())
            .unwrap_or_default();
        for e in extra_bypass {
            if !list.iter().any(|x| x.eq_ignore_ascii_case(e)) {
                list.push(e.clone());
            }
        }
        if let Err(e) = set_bypass(&s, &list) {
            // bypass 写失败不影响代理本身可用，只记提示
            eprintln!("[miniproxy] 设置 {} 的 bypass 列表失败: {}", s, e);
        }
    }

    if errs.is_empty() {
        SYS_PROXY_ON.store(true, Ordering::SeqCst);
        // 看门狗起不来 = 强杀本进程后没人能恢复系统代理，属于静默降级，必须让调用方知道
        if let Err(e) = spawn_watchdog() {
            return Err(format!(
                "系统代理已开启，但{}。如需强杀本进程，请先手动关闭系统代理",
                e
            ));
        }
        Ok(())
    } else if already_ours {
        // 只是重复刷新：本次失败不影响上一次已经生效的配置，保持原状即可
        Err(format!(
            "系统代理由本实例开启中，本次刷新失败：{}（原配置仍然有效）",
            errs.join("；")
        ))
    } else {
        // 部分服务设置失败：把已经改过的服务回滚回开启前的配置。
        // 否则会留下"一半指向本实例、一半指向别处"的僵状态，
        // 而且没有 SYS_PROXY_ON 标记，退出时也不会被自动清理。
        let mut rb_errs = Vec::new();
        for name in &changed {
            if let Some(orig) = rollback_base.iter().find(|x| &x.name == name) {
                restore_service(name, orig.http.clone(), "-setwebproxy", "-setwebproxystate", &mut rb_errs);
                restore_service(name, orig.https.clone(), "-setsecurewebproxy", "-setsecurewebproxystate", &mut rb_errs);
                restore_service(name, orig.socks.clone(), "-setsocksfirewallproxy", "-setsocksfirewallproxystate", &mut rb_errs);
                if let Err(e) = set_bypass(name, &orig.bypass) {
                    rb_errs.push(format!("{}: {}", name, e));
                }
            }
        }
        // 已回滚，这份备份不再代表"开启前的配置"，删掉避免下次被误当成有效备份
        let _ = std::fs::remove_file(&path);
        let mut msg = format!("开启系统代理失败：{}", errs.join("；"));
        if rb_errs.is_empty() {
            msg.push_str(&format!("；已回滚 {} 个网络服务的改动", changed.len()));
        } else {
            msg.push_str(&format!("；回滚时又有错误: {}", rb_errs.join("；")));
        }
        Err(msg)
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
                // 恢复开启前的 bypass 列表
                if let Err(e) = set_bypass(&s.name, &s.bypass) {
                    errs.push(format!("{}: {}", s.name, e));
                }
            }
            // 只有全部恢复成功才删备份：否则一旦出错就再也没有"原值"可重试，
            // 用户手动改回去也无从对照，只能自己回忆。
            if errs.is_empty() {
                let _ = std::fs::remove_file(&path);
            } else {
                eprintln!(
                    "[miniproxy] 恢复系统代理有 {} 处失败，已保留备份 {} 便于重试",
                    errs.len(),
                    path.display()
                );
            }
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
