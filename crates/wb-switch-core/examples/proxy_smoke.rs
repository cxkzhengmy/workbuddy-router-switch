//! 反代冒烟：起服务 → 用一个临时 Key 打一次真实上游。
//!
//! 只验证三件事：身份头被上游接受（200）、usage 能解析出来、台账落盘。
//! 请求体刻意压到 `max_tokens: 16`，把积分消耗控制在可忽略的量级。
//!
//! 跑：`cargo run -p wb-switch-core --example proxy_smoke --offline`

use serde_json::json;
use wb_switch_core::modules::proxy;

#[tokio::main]
async fn main() {
    let acc = match proxy::account::resolve() {
        Ok(a) => a,
        Err(err) => {
            println!("✗ 取不到当前账号：{err}");
            return;
        }
    };
    println!(
        "账号：{}  档位：{}  凭据长度：{}",
        acc.label,
        acc.variant.as_str(),
        acc.token.len()
    );

    let models = proxy::models(false).await;
    let list = models
        .get("models")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    println!("模型目录：{} 个", list.len());
    for m in list.iter().take(5) {
        println!("  - {}", m.get("id").and_then(|v| v.as_str()).unwrap_or("?"));
    }

    let picked = std::env::var("SMOKE_MODEL")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| {
            list.iter()
                .filter_map(|m| m.get("id").and_then(|v| v.as_str()))
                .find(|id| *id == "deepseek-v4.1-flash")
                .map(str::to_string)
        })
        .or_else(|| {
            list.iter()
                .filter_map(|m| m.get("id").and_then(|v| v.as_str()))
                .next()
                .map(str::to_string)
        })
        .unwrap_or_else(|| "deepseek-v4.1-flash".to_string());

    let (_, secret) = match proxy::create_key("smoke", None) {
        Ok(v) => (
            v["id"].as_str().unwrap_or_default().to_string(),
            v["secret"].as_str().unwrap_or_default().to_string(),
        ),
        Err(err) => {
            println!("✗ 建 Key 失败：{err}");
            return;
        }
    };

    if let Err(err) = proxy::start(8799).await {
        println!("✗ 启动失败：{err}");
        return;
    }
    println!("网关已起在 8799，模型 = {picked}");

    let body = json!({
        "model": picked,
        "messages": [{"role": "user", "content": "只回复两个字：收到"}],
        "max_tokens": 16,
        "stream": false,
    });
    let resp = proxy::upstream::client()
        .post("http://127.0.0.1:8799/v1/chat/completions")
        .bearer_auth(&secret)
        .json(&body)
        .send()
        .await;
    match resp {
        Ok(r) => {
            let status = r.status();
            let text = r.text().await.unwrap_or_default();
            println!("HTTP {status}");
            println!("{}", text.chars().take(600).collect::<String>());
        }
        Err(err) => println!("✗ 请求失败：{err}"),
    }

    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    let overview = proxy::overview(7);
    println!("今日汇总：{}", overview["today"]);

    // 流式：上游本身就是 SSE，验证网关是边收边转发而不是攒完再发。
    println!("--- 流式 ---");
    let body = json!({
        "model": picked,
        "messages": [{"role": "user", "content": "数到五，用逗号分隔"}],
        "stream": true,
    });
    match proxy::upstream::client()
        .post("http://127.0.0.1:8799/v1/chat/completions")
        .bearer_auth(&secret)
        .json(&body)
        .send()
        .await
    {
        Ok(resp) => {
            println!("HTTP {}", resp.status());
            let mut stream = resp.bytes_stream();
            let mut lines: Vec<String> = Vec::new();
            while let Some(chunk) = futures_util::StreamExt::next(&mut stream).await {
                let Ok(bytes) = chunk else { break };
                let text = String::from_utf8_lossy(&bytes);
                for line in text.lines() {
                    if line.starts_with("data:") && !line.contains("[DONE]") {
                        lines.push(line.chars().take(120).collect());
                    }
                }
                if lines.len() >= 4 {
                    break;
                }
            }
            for line in lines.iter().take(4) {
                println!("  {line}");
            }
            println!("  （共收到 {} 个数据帧以上）", lines.len());
        }
        Err(err) => println!("✗ 流式请求失败：{err}"),
    }

    // 未授权应当 401，验证 Key 门禁没有漏。
    let anon = proxy::upstream::client()
        .post("http://127.0.0.1:8799/v1/chat/completions")
        .json(&json!({"model": picked, "messages": []}))
        .send()
        .await;
    if let Ok(resp) = anon {
        println!("无 Key 请求 → HTTP {}（预期 401）", resp.status());
    }

    // 清掉这次冒烟建的 Key，别把测试数据留在用户的台账里。
    if let Ok(keys) = serde_json::from_value::<Vec<serde_json::Value>>(proxy::list_keys()) {
        for key in keys {
            if key["name"] == "smoke" {
                if let Some(id) = key["id"].as_str() {
                    let _ = proxy::delete_key(id);
                }
            }
        }
    }

    proxy::shutdown();
    println!("已停止");
}
