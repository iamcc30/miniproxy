//! 出站分流规则：哪些域名/IP 跳过代理（直连），哪些强制走代理。
//!
//! 两个列表（都支持域名、通配符、IP、CIDR 网段，大小写不敏感）：
//! - `direct`：命中者**直连源站**，不经上游级联（`upstream_for()` 返回 None）；
//! - `proxied`：命中者**强制走上游**，优先级高于 `direct` 与内置直连段。
//!
//! 条目写法：
//! - `example.com` / `*.example.com` / `.example.com`：本域及所有子域
//! - `*.cdn.example.com`、`api-*.foo.com`：`*` 通配（可出现在任意位置）
//! - `1.2.3.4`、`192.168.1.*`：单个 IP / IPv4 通配
//! - `10.0.0.0/8`、`192.168.0.0/16`、`fc00::/7`：CIDR 网段
//!
//! 内置直连（无需配置，可用 `proxied` 覆盖）：回环、链路本地、私有网段、
//! mDNS（`*.local`）——这些目标经外部上游转发没有意义，通常也根本到不了。

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::{Arc, RwLock};

use crate::dial::Upstream;

/// 运行期可变的分流规则（界面里改完立即对新连接生效）。
pub type SharedRules = Arc<RwLock<Rules>>;

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Rules {
    /// 跳过代理、直连源站的域名/IP
    pub direct: Vec<String>,
    /// 强制走上游代理的域名/IP（优先级最高）
    pub proxied: Vec<String>,
    /// 直连的域名是否同时跳过 MITM 解密（默认 true：内网/自签证书站点居多，
    /// 解密只会带来证书错误；关掉则仍解密抓包，只是不经上游）
    pub direct_no_mitm: Option<bool>,
}

/// 单个 host 的路由结论。
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Route {
    /// 直连源站（跳过上游代理）
    Direct,
    /// 经上游代理（未配置上游时等价于直连）
    Upstream,
}

impl Rules {
    /// 是否跳过 MITM 解密（配合 `direct`）。
    pub fn direct_skip_mitm(&self) -> bool {
        self.direct_no_mitm.unwrap_or(true)
    }

    /// 该 host 的路由结论：`proxied` > 用户 `direct` > 内置直连段 > 默认走上游。
    pub fn route(&self, host: &str) -> Route {
        let h = normalize_host(host);
        if h.is_empty() {
            return Route::Upstream;
        }
        if matches_any(&self.proxied, &h) {
            return Route::Upstream;
        }
        if matches_any(&self.direct, &h) || is_builtin_direct(&h) {
            return Route::Direct;
        }
        Route::Upstream
    }

    /// 该 host 实际要用的上游代理：直连则 None。
    pub fn upstream_for(&self, host: &str, configured: Option<Upstream>) -> Option<Upstream> {
        match self.route(host) {
            Route::Direct => None,
            Route::Upstream => configured,
        }
    }

    /// 需要写入系统代理 bypass 列表的条目（只取用户 `direct` 里对 PAC 有意义的写法，
    /// 并剔除被 `proxied` 覆盖的条目）。
    pub fn system_bypass_entries(&self) -> Vec<String> {
        let mut out = Vec::new();
        for e in &self.direct {
            let e = e.trim();
            if e.is_empty() {
                continue;
            }
            let key = normalize_rule_key(e);
            if matches_any(&self.proxied, &key) {
                continue; // 使用代理优先
            }
            let v = e.to_string();
            if !out.contains(&v) {
                out.push(v);
            }
        }
        out
    }
}

/// 归一化 host：小写、去端口、去 IPv6 方括号、去尾部点。
pub fn normalize_host(host: &str) -> String {
    let mut h = host.trim().to_lowercase();
    if let Some(rest) = h.strip_prefix('[') {
        // [::1]:443
        if let Some(idx) = rest.find(']') {
            h = rest[..idx].to_string();
        }
    } else if !h.contains(':') {
        if let Some((host_part, _)) = h.split_once(':') {
            h = host_part.to_string();
        }
    } else if h.matches(':').count() == 1 {
        // 形如 example.com:443（非 IPv6）
        if let Some((host_part, port)) = h.split_once(':') {
            if port.chars().all(|c| c.is_ascii_digit()) {
                h = host_part.to_string();
            }
        }
    }
    h.trim_end_matches('.').to_string()
}

/// 规则键归一化（用于判断 `proxied` 是否覆盖了某条 `direct`）。
fn normalize_rule_key(rule: &str) -> String {
    let r = rule.trim();
    let r = r.strip_prefix("*.").unwrap_or(r);
    let r = r.strip_prefix('.').unwrap_or(r);
    normalize_host(r)
}

/// 列表里是否有条目命中 host。
fn matches_any(list: &[String], host: &str) -> bool {
    list.iter().any(|rule| rule_matches(rule, host))
}

/// 单条规则匹配。
fn rule_matches(rule: &str, host: &str) -> bool {
    let r = rule.trim().to_lowercase();
    if r.is_empty() {
        return false;
    }
    if r == "*" {
        return true;
    }
    // CIDR：仅当 host 是 IP 时才有意义
    if let Some((net, prefix)) = r.split_once('/') {
        if let (Ok(ip), Ok(bits)) = (host.parse::<IpAddr>(), prefix.parse::<u8>()) {
            return cidr_match(ip, net, bits);
        }
        return false;
    }
    // 含通配符：glob 匹配（域名或 IP 通配）
    if r.contains('*') {
        return glob_match(&r, host);
    }
    // 域名后缀：`example.com` 命中 example.com 与 *.example.com
    let key = normalize_rule_key(&r);
    if key.is_empty() {
        return false;
    }
    host == key || host.ends_with(&format!(".{}", key))
}

/// 内置直连段：回环、链路本地、私有网段、mDNS。
fn is_builtin_direct(host: &str) -> bool {
    if host == "localhost" || host.ends_with(".local") {
        return true;
    }
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(v4)) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_unspecified()
                || is_cgnat(v4)
        }
        Ok(IpAddr::V6(v6)) => v6.is_loopback() || is_ipv6_private(v6),
        Err(_) => false,
    }
}

/// 100.64.0.0/10（运营商级 NAT / Tailscale 也常用）
fn is_cgnat(v4: Ipv4Addr) -> bool {
    let o = v4.octets();
    o[0] == 100 && (64..128).contains(&o[1])
}

/// fc00::/7（ULA）、fe80::/10（链路本地）
fn is_ipv6_private(v6: Ipv6Addr) -> bool {
    let seg = v6.segments();
    (seg[0] & 0xfe00) == 0xfc00 || (seg[0] & 0xffc0) == 0xfe80
}

/// CIDR 匹配：`net` 可以是 IP 字面量（`10.0.0.0/8`）。
fn cidr_match(ip: IpAddr, net: &str, bits: u8) -> bool {
    match (ip, net.trim().parse::<IpAddr>()) {
        (IpAddr::V4(a), Ok(IpAddr::V4(b))) => {
            if bits > 32 {
                return false;
            }
            let mask = if bits == 0 {
                0u32
            } else {
                u32::MAX << (32 - bits as u32)
            };
            (u32::from(a) & mask) == (u32::from(b) & mask)
        }
        (IpAddr::V6(a), Ok(IpAddr::V6(b))) => {
            if bits > 128 {
                return false;
            }
            let a = u128::from(a);
            let b = u128::from(b);
            let mask = if bits == 0 {
                0u128
            } else {
                u128::MAX << (128 - bits as u32)
            };
            (a & mask) == (b & mask)
        }
        _ => false,
    }
}

/// 简单 glob：`*` 匹配任意字符序列（不含分隔符概念，域名/IP 都用）。
fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    // 标准的双指针回溯匹配
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut mark) = (usize::MAX, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = pi;
            mark = ti;
            pi += 1;
        } else if star != usize::MAX {
            pi = star + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}
