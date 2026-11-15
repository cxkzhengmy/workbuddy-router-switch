//! 外部模型：Trae 反代的门面。
//!
//! 真正的协议翻译在 [`super::trae`]，这里只负责网关这一侧该做的事：配置读写、
//! 模型列表、把请求交给引擎、把引擎的返回喂给调用方。
//!
//! 1.0.14 之前这条通道是本机 doubao2api（标准 OpenAI 接口，原样透传）。换成
//! Trae 之后语义完全不同：上游是 IDE 私有协议，必须双向翻译，所以 `forward`
//! 不再返回 `reqwest::Response`，而是返回一个已经转成 OpenAI 格式的 [`Reply`]。
//!
//! 只接聊天：Trae 这条通道里没有图片 / 视频 / 音乐生成能力，生成类请求不存在
//! 可以代理的对象，所以也不需要像豆包那样额外拦截。

use serde_json::Value;

use crate::modules::proxy::trae;

/// 引擎返回的响应形态，网关侧要按它决定怎么交给客户端。
pub use crate::modules::proxy::trae::Reply;

/// 模型前缀，同时也是对客户端暴露时的命名空间。
pub const PREFIX: &str = trae::PREFIX;

/// 给界面看的视图。
pub fn view() -> Value {
    trae::view()
}

/// 保存配置。
pub fn save(input: &Value) -> Result<Value, String> {
    trae::save(input)
}

/// 开始浏览器登录，返回要用户打开的授权页地址。
pub async fn sign_in_start(realm: &str) -> Result<Value, String> {
    trae::sign_in_start(realm).await
}

/// 登录进度，界面按 1 秒轮询。
pub fn sign_in_state() -> Value {
    trae::sign_in_state()
}

/// 登出：清掉账号与设备，配置里的开关与勾选保留。
pub fn sign_out() -> Value {
    trae::sign_out()
}

/// 可用的模型列表（带前缀），供 `/v1/models` 合并。
///
/// 走配置里勾选的那份——它由用户在页面上从实时目录里挑，所以这里不需要联网，
/// 网关的 `/v1/models` 才能保持毫秒级返回。
pub fn models_for_listing() -> Vec<Value> {
    let cfg = trae::config();
    if !cfg.enabled {
        return Vec::new();
    }
    cfg.models
        .iter()
        .map(|id| {
            serde_json::json!({
                "id": format!("{PREFIX}{id}"),
                "name": id,
                "object": "model",
                "owned_by": "trae",
            })
        })
        .collect()
}

/// 拉一次账号自己的模型目录（不带前缀），供页面勾选。
pub async fn fetch_models() -> Result<Vec<String>, String> {
    let models = trae::fetch_models().await?;
    Ok(models.into_iter().map(|m| m.id).collect())
}

/// 账号状态：登录没登录、令牌还多久、用的是哪个部署。
///
/// 这里的「健康」和豆包那版不是一回事。豆包是独立进程，要问它活没活；Trae 是
/// 我们自己在跑，能失败的地方只有「没登录」和「令牌过期了」。
pub async fn status() -> Value {
    let cfg = trae::config();
    let view = trae::view();
    let acc = trae::account();
    let now = chrono::Utc::now().timestamp_millis();
    let Some(acc) = acc else {
        return serde_json::json!({
            "ok": false,
            "signedIn": false,
            "realm": cfg.realm,
            "expiresInMin": Value::Null,
            "models": Value::Null,
            "message": "还没有登录 Trae。点「登录」会打开官方授权页，登录完自动回到这里。",
        });
    };
    let left_min = ((acc.expires - now) / 60_000).max(0);
    let catalog = trae::fetch_models().await;
    let (count, message) = match &catalog {
        Ok(models) => (Value::from(models.len()), format!("已登录，目录里有 {} 个模型", models.len())),
        Err(e) => (Value::Null, format!("已登录，但目录没拉到：{e}")),
    };
    serde_json::json!({
        "ok": true,
        "signedIn": true,
        "realm": acc.realm,
        "name": if acc.name.is_empty() { acc.uid.clone() } else { acc.name.clone() },
        "uid": acc.uid,
        "deviceId": acc.device_id,
        "expiresAt": acc.expires,
        "expiresInMin": left_min,
        "models": count,
        "viewPrefix": view.get("prefix").cloned().unwrap_or(Value::Null),
        "message": message,
    })
}

/// 账号的额度（积分）用量。
///
/// 这是 **Trae 账号自己的额度**，与 WorkBuddy 的积分是两套：走 Trae 通道花的是
/// 这个额度，不扣 WorkBuddy 的 credit，所以「积分计费」页里看不到这部分消耗。
/// 界面上要把它和 WorkBuddy 的积分分开说，别混成一个数。
pub async fn entitlement() -> Result<Value, String> {
    if trae::account().is_none() {
        return Ok(serde_json::json!({
            "ok": false,
            "message": "还没有登录 Trae，登录后这里会显示账号的剩余额度。",
        }));
    }
    trae::entitlement().await
}

/// 转发一次对话。`model` 已经去掉前缀。
///
/// 返回的 [`Reply`] 已经是 OpenAI 的格式：流式是一串 `chat.completion.chunk`，
/// 非流式是一个完整的 `chat.completion`。网关那一侧只负责把它交给客户端，
/// 顺手把 usage 记进台账。
pub async fn forward(body: &Value, model: &str, stream: bool) -> Result<Reply, (u16, String)> {
    let cfg = trae::config();
    if !cfg.enabled {
        return Err((404, "外部模型未启用".to_string()));
    }
    if !cfg.models.iter().any(|id| id == model) {
        return Err((403, format!("模型 {model} 不在已启用的 Trae 模型列表里")));
    }
    trae::forward(body, model, stream).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_is_traes_namespace() {
        assert_eq!(PREFIX, "trae/");
    }

    #[test]
    fn listing_is_empty_until_enabled() {
        // 默认配置是关的。这条只断言「没启用就不该出现在 /v1/models」，
        // 不碰真实配置，避免把开发机上的状态写进断言。
        let listed = models_for_listing();
        for item in listed {
            let id = item.get("id").and_then(Value::as_str).unwrap_or("");
            assert!(id.starts_with(PREFIX), "未加前缀的模型混进了列表：{id}");
        }
    }
}
