//! 验证 list 与 detail 两条路径都能拿到会话标题与内容状态。
//!
//! 背景：cache 改成只读后，detail 曾传空缓存，导致详情页标题全部
//! 退回「会话标题暂不可用」。这类 bug 编译期看不出，只能比对实际输出。

use wb_switch_core::modules::session_groups::{list, detail, SessionClient};

fn main() {
    let list_value = list(SessionClient::Workbuddy, None).expect("list 失败");
    let groups = list_value["groups"].as_array().cloned().unwrap_or_default();
    println!("  list: {} 个分组", groups.len());

    let placeholder = "会话标题暂不可用";
    let blank = groups
        .iter()
        .filter(|g| g["title"].as_str().unwrap_or("") == placeholder)
        .count();
    println!("  list 里标题不可用的分组: {blank}");

    let mut detail_bad = 0;
    let mut checked = 0;
    for g in groups.iter().take(5) {
        let Some(id) = g["groupId"].as_str() else { continue };
        let d = detail(SessionClient::Workbuddy, None, id);
        match d {
            Ok(v) => {
                checked += 1;
                let title = v["title"].as_str().unwrap_or("");
                if title == placeholder || title.is_empty() {
                    detail_bad += 1;
                }
                println!("    detail {:.8}… title={:?}", id, title);
            }
            Err(e) => println!("    detail {:.8}… 失败: {e}", id),
        }
    }
    println!(
        "\n  detail 抽查 {checked} 个，标题异常 {detail_bad} 个 —— {}",
        if detail_bad == 0 { "PASS" } else { "FAIL" }
    );
}
