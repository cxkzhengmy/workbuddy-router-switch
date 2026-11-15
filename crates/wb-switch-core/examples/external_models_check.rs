//! 验证外部上游（Trae 反代）的连通性。
//!
//! 这是这条通道的实机探针：它不登录也能跑，会告诉你现在卡在哪一步——
//! 没登录、登录了但令牌过期、还是目录拉不到。
//!
//! ```text
//! cargo run -p wb-switch-core --example external_models_check
//! ```
//!
//! 没登录时它会打印授权页地址。把那个地址贴到浏览器里完成登录，回调会落到
//! 本机的随机端口上——但那是另一个进程起的监听，本进程收不到，所以登录请在
//! 应用界面里做，这里再跑一次就能看到目录。
use wb_switch_core::modules::proxy;

#[tokio::main]
async fn main() {
    let view = proxy::external_config();
    println!("配置: {}", serde_json::to_string(&view).unwrap_or_default());

    let status = proxy::external_status().await;
    println!(
        "状态: ok={} signedIn={} realm={} 剩余={:?} 分钟",
        status.get("ok").and_then(|v| v.as_bool()).unwrap_or(false),
        status.get("signedIn").and_then(|v| v.as_bool()).unwrap_or(false),
        status.get("realm").and_then(|v| v.as_str()).unwrap_or("?"),
        status.get("expiresInMin"),
    );
    println!("说明: {}", status.get("message").and_then(|v| v.as_str()).unwrap_or("?"));
    if let Some(device) = status.get("deviceId").and_then(|v| v.as_str()).filter(|d| !d.is_empty()) {
        println!("设备: {device}");
    }

    if status.get("signedIn").and_then(|v| v.as_bool()) != Some(true) {
        // 只打印地址，不真起登录：本进程退出后监听也就没了，收不到回调。
        match proxy::external_sign_in("trae-cn").await {
            Ok(v) => println!(
                "未登录。授权页地址（请在应用界面里登录，不要在这里开）：\n  {}",
                v.get("url").and_then(|u| u.as_str()).unwrap_or("?")
            ),
            Err(e) => println!("✗ 生成授权页地址失败: {e}"),
        }
        return;
    }

    let res = proxy::external_models().await;
    let ok = res.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
    if ok {
        let list = res.get("models").and_then(|v| v.as_array()).cloned().unwrap_or_default();
        println!("✓ 拉到 {} 个模型", list.len());
        for m in list.iter().take(20) {
            println!("   {}", m.as_str().unwrap_or("?"));
        }
    } else {
        println!("✗ 失败: {}", res.get("error").and_then(|v| v.as_str()).unwrap_or("?"));
    }

    // 额度（积分）：这是 Trae 账号自己的额度，不是 WorkBuddy 的积分。
    match proxy::external_entitlement().await {
        Ok(ent) => {
            if ent.get("ok").and_then(|v| v.as_bool()) != Some(true) {
                println!(
                    "额度: 拉不到 —— {}",
                    ent.get("message").and_then(|v| v.as_str()).unwrap_or("?")
                );
            } else {
                let f = |k: &str| ent.get(k).and_then(|v| v.as_f64()).unwrap_or(0.0);
                let unlimited = ent.get("unlimited").and_then(|v| v.as_bool()).unwrap_or(false);
                if unlimited {
                    println!("额度: 余额 {:.1}（账号含不限额度包）", f("left"));
                } else {
                    println!(
                        "额度: 剩 {:.1} / {:.1} 积分，已用 {:.1}（{:.0}%）",
                        f("left"),
                        f("total"),
                        f("used"),
                        f("ratio") * 100.0
                    );
                }
                if let Some(ms) = ent.get("expiresAt").and_then(|v| v.as_i64()) {
                    let when = chrono::DateTime::from_timestamp_millis(ms)
                        .map(|d| d.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string())
                        .unwrap_or_else(|| ms.to_string());
                    println!("  最早到期 {when}，届时 {:.1} 分失效", f("lapsing"));
                }
                for p in ent.get("packs").and_then(|v| v.as_array()).into_iter().flatten() {
                    println!(
                        "  · {} {} 上限={:?} 已用={:.1}",
                        p.get("label").and_then(|v| v.as_str()).unwrap_or("?"),
                        p.get("group").and_then(|v| v.as_str()).unwrap_or(""),
                        p.get("limit"),
                        p.get("used").and_then(|v| v.as_f64()).unwrap_or(0.0),
                    );
                }
            }
        }
        Err(e) => println!("额度: 查询失败 —— {e}"),
    }
}
