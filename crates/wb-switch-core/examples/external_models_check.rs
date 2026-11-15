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
}
