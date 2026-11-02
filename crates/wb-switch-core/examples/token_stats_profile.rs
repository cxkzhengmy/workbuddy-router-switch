//! 定位 Token 统计页「加载很久」到底慢在哪。
//!
//! 分别测：冷调用耗时、命中缓存耗时、返回 JSON 体积、各字段体积占比。
//!
//! 跑：`cargo run -p wb-switch-core --example token_stats_profile --release --offline`

use std::time::Instant;

fn main() {
    println!("当前数据指纹: {}", wb_switch_core::modules::token_stats::data_fingerprint());
    if let Ok(text) = std::fs::read_to_string(
        wb_switch_core::modules::config::store_dir().join("token_stats_cache.json"),
    ) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
            println!("磁盘缓存指纹: {}", v.get("fingerprint").cloned().unwrap_or_default());
        }
    }

    println!("=== 冷调用 ===");
    let t = Instant::now();
    let first = wb_switch_core::modules::token_stats::get_statistics(None);
    let cold = t.elapsed();
    println!("  耗时 {:.0} ms", cold.as_secs_f64() * 1000.0);

    println!("=== 二次调用（应命中缓存）===");
    let t = Instant::now();
    let second = wb_switch_core::modules::token_stats::get_statistics(None);
    let warm = t.elapsed();
    println!("  耗时 {:.1} ms", warm.as_secs_f64() * 1000.0);

    let bytes = serde_json::to_vec(&*first).map(|v| v.len()).unwrap_or(0);
    println!("=== 返回体积 ===");
    println!("  JSON {:.1} MB", bytes as f64 / 1_048_576.0);
    println!("  Arc 指针相同（缓存命中）: {}", std::sync::Arc::ptr_eq(&first, &second));

    println!("=== 各来源字段体积占比 ===");
    if let Some(sources) = first.get("sources").and_then(|v| v.as_array()) {
        for source in sources {
            let name = source.get("source").and_then(|v| v.as_str()).unwrap_or("?");
            let total: usize = serde_json::to_vec(source).map(|v| v.len()).unwrap_or(0);
            println!("  [{name}] 合计 {:.2} MB", total as f64 / 1_048_576.0);
            if let Some(obj) = source.as_object() {
                let mut parts: Vec<(String, usize)> = obj
                    .iter()
                    .map(|(k, v)| {
                        (
                            k.clone(),
                            serde_json::to_vec(v).map(|b| b.len()).unwrap_or(0),
                        )
                    })
                    .collect();
                parts.sort_by_key(|(_, size)| std::cmp::Reverse(*size));
                for (key, size) in parts.iter().take(8) {
                    if *size < 4096 {
                        continue;
                    }
                    println!(
                        "      {:<16} {:>8.2} MB  ({:.0}%)",
                        key,
                        *size as f64 / 1_048_576.0,
                        *size as f64 / total.max(1) as f64 * 100.0
                    );
                }
            }
        }
    }
}
