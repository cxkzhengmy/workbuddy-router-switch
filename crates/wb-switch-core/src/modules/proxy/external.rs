//! 外部 OpenAI 兼容上游（预置 OpenCode）。
//!
//! 与 WorkBuddy 上游的区别：这里**不做任何协议转换**——对端本来就是标准
//! OpenAI 接口，请求体和响应体原样进出，只需要补鉴权头。所以这条路径比
//! `upstream.rs` 单纯得多：没有身份头族、没有 ID 派生、没有非流式聚合。
//!
//! 模型用 `opencode/<model-id>` 前缀与 WorkBuddy 自己的模型区分：两边都有
//! `deepseek-v4.1-flash` 这类同名模型，不加前缀根本没法路由。

use serde_json::{json, Value};

use crate::modules::config::atomic_write;
use crate::modules::proxy::store;

/// 设置键名。结构和「账号策略」一样走 settings 表。
const CONFIG_KEY: &str = "external_opencode";

/// 模型前缀，同时也是对客户端暴露时的命名空间。
pub const PREFIX: &str = "opencode/";

/// OpenCode 官方的 OpenAI 兼容入口。
const DEFAULT_BASE: &str = "https://opencode.ai/inference/openai/v1";

/// 默认 UA。文档说免费模型「require OpenCode」，带上客户端的 UA 至少不会
/// 因为「不像客户端」而被额外拦一道；它不解决额度问题，但也不会有坏处。
const DEFAULT_UA: &str = "opencode/0.15.0";

#[derive(Debug, Clone)]
pub struct ExternalConfig {
    pub enabled: bool,
    pub base_url: String,
    pub api_key: String,
    /// 启用的模型 id（不带前缀）。
    pub models: Vec<String>,
    pub user_agent: String,
}

impl Default for ExternalConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            base_url: DEFAULT_BASE.to_string(),
            api_key: String::new(),
            models: Vec::new(),
            user_agent: DEFAULT_UA.to_string(),
        }
    }
}

pub fn config() -> ExternalConfig {
    let raw = store::load_settings(CONFIG_KEY);
    let default = ExternalConfig::default();
    ExternalConfig {
        enabled: raw.get("enabled").and_then(Value::as_bool).unwrap_or(default.enabled),
        base_url: raw
            .get("baseUrl")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .unwrap_or(DEFAULT_BASE)
            .trim_end_matches('/')
            .to_string(),
        api_key: raw.get("apiKey").and_then(Value::as_str).unwrap_or("").to_string(),
        models: raw
            .get("models")
            .and_then(Value::as_array)
            .map(|list| list.iter().filter_map(Value::as_str).map(str::to_string).collect())
            .unwrap_or_default(),
        user_agent: raw
            .get("userAgent")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .unwrap_or(DEFAULT_UA)
            .to_string(),
    }
}

/// 保存配置。`apiKey` 传空字符串表示「保持原值」——界面上的密码框不会
/// 回显真实 Key，用户不改它时前端就发空，不能因此把 Key 清掉。
pub fn save(input: &Value) -> Result<Value, String> {
    let mut current = config();
    if let Some(enabled) = input.get("enabled").and_then(Value::as_bool) {
        current.enabled = enabled;
    }
    if let Some(base) = input.get("baseUrl").and_then(Value::as_str) {
        let trimmed = base.trim().trim_end_matches('/');
        if !trimmed.is_empty() {
            if !trimmed.starts_with("http") {
                return Err("Base URL 必须以 http(s):// 开头".to_string());
            }
            current.base_url = trimmed.to_string();
        }
    }
    match input.get("apiKey").and_then(Value::as_str) {
        // 前端发 "-" 表示「清空」，发空表示「不改」。区分开是因为
        // 空字符串在表单里同时代表「没动过」和「故意删掉」。
        Some("-") => current.api_key.clear(),
        Some(key) if !key.trim().is_empty() => current.api_key = key.trim().to_string(),
        _ => {}
    }
    if let Some(list) = input.get("models").and_then(Value::as_array) {
        current.models = list.iter().filter_map(Value::as_str).map(str::to_string).collect();
    }
    if let Some(ua) = input.get("userAgent").and_then(Value::as_str) {
        let trimmed = ua.trim();
        if !trimmed.is_empty() {
            current.user_agent = trimmed.to_string();
        }
    }
    if current.enabled && current.models.is_empty() {
        return Err("启用前至少要勾选一个模型".to_string());
    }
    let payload = json!({
        "enabled": current.enabled,
        "baseUrl": current.base_url,
        "apiKey": current.api_key,
        "models": current.models,
        "userAgent": current.user_agent,
    });
    store::save_settings(CONFIG_KEY, &payload)?;
    Ok(view())
}

/// 给界面看的视图：Key 只回「有没有」和末四位，绝不回明文。
pub fn view() -> Value {
    let cfg = config();
    json!({
        "enabled": cfg.enabled,
        "baseUrl": cfg.base_url,
        "hasKey": !cfg.api_key.is_empty(),
        "keyTail": tail_of(&cfg.api_key),
        "models": cfg.models,
        "userAgent": cfg.user_agent,
        "prefix": PREFIX,
    })
}

fn tail_of(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.len() <= 4 {
        return String::new();
    }
    chars[chars.len() - 4..].iter().collect()
}

/// 可用的模型列表（带前缀），供 `/v1/models` 合并。
pub fn models_for_listing() -> Vec<Value> {
    let cfg = config();
    if !cfg.enabled {
        return Vec::new();
    }
    cfg.models
        .iter()
        .map(|id| {
            json!({
                "id": format!("{PREFIX}{id}"),
                "name": id,
                "object": "model",
                "owned_by": "opencode",
            })
        })
        .collect()
}

/// 模型目录的候选地址。
///
/// OpenCode 的两个端点不在同一前缀下：
///   chat   `/inference/openai/v1/chat/completions`
///   models `/inference/v1/models`
/// 按 `{base}/models` 拼会得到 `/openai/v1/models` 并 404。所以这里按
/// 「去掉 /openai」和「去掉 /v1」各补一个候选，逐个试——对方的目录接口
/// 不在同一层这种事，不该让用户自己去猜。
fn model_url_candidates(base: &str) -> Vec<String> {
    let mut out = vec![format!("{base}/models")];
    if let Some(prefix) = base.strip_suffix("/openai/v1") {
        out.push(format!("{prefix}/v1/models"));
    }
    if let Some(prefix) = base.strip_suffix("/v1") {
        let alt = format!("{prefix}/v1/models");
        if !out.contains(&alt) {
            out.push(alt);
        }
    }
    out
}

/// 拉取上游模型目录（不带前缀）。只用于界面上的勾选列表。
pub async fn fetch_models() -> Result<Vec<String>, String> {
    let cfg = config();
    let mut last: String = "没有可用的模型目录地址".to_string();
    for url in model_url_candidates(&cfg.base_url) {
        match fetch_models_at(&url, &cfg).await {
            Ok(models) => return Ok(models),
            Err(err) => last = err,
        }
    }
    Err(last)
}

async fn fetch_models_at(url: &str, cfg: &ExternalConfig) -> Result<Vec<String>, String> {
    let mut req = super::upstream::client().get(url).header("User-Agent", &cfg.user_agent);
    if !cfg.api_key.is_empty() {
        req = req.bearer_auth(&cfg.api_key);
    }
    let resp = req.send().await.map_err(|e| format!("连接 {url} 失败：{e}"))?;
    if !resp.status().is_success() {
        let code = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        return Err(format!(
            "{url} 返回 {code}：{}",
            text.chars().take(200).collect::<String>()
        ));
    }
    let payload: Value = resp.json().await.map_err(|e| format!("{url} 响应不是 JSON：{e}"))?;
    let ids: Vec<String> = payload
        .get("data")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|item| item.get("id").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    if ids.is_empty() {
        return Err(format!("{url} 返回的模型列表为空"));
    }
    Ok(ids)
}

/// 转发一次对话请求。模型 id 已去掉前缀。
///
/// 返回上游的原始响应：流式由调用方直接透传字节，非流式直接透传 JSON。
pub async fn forward(body: &Value, model: &str) -> Result<reqwest::Response, (u16, String)> {
    let cfg = config();
    if !cfg.enabled {
        return Err((404, "外部模型未启用".to_string()));
    }
    if !cfg.models.iter().any(|id| id == model) {
        return Err((403, format!("模型 {model} 不在已启用的外部模型列表里")));
    }
    let mut payload = body.clone();
    if let Some(obj) = payload.as_object_mut() {
        obj.insert("model".into(), json!(model));
    }
    let url = format!("{}/chat/completions", cfg.base_url);
    let mut req = super::upstream::client()
        .post(&url)
        .header("User-Agent", &cfg.user_agent)
        .json(&payload);
    if !cfg.api_key.is_empty() {
        req = req.bearer_auth(&cfg.api_key);
    }
    match req.send().await {
        Ok(resp) => Ok(resp),
        Err(err) => Err((502, format!("连接外部上游失败：{err}"))),
    }
}
