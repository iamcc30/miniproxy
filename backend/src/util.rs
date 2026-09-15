//! 通用小工具：PEM 解码、hex、authority 拆分、头部提取等。

use base64::Engine;
use hyper::HeaderMap;

/// 极简 PEM -> DER（base64 解码正文）。
pub fn pem_to_der(pem: &str) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let body: String = pem
        .lines()
        .filter(|l| !l.starts_with("-----") && !l.trim().is_empty())
        .collect();
    let der = base64::engine::general_purpose::STANDARD.decode(body.trim())?;
    Ok(der)
}

pub fn hex_preview(data: &[u8]) -> String {
    const CHARS: &[u8] = b"0123456789abcdef";
    let mut out = String::new();
    for (i, b) in data.iter().take(256).enumerate() {
        if i > 0 && i % 16 == 0 {
            out.push('\n');
        } else if i > 0 {
            out.push(' ');
        }
        out.push(CHARS[(b >> 4) as usize] as char);
        out.push(CHARS[(b & 0x0f) as usize] as char);
    }
    if data.len() > 256 {
        out.push_str("\n... (仅显示前 256 字节)");
    }
    out
}

/// "host:port" / "host" -> (host, port)
pub fn split_authority(authority: &str, default_port: u16) -> (String, u16) {
    if let Some(rest) = authority.strip_prefix('[') {
        // IPv6 [::1]:8080
        if let Some(end) = rest.find(']') {
            let host = rest[..end].to_string();
            let port = rest[end + 1..]
                .strip_prefix(':')
                .and_then(|p| p.parse().ok())
                .unwrap_or(default_port);
            return (host, port);
        }
    }
    match authority.rsplit_once(':') {
        Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty() => {
            (h.to_string(), p.parse().unwrap_or(default_port))
        }
        _ => (authority.to_string(), default_port),
    }
}

pub fn header_pairs(headers: &HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .filter_map(|(k, v)| {
            let v = v.to_str().ok()?.to_string();
            Some((k.as_str().to_string(), v))
        })
        .collect()
}

/// 判断是否为 HTTP Upgrade 请求（WebSocket）。
pub fn is_upgrade_request(method: &hyper::Method, headers: &HeaderMap) -> bool {
    method == hyper::Method::GET
        && headers.contains_key(hyper::header::UPGRADE)
        && headers
            .get(hyper::header::CONNECTION)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.to_ascii_lowercase().contains("upgrade"))
            .unwrap_or(false)
}
