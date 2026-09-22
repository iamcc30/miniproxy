//! 持久化用户配置：~/.miniproxy/config.json
//!
//! 目前存放「上游级联」设置。字段用 Option 表达三态：
//! - `null`：从未设置过（允许启动时自动探测本机代理）
//! - `true` + addr：用户显式启用某个上游（启动时沿用）
//! - `false`：用户显式关闭（启动时不再自动探测，尊重用户选择）

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Config {
    /// 是否启用上游级联（None = 未设置）
    #[serde(default)]
    pub upstream_enabled: Option<bool>,
    /// 上游地址，形如 "127.0.0.1:7890"
    #[serde(default)]
    pub upstream_addr: Option<String>,
}

pub fn path() -> PathBuf {
    crate::ca::base_dir().join("config.json")
}

pub fn load() -> Config {
    std::fs::read_to_string(path())
        .ok()
        .and_then(|s| serde_json::from_str::<Config>(&s).ok())
        .unwrap_or_default()
}

pub fn save(cfg: &Config) -> std::io::Result<()> {
    let dir = crate::ca::base_dir();
    std::fs::create_dir_all(&dir)?;
    let s = serde_json::to_string_pretty(cfg).unwrap_or_else(|_| "{}".to_string());
    std::fs::write(path(), s)
}
