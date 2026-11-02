//! 量化关联会话页的扫描耗时。
//!
//! 跑法：cargo run -p wb-switch-core --example session_groups_bench --release

use std::time::Instant;
use wb_switch_core::modules::session_groups::{list, SessionClient};

fn main() {
    for (label, client) in [
        ("workbuddy", SessionClient::Workbuddy),
        ("codebuddyIde", SessionClient::CodebuddyIde),
        ("vscodeExt", SessionClient::VscodeExt),
    ] {
        let t = Instant::now();
        let v = list(client, None);
        let el = t.elapsed().as_secs_f64();
        let groups = v.as_ref().map(|x| x["groups"].as_array().map(Vec::len).unwrap_or(0)).unwrap_or(0);
        let status = v.as_ref().map(|x| x["storeStatus"].as_str().unwrap_or("?").to_string()).unwrap_or_else(|e| format!("ERR:{e}"));
        println!("  {label:14} groups={groups:4} store={status:10} {:.0}ms", el * 1000.0);

        // 顺序与内容稳定性：再跑一次必须完全一致（并行不能打乱顺序）
        if let Ok(first) = &v {
            let t2 = Instant::now();
            if let Ok(second) = list(client, None) {
                let same = first == &second;
                println!(
                    "      再次 {:.0}ms，结果一致={same}，首组={:?}",
                    t2.elapsed().as_secs_f64() * 1000.0,
                    second["groups"][0]["title"].as_str().unwrap_or("-")
                );
            }
        }
    }
}
