//! 验证外部上游的模型目录能不能拉下来。
use wb_switch_core::modules::proxy;

#[tokio::main]
async fn main() {
    let view = proxy::external_config();
    println!("配置: {}", serde_json::to_string(&view).unwrap_or_default());
    let res = proxy::external_models().await;
    let ok = res.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
    if ok {
        let list = res.get("models").and_then(|v| v.as_array()).cloned().unwrap_or_default();
        println!("✓ 拉到 {} 个模型", list.len());
        for m in list.iter().take(12) {
            println!("   {}", m.as_str().unwrap_or("?"));
        }
    } else {
        println!("✗ 失败: {}", res.get("error").and_then(|v| v.as_str()).unwrap_or("?"));
    }
}
