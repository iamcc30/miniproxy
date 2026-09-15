//! 归因：客户端进程识别（哪个 App 发出的请求）与站点（主域名）提取。
//!
//! - `site_of`      : 把 api.example.com / cdn.example.com 归并为 example.com
//! - `process_for_port`: 由客户端本地端口反查其所属进程名（macOS lsof）

use std::collections::HashMap;
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// 双段公共后缀：命中时站点取三段（api.example.com.cn -> example.com.cn）。
const TWO_PART_SUFFIX: &[&str] = &[
    "com.cn", "net.cn", "org.cn", "gov.cn", "edu.cn", "ac.cn",
    "com.hk", "com.tw", "com.mo", "com.uk",
    "co.jp", "co.kr", "co.uk", "org.uk", "ac.uk", "gov.uk", "me.uk",
    "com.au", "net.au", "org.au", "co.nz", "net.nz", "org.nz",
    "com.sg", "com.my", "com.ph", "com.vn", "com.id", "co.th", "co.id",
    "co.in", "net.in", "org.in", "co.za", "co.il", "co.ke", "com.ng",
    "com.br", "com.mx", "com.ar", "com.co", "com.pe", "com.ve", "com.uy",
    "com.tr", "com.sa", "com.eg", "com.pk", "com.bd", "com.ua", "com.pl",
    "com.ru", "com.es", "com.pt", "com.gr", "com.ro", "com.tw",
];

/// 站点（主域名）：把同一站点的多级子域归并到一起。
///
/// - `api.example.com` / `cdn.example.com` -> `example.com`
/// - `a.b.example.com.cn` -> `example.com.cn`
/// - IP / IPv6 / localhost -> 原样返回
pub fn site_of(host: &str) -> String {
    let h = host
        .trim()
        .trim_matches(|c| c == '[' || c == ']')
        .trim_end_matches('.')
        .to_lowercase();
    if h.is_empty() {
        return String::new();
    }
    // IPv6 / IPv4 / 带端口残留
    if h.contains(':') || h.parse::<std::net::Ipv4Addr>().is_ok() {
        return h;
    }
    let parts: Vec<&str> = h.split('.').filter(|s| !s.is_empty()).collect();
    if parts.len() < 2 {
        return h; // localhost、单标签内网名
    }
    let n = parts.len();
    let last2 = format!("{}.{}", parts[n - 2], parts[n - 1]);
    if n >= 3 && TWO_PART_SUFFIX.contains(&last2.as_str()) {
        format!("{}.{}", parts[n - 3], last2)
    } else {
        last2
    }
}

/* ---------------- 客户端进程归因 ---------------- */

const CACHE_TTL: Duration = Duration::from_secs(20);
const CACHE_CAP: usize = 1024;

fn cache() -> &'static Mutex<HashMap<u16, (Option<String>, Instant)>> {
    static C: OnceLock<Mutex<HashMap<u16, (Option<String>, Instant)>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 是否启用客户端进程归因（默认开启；`MINIPROXY_NO_APP=1` 可关闭）。
pub fn enabled() -> bool {
    static E: OnceLock<bool> = OnceLock::new();
    *E.get_or_init(|| {
        !matches!(
            std::env::var("MINIPROXY_NO_APP").as_deref(),
            Ok("1") | Ok("true") | Ok("yes")
        )
    })
}

/// 由客户端本地端口反查所属进程名（带缓存）。
///
/// 内部会调用 `lsof`，属阻塞操作（约 60ms），调用方应放在 blocking 线程中执行。
pub fn process_for_port(port: u16) -> Option<String> {
    if !enabled() {
        return None;
    }
    {
        let c = cache().lock().unwrap();
        if let Some((v, at)) = c.get(&port) {
            if at.elapsed() < CACHE_TTL {
                return v.clone();
            }
        }
    }
    let found = lsof_lookup(port);
    let mut c = cache().lock().unwrap();
    if c.len() >= CACHE_CAP {
        c.clear();
    }
    c.insert(port, (found.clone(), Instant::now()));
    found
}

fn lsof_bin() -> &'static str {
    static B: OnceLock<&'static str> = OnceLock::new();
    B.get_or_init(|| {
        if std::path::Path::new("/usr/sbin/lsof").exists() {
            "/usr/sbin/lsof"
        } else {
            "lsof"
        }
    })
}

fn lsof_lookup(port: u16) -> Option<String> {
    let self_pid = std::process::id();
    // 注意：`-iTCP:<port>` 必须是单个参数，拆成 `-iTCP` + `:port` lsof 会当作文件名报错
    let port_arg = format!("-iTCP:{}", port);
    let out = Command::new(lsof_bin())
        .args(["-nP", port_arg.as_str(), "-sTCP:ESTABLISHED", "-F", "pc"])
        .output()
        .ok()?;
    if out.stdout.is_empty() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut cur_pid: u32 = 0;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix('p') {
            cur_pid = rest.trim().parse().unwrap_or(0);
        } else if let Some(rest) = line.strip_prefix('c') {
            // 过滤掉本进程（代理自身的监听端也会出现在结果里）
            if cur_pid == 0 || cur_pid == self_pid {
                continue;
            }
            let name = decode_lsof_name(rest.trim());
            if !name.is_empty() && !name.eq_ignore_ascii_case("lsof") {
                return Some(name);
            }
        }
    }
    None
}

/// lsof 会把非 ASCII 字节输出为 `\xNN` 转义（如「企业微信」），此处还原为 UTF-8。
fn decode_lsof_name(s: &str) -> String {
    if !s.contains("\\x") {
        return s.to_string();
    }
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() && b[i + 1] == b'x' {
            if let Ok(v) = u8::from_str_radix(&s[i + 2..i + 4], 16) {
                out.push(v);
                i += 4;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn site_merge() {
        assert_eq!(site_of("api.example.com"), "example.com");
        assert_eq!(site_of("cdn.static.example.com"), "example.com");
        assert_eq!(site_of("a.b.example.com.cn"), "example.com.cn");
        assert_eq!(site_of("chat.openai.com"), "openai.com");
        assert_eq!(site_of("127.0.0.1"), "127.0.0.1");
        assert_eq!(site_of("localhost"), "localhost");
        assert_eq!(site_of(""), "");
    }

    #[test]
    fn lsof_escape_decode() {
        assert_eq!(decode_lsof_name("curl"), "curl");
        assert_eq!(
            decode_lsof_name("\\xe4\\xbc\\x81\\xe4\\xb8\\x9a\\xe5\\xbe\\xae\\xe4\\xbf\\xa1"),
            "企业微信"
        );
    }
}
