//! 账号调度自检：验证三种模式与轮询顺序，不发上游请求、不消耗积分。
//!
//! 跑：`cargo run -p wb-switch-core --example proxy_policy_check --offline`

use serde_json::json;
use wb_switch_core::modules::proxy;

fn main() {
    let view = proxy::account_config();
    println!("当前策略：{}", view["policy"]);
    let accounts = view["accounts"].as_array().cloned().unwrap_or_default();
    println!("账号库：{} 个", accounts.len());
    for acc in &accounts {
        println!(
            "  {:<16} variant={} usable={} current={} today={}",
            acc["name"].as_str().unwrap_or("-"),
            acc["variant"].as_str().unwrap_or("-"),
            acc["usable"].as_bool().unwrap_or(false),
            acc["isCurrent"].as_bool().unwrap_or(false),
            acc["todayRequests"].as_i64().unwrap_or(0),
        );
    }

    let usable: Vec<String> = accounts
        .iter()
        .filter(|a| a["usable"].as_bool().unwrap_or(false))
        .filter_map(|a| a["ref"].as_str().map(str::to_string))
        .collect();

    // 1) current 模式
    proxy::save_account_config(&json!({"mode": "current"})).expect("save current");
    let cands = proxy::account::resolve_candidates().expect("resolve current");
    println!(
        "\n[current] 候选 {} 个 → {:?}",
        cands.len(),
        cands.iter().map(|a| a.label.clone()).collect::<Vec<_>>()
    );

    // 2) fixed 模式（选第一个可用账号）
    if let Some(first) = usable.first() {
        proxy::save_account_config(&json!({"mode": "fixed", "fixed": first}))
            .expect("save fixed");
        let cands = proxy::account::resolve_candidates().expect("resolve fixed");
        println!(
            "[fixed]   候选 {} 个 → {:?}",
            cands.len(),
            cands.iter().map(|a| a.label.clone()).collect::<Vec<_>>()
        );
    }

    // 3) pool 模式：连调三次看轮询是否真的轮转
    if usable.len() >= 2 {
        let pool: Vec<String> = usable.iter().take(3).cloned().collect();
        proxy::save_account_config(&json!({
            "mode": "pool",
            "pool": pool,
            "strategy": "round_robin",
            "failover": true,
        }))
        .expect("save pool");
        println!("\n[pool/round_robin] 池大小 {}", pool.len());
        for round in 1..=3 {
            let cands = proxy::account::resolve_candidates().expect("resolve pool");
            println!(
                "  第 {round} 次候选顺序 → {:?}",
                cands.iter().map(|a| a.label.clone()).collect::<Vec<_>>()
            );
        }

        // 4) 最少请求优先：应该按今日请求数升序
        proxy::save_account_config(&json!({
            "mode": "pool",
            "pool": pool,
            "strategy": "least_requests",
            "failover": true,
        }))
        .expect("save least");
        let cands = proxy::account::resolve_candidates().expect("resolve least");
        println!(
            "\n[pool/least_requests] → {:?}",
            cands.iter().map(|a| a.label.clone()).collect::<Vec<_>>()
        );
    } else {
        println!("\n可用账号不足 2 个，跳过池模式验证");
    }

    // 收尾：恢复默认，别把自检的选择留在用户配置里。
    proxy::save_account_config(&json!({"mode": "current"})).expect("restore current");
    println!("\n已恢复 current 模式");
}
