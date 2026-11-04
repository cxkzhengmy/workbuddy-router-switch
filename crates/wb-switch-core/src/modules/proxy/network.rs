//! 网关的监听范围：只给本机，还是给整个局域网。
//!
//! # 为什么默认必须只监听回环
//!
//! 这个网关没有用户体系，唯一的访问控制是 `Authorization: Bearer <API Key>`。
//! 绑 `0.0.0.0` 等于把「用本机账号额度调上游」这件事开放给同网段的每一台设备
//! ——包括咖啡厅 Wi-Fi 里那些你不认识的设备。所以默认必须是 `127.0.0.1`，
//! 开放局域网只能是**用户显式点的开关**，而不是一个隐式默认。
//!
//! 这也是为什么开关要落盘：应用每次启动都会自动拉起网关，若不在配置里记住
//! 用户的意图，重启一次就会悄悄缩回回环（用户以为还能连），或者更糟——
//! 默认放开。两种「自以为记住了」都不如显式存储 + 界面回显可靠。

use serde_json::{json, Value};

/// 网络策略在 `proxy.db` 的 settings 表里，键名固定。
pub const KEY: &str = "network_policy";

/// 是否允许局域网访问。默认 `false`。
pub fn lan_access() -> bool {
    super::store::load_settings(KEY)
        .get("lanAccess")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// 保存开关。只写这一个键，不碰同表里的其它策略。
pub fn save_lan_access(enabled: bool) -> Result<(), String> {
    super::store::save_settings(KEY, &json!({ "lanAccess": enabled }))
}

/// 绑定地址：局域网模式绑全地址，否则只绑回环。
pub fn bind_host(lan: bool) -> &'static str {
    if lan {
        "0.0.0.0"
    } else {
        "127.0.0.1"
    }
}

/// 本机的局域网 IPv4，识别不到时返回 `None`。
///
/// 用「UDP connect 一个外网地址再读 local_addr」这个经典技巧：`connect` 对 UDP
/// 只在内核里选路，不会真的发包，因此不需要外网可达，也不会产生流量。选的
/// 是几个国内可路由的地址，只要有一条默认路由就能拿到出口网卡的地址。
///
/// 刻意过滤回环 / 未指定 / 链路本地：这些地址对「其它设备该连哪里」毫无意义，
/// 显示出来只会让人填错。
pub fn lan_ip() -> Option<String> {
    for target in ["223.5.5.5:53", "114.114.114.114:53", "8.8.8.8:80"] {
        let Ok(socket) = std::net::UdpSocket::bind("0.0.0.0:0") else {
            continue;
        };
        if socket.connect(target).is_err() {
            continue;
        }
        let Ok(addr) = socket.local_addr() else {
            continue;
        };
        if let std::net::IpAddr::V4(ip) = addr.ip() {
            if !ip.is_loopback() && !ip.is_unspecified() && !ip.is_link_local() {
                return Some(ip.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bind_host_follows_switch() {
        assert_eq!(bind_host(false), "127.0.0.1");
        assert_eq!(bind_host(true), "0.0.0.0");
    }

    /// 局域网地址要么识别不出来（没网卡 / 无路由），要么必须是一个
    /// 「别的设备能连」的地址——绝不能把回环地址当成结果返回。
    #[test]
    fn lan_ip_is_never_loopback() {
        match lan_ip() {
            Some(ip) => {
                assert_ne!(ip, "127.0.0.1");
                assert!(!ip.starts_with("127."));
                assert!(!ip.starts_with("0."));
                assert!(!ip.starts_with("169.254."));
            }
            None => {}
        }
    }
}
