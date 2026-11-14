//! 外部 OpenAI 兼容上游：本机 doubao2api（豆包反代）。
//!
//! 与 WorkBuddy 上游的区别：这里**不做任何协议转换**——对端本来就是标准
//! OpenAI 接口，请求体和响应体原样进出，只需要补鉴权头。所以这条路径比
//! `upstream.rs` 单纯得多：没有身份头族、没有 ID 派生、没有非流式聚合。
//!
//! 模型用 `doubao/<model-id>` 前缀与 WorkBuddy 自己的模型区分，避免同名
//! 模型路由打架。上游是**本机服务**（默认 `http://127.0.0.1:9090/v1`），
//! 不像旧版 OpenCode 通道要过公网免费层的 TLS 指纹 / UA 版本闸 —— 那条路
//! 已经在 1.0.12 判定不可用，1.0.14 起整条通道换成豆包反代。
//!
//! 只接聊天：`/v1/chat/completions` 走网关转发，图片 / 视频 / 音乐生成端点
//! 不在这里代理（视频一次要等 1–3 分钟，超时与缓冲策略都不一样），客户端
//! 要用生成能力请直连 9090。生成类模型因此会被 `save` / `forward` 明确拒绝。

use std::sync::OnceLock;
use std::time::Duration;

use serde_json::{json, Value};

use crate::modules::proxy::store;

/// 设置键名。结构和「账号策略」一样走 settings 表。
///
/// 换了键名而不是复用旧的 `external_opencode`：旧配置指向的是已经不可用的
/// 公网免费层，继承过来只会让界面显示一个永远连不上的地址。
const CONFIG_KEY: &str = "external_doubao";

/// 模型前缀，同时也是对客户端暴露时的命名空间。
pub const PREFIX: &str = "doubao/";

/// doubao2api 默认监听地址（OpenAI 兼容的 `/v1` 根路径）。
const DEFAULT_BASE: &str = "http://127.0.0.1:9090/v1";

/// doubao2api 不看 UA，这个字段只为兼容旧配置形态保留；给一个不会冒充
/// 任何官方客户端的中性值。
const DEFAULT_UA: &str = "wb-switch";

/// 上游内建的对话模型（与 `unified_server.py` 的 `CHAT_MODELS` 一致）。
/// 界面上「一键填入」用这份，省得用户先去拉目录。
pub const CHAT_MODELS: &[&str] = &["doubao", "doubao-pro", "doubao-think", "doubao-expert"];

/// 本机上游专用客户端：**显式禁用系统代理**。
///
/// 不能复用 `upstream::client()`：reqwest 默认读取 `HTTP_PROXY` / `HTTPS_PROXY`，
/// 这台机器常设 `http://127.0.0.1:41637`。本机 127.0.0.1 的请求被代理接管后，
/// `/v1/models` 会拿到代理自己的 404 `{"detail":"Not Found"}`，而 `/health`
/// 恰好还能通，表现为「状态正常但模型拉不到」这种极难定位的假故障。
/// 反代就在本机，绕开代理既正确又少一跳。
fn upstream_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .no_proxy()
            .connect_timeout(Duration::from_secs(5))
            // 豆包视频/长思考可能很慢；聊天流式由网关自己控制超时，这里不设短上限。
            .timeout(Duration::from_secs(600))
            .build()
            .expect("failed to build local upstream client")
    })
}

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

/// 生成类模型：只走 doubao2api 自己的 `/v1/{images,video,audio}/generations`，
/// 网关不代理这些端点。判据用后缀 + 明确的固定 id，避免将来新增模型漏判。
pub fn is_generation_model(id: &str) -> bool {
    let lower = id.to_ascii_lowercase();
    lower.ends_with("-image")
        || lower.ends_with("-video")
        || lower.ends_with("-music")
        || matches!(lower.as_str(), "doubao-image" | "doubao-video" | "doubao-music")
}

/// 去掉 `/v1` 得到服务根地址，用于拼 `/health`、`/admin` 这类不在 `/v1` 下的端点。
fn root_url(base: &str) -> String {
    base.strip_suffix("/v1")
        .unwrap_or(base)
        .trim_end_matches('/')
        .to_string()
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
    if let Some(bad) = current.models.iter().find(|id| is_generation_model(id)) {
        return Err(format!(
            "{bad} 是生成类模型，网关只代理对话请求；请去掉它，或让客户端直连 doubao2api 的生成端点"
        ));
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
    let root = root_url(&cfg.base_url);
    json!({
        "enabled": cfg.enabled,
        "baseUrl": cfg.base_url,
        "hasKey": !cfg.api_key.is_empty(),
        "keyTail": tail_of(&cfg.api_key),
        "models": cfg.models,
        "userAgent": cfg.user_agent,
        "prefix": PREFIX,
        "provider": "doubao",
        "chatModelPreset": CHAT_MODELS,
        // 扫码登录在 Admin 面板里完成。`adminUrl` 是可显示的纯地址，
        // `adminLink` 只用于跳转：上游 /admin 也走 _check_auth，不带 key 会 401。
        // 后者含用户自己本机服务的 Key，界面不把它当文本渲染出来。
        "adminUrl": format!("{root}/admin"),
        "adminLink": admin_link(&root, &cfg.api_key),
    })
}

/// `/admin` 的鉴权走 `?key=`，与 Bearer 等价（见上游 `_check_auth`）。
/// 没配 Key 时直接给纯地址。
fn admin_link(root: &str, key: &str) -> String {
    if key.is_empty() {
        format!("{root}/admin")
    } else {
        format!("{root}/admin?key={}", urlencoded(key))
    }
}

fn urlencoded(text: &str) -> String {
    // Key 是我们自己生成/用户粘贴的 token，只可能出现少量非 ASCII 或保留字符，
    // 用最简单的百分号编码即可，不为此引入新依赖。
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn tail_of(key: &str) -> String {
    let chars: Vec<char> = chars_of(key);
    if chars.len() <= 4 {
        return String::new();
    }
    chars[chars.len() - 4..].iter().collect()
}

fn chars_of(text: &str) -> Vec<char> {
    text.chars().collect()
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
                "owned_by": "doubao",
            })
        })
        .collect()
}

/// 模型目录地址。doubao2api 的 `/v1/models` 就在 `/v1` 下，标准 OpenAI 格式，
/// 不像 OpenCode 那样 chat 与 models 分处不同前缀，所以不需要多候选兜底。
fn model_url(base: &str) -> String {
    format!("{base}/models")
}

/// 拉取上游模型目录（不带前缀）。返回全部 id，由界面把生成类模型标出来；
/// 保存与转发时再拦截，避免「目录里有但网关其实不代理」的误导。
pub async fn fetch_models() -> Result<Vec<String>, String> {
    let cfg = config();
    let url = model_url(&cfg.base_url);
    fetch_models_at(&url, &cfg).await
}

async fn fetch_models_at(url: &str, cfg: &ExternalConfig) -> Result<Vec<String>, String> {
    let mut req = upstream_client()
        .get(url)
        .header("User-Agent", &cfg.user_agent)
        .timeout(Duration::from_secs(10));
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

/// 探测 doubao2api 是否在线、是否已扫码登录、是否卡在风控验证码。
///
/// `/health` 按上游约定无需认证，但带上 Key 也无害。这个状态对豆包反代
/// 特别重要：服务在跑 ≠ 能用，session 过期或需要人工过验证码时聊天会 503。
pub async fn health() -> Value {
    let cfg = config();
    let url = format!("{}/health", root_url(&cfg.base_url));
    let mut req = upstream_client()
        .get(&url)
        .header("User-Agent", &cfg.user_agent)
        .timeout(Duration::from_secs(5));
    if !cfg.api_key.is_empty() {
        req = req.bearer_auth(&cfg.api_key);
    }
    let resp = match req.send().await {
        Ok(resp) => resp,
        Err(err) => {
            return json!({
                "ok": false,
                "reachable": false,
                "loggedIn": Value::Null,
                "needsCaptcha": Value::Null,
                "url": url,
                "message": format!("连不上 {url}：{err}。确认 doubao2api 已启动。"),
            });
        }
    };
    let code = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    let payload: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    let logged_in = payload.get("logged_in").and_then(Value::as_bool);
    let needs_captcha = payload.get("needs_captcha").and_then(Value::as_bool);
    let upstream_status = payload.get("status").and_then(Value::as_str).unwrap_or("");
    let reachable = (200..300).contains(&code);
    let message = if !reachable {
        format!("{url} 返回 {code}")
    } else if needs_captcha == Some(true) {
        "上游命中风控验证码，需要到 Admin 面板处理后再恢复".to_string()
    } else if logged_in == Some(false) {
        "服务在线但尚未登录，请到 Admin 面板扫码登录豆包账号".to_string()
    } else if logged_in == Some(true) {
        format!("服务正常（{upstream_status}），已登录")
    } else {
        format!("服务在线（HTTP {code}），/health 未返回登录状态")
    };
    json!({
        "ok": reachable && logged_in != Some(false) && needs_captcha != Some(true),
        "reachable": reachable,
        "loggedIn": logged_in,
        "needsCaptcha": needs_captcha,
        "statusText": upstream_status,
        "url": url,
        "message": message,
    })
}

/// 转发一次对话请求。模型 id 已去掉前缀。
///
/// 返回上游的原始响应：流式由调用方直接透传字节，非流式直接透传 JSON。
pub async fn forward(body: &Value, model: &str) -> Result<reqwest::Response, (u16, String)> {
    let cfg = config();
    if !cfg.enabled {
        return Err((404, "外部模型未启用".to_string()));
    }
    if is_generation_model(model) {
        return Err((
            400,
            format!("{model} 是生成类模型，网关只代理对话请求；请直连 doubao2api 的生成端点"),
        ));
    }
    if !cfg.models.iter().any(|id| id == model) {
        return Err((403, format!("模型 {model} 不在已启用的豆包模型列表里")));
    }
    let mut payload = body.clone();
    if let Some(obj) = payload.as_object_mut() {
        obj.insert("model".into(), json!(model));
    }
    let url = format!("{}/chat/completions", cfg.base_url);
    let mut req = upstream_client()
        .post(&url)
        .header("User-Agent", &cfg.user_agent)
        .json(&payload);
    if !cfg.api_key.is_empty() {
        req = req.bearer_auth(&cfg.api_key);
    }
    match req.send().await {
        Ok(resp) => Ok(resp),
        Err(err) => Err((502, format!("连接豆包反代失败：{err}。确认 {url} 可访问"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generation_models_are_detected() {
        assert!(is_generation_model("doubao-image"));
        assert!(is_generation_model("doubao-video"));
        assert!(is_generation_model("doubao-music"));
        assert!(is_generation_model("seedance-video"));
        assert!(!is_generation_model("doubao"));
        assert!(!is_generation_model("doubao-think"));
        assert!(!is_generation_model("qianwen-coder"));
    }

    #[test]
    fn root_url_strips_v1_for_health_and_admin() {
        assert_eq!(root_url("http://127.0.0.1:9090/v1"), "http://127.0.0.1:9090");
        assert_eq!(root_url("http://127.0.0.1:9090"), "http://127.0.0.1:9090");
        assert_eq!(root_url("https://example.com/api/v1"), "https://example.com/api");
    }

    #[test]
    fn chat_preset_contains_only_chat_models() {
        assert_eq!(CHAT_MODELS.len(), 4);
        assert!(CHAT_MODELS.iter().all(|id| !is_generation_model(id)));
    }

    #[test]
    fn admin_link_carries_key_only_when_configured() {
        assert_eq!(
            admin_link("http://127.0.0.1:9090", ""),
            "http://127.0.0.1:9090/admin"
        );
        assert_eq!(
            admin_link("http://127.0.0.1:9090", "sk-a b"),
            "http://127.0.0.1:9090/admin?key=sk-a%20b"
        );
    }

    /// 实机探针：需要本机真的跑着 doubao2api，且 `external_doubao` 已配置。
    /// 只断言「能连上、鉴权通过、目录非空」，不要求已扫码登录（登录是人工步骤）。
    /// `cargo test -p wb-switch-core probe_real_doubao_service -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn probe_real_doubao_service() {
        let cfg = config();
        println!("base={} hasKey={} enabled={} models={:?}", cfg.base_url, !cfg.api_key.is_empty(), cfg.enabled, cfg.models);
        let h = health().await;
        println!("health={h}");
        assert_eq!(h["reachable"], true, "doubao2api 没起来或 /health 不可达");

        let models = fetch_models().await.expect("模型目录应能拉取（检查 API Key）");
        println!("models={} {:?}", models.len(), models);
        assert!(models.iter().any(|id| id == "doubao"));

        let body = json!({ "messages": [{ "role": "user", "content": "hi" }], "stream": false });
        let resp = forward(&body, "doubao").await.expect("转发应能到达上游");
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        println!("chat status={status} body={}", text.chars().take(300).collect::<String>());
        assert_ne!(status, 401, "鉴权没生效：网关没把 Key 传给上游");
    }
}
