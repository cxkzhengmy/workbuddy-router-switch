//! 网关运行配置。
//!
//! 历史背景：这个模块原本负责托管**外置的 python hub**（拉起 wb_proxy.py 子进程、
//! 释放内嵌资源、原地升级、读面板日志）。内置 Rust 网关落地后，那些职责全部消失——
//! 服务跑在应用进程内，没有子进程、没有资源释放、没有 hub 目录。
//!
//! 现在只剩两件事：读写 `~/.wb-switch/gateway.json`，以及端口默认值。
//! 旧配置里的 `hubDir` 字段**保留不删**：用户可能仍想回退到 python 方案，
//! 或者我们哪天要看历史目录，删字段容易造成误判。

use serde_json::{json, Value};
use std::path::PathBuf;

use crate::modules::config::{atomic_write, store_dir};

/// 默认端口。沿用外置 hub 的 8788，下游客户端的 base_url 不用改。
pub const DEFAULT_PORT: u16 = 8788;

fn config_file() -> PathBuf {
    store_dir().join("gateway.json")
}

/// 读原始配置，不填任何默认值。
///
/// 不在这里补默认值是有教训的：老实现为了补 `hubDir` 会去问
/// `default_hub_dir()`，而后者又要读配置，首次运行时直接无限递归。
pub fn load_config_raw() -> Value {
    let text = std::fs::read_to_string(config_file()).unwrap_or_default();
    let cfg: Value = serde_json::from_str(&text).unwrap_or_else(|_| json!({}));
    if cfg.is_object() {
        cfg
    } else {
        json!({})
    }
}

/// 读配置并补齐端口默认值。
pub fn load_config() -> Value {
    let mut cfg = load_config_raw();
    if cfg.get("port").and_then(Value::as_u64).is_none() {
        cfg["port"] = json!(DEFAULT_PORT);
    }
    cfg
}

/// 写配置。先把父目录建出来：首次保存时 `~/.wb-switch` 可能还不存在，
/// 而 `atomic_write` 只做「写临时文件 + rename」，不会替你建目录。
pub fn save_config(cfg: &Value) -> std::io::Result<()> {
    let path = config_file();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(cfg).unwrap_or_default();
    atomic_write(&path, &content)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fills_default_port() {
        let cfg = load_config();
        assert!(cfg.get("port").and_then(Value::as_u64).is_some());
    }
}
