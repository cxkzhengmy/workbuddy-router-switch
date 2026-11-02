//! 拆解：单次 list_sessions_for_user 的耗时与条数。
//!
//! 关联会话页对每个 (档位, uid) 调一次这个函数（已缓存），
//! 它内部对**每条会话**调 find_project_jsonl 检查 hasHistory。

use std::time::Instant;
use wb_switch_core::modules::session;
use wb_switch_core::modules::variant::WbVariant;

fn main() {
    let uids = [
        "0eed278f-193c-4755-bed4-67b034e0e604",
        "1f94a793-bb37-4cb0-9e97-7412bcfddfc0",
        "2cdb9855-4a4e-4910-9a5a-33b5b59b8e11",
        "389b6f68-64c5-48e2-b904-5abf1236ee3c",
        "7f8c244a-131a-4794-ac79-4741e71194c6",
    ];
    let mut total = 0.0;
    for uid in uids {
        let t = Instant::now();
        let v = session::list_sessions_for_user(WbVariant::Cn, uid);
        let el = t.elapsed().as_secs_f64();
        let n = v.as_array().map(Vec::len).unwrap_or(0);
        total += el;
        println!("  {}… {} 条会话  {:.0}ms", &uid[..8], n, el * 1000.0);
    }
    println!("\n  合计 {:.0}ms", total * 1000.0);
}
