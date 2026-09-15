//! 一键设置系统代理（macOS 通过 networksetup 实现）。
//!
//! 设计要点：
//! - enable 前把所有网络服务的现有代理配置备份到 ~/.miniproxy/sysproxy-backup.json，
//!   disable 时优先按备份恢复（避免覆盖用户原有的代理，如 Clash 等）。
//! - 关闭用 `-set...proxystate off`，不会留下 Enabled: Yes / Server: off 的脏状态。

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

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
        Ok(())
    } else {
        Err(errs.join("；"))
    }
}

/// 关闭系统代理：优先按备份恢复用户原有配置，否则彻底关闭。
pub fn disable() -> Result<(), String> {
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
