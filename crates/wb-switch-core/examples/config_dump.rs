//! 抓一份原始 `/v3/config`，逐条打印模型特征，用来决定「哪些不该出现在
//! 客户端的模型列表里」。
//!
//! 跑：`cargo run -p wb-switch-core --example config_dump --offline`

use serde_json::Value;
use wb_switch_core::modules::proxy::account;
use wb_switch_core::modules::proxy::upstream::{self, Identity};

fn col(v: bool) -> &'static str {
    if v { "Y" } else { "." }
}

#[tokio::main]
async fn main() {
    let acc = match account::resolve() {
        Ok(a) => a,
        Err(err) => { println!("取不到账号：{err}"); return; }
    };
    let (base, _) = upstream::endpoint_for(acc.variant, Identity::Desktop);
    let mut headers = upstream::identity_headers(&acc, Identity::Desktop, None, None);
    headers.insert("Accept".into(), "application/json, text/plain, */*".into());

    let mut req = upstream::client().get(format!("{base}{}", upstream::CONFIG_PATH));
    for (k, v) in headers { req = req.header(k, v); }
    let payload: Value = match req.send().await {
        Ok(resp) if resp.status().is_success() => match resp.json().await {
            Ok(v) => v,
            Err(err) => { println!("不是 JSON：{err}"); return; }
        },
        Ok(resp) => { println!("HTTP {}", resp.status()); return; }
        Err(err) => { println!("请求失败：{err}"); return; }
    };

    let root = payload.get("data").unwrap_or(&payload);
    let models = match root.get("models").and_then(Value::as_array) {
        Some(list) => list,
        None => { println!("没有顶层 models"); return; }
    };

    println!("{:<30} {:>7} {:>4} {:>4} {:>4} {:>5}  tags", "id", "input", "vid", "img", "tool", "ctx");
    for m in models {
        let id = m.get("id").and_then(Value::as_str).unwrap_or("?");
        let input = m.get("maxInputTokens").and_then(Value::as_i64).unwrap_or(0);
        let tags: Vec<&str> = m.get("tags").and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
        println!("{:<30} {:>7} {:>4} {:>4} {:>4} {:>5}  {:?}",
            id,
            if input > 0 { input.to_string() } else { "-".into() },
            col(m.get("videoGeneration").is_some()),
            col(m.get("imageGeneration").is_some()),
            col(m.get("supportsToolCall").and_then(Value::as_bool).unwrap_or(false)),
            col(m.get("contextWindow").is_some()),
            tags);
    }

    let parsed = upstream::parse_catalog(&payload);
    println!("\nparse_catalog: {} 个，含 kimi-k3.1-pro={}", parsed.len(),
        parsed.iter().any(|m| m == "kimi-k3.1-pro"));
    if let Some(tags) = root.get("modelTags") {
        println!("\nmodelTags: {}", serde_json::to_string(tags).unwrap_or_default().chars().take(700).collect::<String>());
    }
}
