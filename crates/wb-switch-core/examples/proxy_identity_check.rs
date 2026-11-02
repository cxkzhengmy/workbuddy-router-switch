//! 出站身份自检：分别用 WB / VSC / CLI 三种身份发一次最小请求，看上游是否接受。
//!
//! 三种身份走不同接入点（国内版 VSC 走 www.workbuddy.cn，其余走
//! copilot.tencent.com），头族也不同，必须逐个实测。
//!
//! 跑：`cargo run -p wb-switch-core --example proxy_identity_check --offline`

use serde_json::json;
use wb_switch_core::modules::proxy::upstream::{self, Identity};

#[tokio::main]
async fn main() {
    let acc = match wb_switch_core::modules::proxy::account::resolve() {
        Ok(a) => a,
        Err(err) => {
            println!("✗ 取不到账号：{err}");
            return;
        }
    };
    println!(
        "账号 {} 档位 {} 企业号 {}",
        acc.label,
        acc.variant.as_str(),
        if acc.enterprise_id.is_empty() { "否" } else { "是" }
    );

    for identity in [Identity::Desktop, Identity::Vscode, Identity::Cli] {
        let (base, domain) = upstream::endpoint_for(acc.variant, identity);
        println!(
            "\n--- {} ({}) ---\n  接入点 {base}  X-Domain {domain}",
            identity.label(),
            identity.as_str()
        );
        let body = upstream::build_body(&json!({
            "model": "deepseek-v4.1-flash",
            "messages": [{"role": "user", "content": "只回复两个字：收到"}],
            "max_tokens": 16,
            "stream": true,
        }));
        match upstream::open_chat(&acc, &body, None, identity).await {
            Ok(resp) => {
                let status = resp.status();
                let ok = status.is_success();
                let text = resp.text().await.unwrap_or_default();
                let brief: String = text.chars().take(160).collect();
                println!(
                    "  {} HTTP {status}  首段：{}",
                    if ok { "✓" } else { "✗" },
                    brief.replace('\n', " ")
                );
            }
            Err(err) => println!("  ✗ 连接失败：{err}"),
        }
    }
}
