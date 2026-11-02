//! 上游（WorkBuddy / CodeBuddy）协议：身份头、请求体修补、流式开流。
//!
//! 上游就是一个普通的 `POST {base}/v2/chat/completions`，响应是 OpenAI 兼容的
//! SSE。真正的门槛全在**请求头**上：上游靠 `X-Conversation-ID` 家族识别「这像不像
//! 官方客户端」，缺了它直接 400 / code 11-128（conversation id missing）。
//!
//! 这里的头族对照 workbuddy2api-hub 的 `wb_identity.build_identity_headers`
//! 桌面端分支（5.8.4 抓包值）。ID 只要求形态（带连字符 UUID / 32 位 hex），
//! 上游不校验语义，所以每次随机生成即可。

use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Duration;

use serde_json::{json, Value};
use uuid::Uuid;

use super::account::ActiveAccount;
use crate::modules::variant::WbVariant;

/// 上游对话路径。
const CHAT_PATH: &str = "/v2/chat/completions";
/// 模型目录路径（兜底用）。
pub const MODELS_PATH: &str = "/v2/enterprises/personal/models";
/// 桌面端产品配置路径，返回最全的模型清单（含倍率）。
pub const CONFIG_PATH: &str = "/v3/config";

/// 国内版对话侧默认 base。
///
/// 官方桌面端走 `copilot.tencent.com`，但该域名从海外出口连接时 TLS 会被复位；
/// `www.workbuddy.cn` 提供同一套接口且海外可达。与 hub 的 `WB_CN_CHAT_UPSTREAM`
/// 保持同一个变量名，便于沿用既有部署脚本。
const CN_CHAT_BASE_DEFAULT: &str = "https://copilot.tencent.com";

/// 国际版桌面端版本号（2026-10-16 抓包）。
const INTL_VERSION: &str = "5.8.4";
/// 国内版桌面端版本号（2026-10-18 实跑）。
const CN_VERSION: &str = "5.8.3";
/// 客户端内置 CLI 版本，UA 末段。
const DESKTOP_CLI_VERSION: &str = "2.161.0";

// ---- 另外两种出站身份的参数（取自官方 VSCode 扩展 4.12.39997873 解包与
// ---- CodeBuddy CLI 2.168.2 的实际请求）。三者不只是 UA 不同，头族、ID 形态、
// ---- 甚至国内版的接入点都不一样，所以必须整体切换而不是只换一个字段。
const VSCODE_IDE_VERSION: &str = "1.119.0";
const VSCODE_PRODUCT_VERSION: &str = "4.12.39997873";
const VSCODE_PLATFORM: &str = "VSCode";
const VSCODE_PRODUCT_NAME: &str = "CodeBuddy";
const VSCODE_ENV_ID: &str = "production";
const CLI_VERSION: &str = "2.168.2";
const CLI_AGENT_RUNTIME: &str = "other";

/// 出站身份：上游看到的是"哪种官方客户端在请求"。
///
/// 这不是伪装层面的花活——三种身份走不同的接入点、带不同的头族与 ID 形态，
/// 配额通道也可能不同（国内版 VSCode 身份直接连 www.workbuddy.cn，桌面端与
/// CLI 走 copilot.tencent.com）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Identity {
    /// WorkBuddy 独立桌面客户端（默认）。
    #[default]
    Desktop,
    /// 官方 VSCode 插件。
    Vscode,
    /// 官方 CodeBuddy CLI。
    Cli,
}

impl Identity {
    pub fn parse(raw: Option<&str>) -> Self {
        match raw.map(str::trim).unwrap_or_default().to_ascii_lowercase().as_str() {
            "vscode" | "vsc" => Self::Vscode,
            "cli" => Self::Cli,
            _ => Self::Desktop,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Desktop => "workbuddy",
            Self::Vscode => "vscode",
            Self::Cli => "cli",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Desktop => "WB",
            Self::Vscode => "VSC",
            Self::Cli => "CLI",
        }
    }
}

/// 反代专用 HTTP 客户端。
///
/// 与 `config::http_client()` 分开：那个是 30 秒总超时的管理请求客户端，
/// 拿来跑流式对话会在长回答上被拦腰掐断。这里只限制建连与整体上限，
/// 不设 per-read 超时——上游思考久一点是正常现象。
pub fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(600))
            .build()
            .expect("failed to build proxy http client")
    })
}

/// 对话上游的 (base, X-Domain)。
///
/// 国内版 VSCode 身份走 `www.workbuddy.cn`：官方插件在国内站用的就是这个域，
/// 与桌面端/CLI 的 `copilot.tencent.com` 不是同一条通道。国际版三种身份共用
/// `www.workbuddy.ai`。
pub fn endpoint_for(variant: WbVariant, identity: Identity) -> (String, String) {
    match variant {
        WbVariant::Ai => (
            "https://www.workbuddy.ai".to_string(),
            "www.workbuddy.ai".to_string(),
        ),
        WbVariant::Cn => {
            if identity == Identity::Vscode {
                // 仍允许用环境变量覆盖（自建反代 / 本地抓包场景）。
                let base = std::env::var("WB_PROXY_CN_VSCODE_BASE")
                    .ok()
                    .map(|v| v.trim().trim_end_matches('/').to_string())
                    .filter(|v| v.starts_with("http"))
                    .unwrap_or_else(|| "https://www.workbuddy.cn".to_string());
                let domain = base
                    .split("://")
                    .nth(1)
                    .unwrap_or("www.workbuddy.cn")
                    .to_string();
                return (base, domain);
            }
            let base = std::env::var("WB_PROXY_CN_BASE")
                .ok()
                .map(|v| v.trim().trim_end_matches('/').to_string())
                .filter(|v| v.starts_with("http"))
                .unwrap_or_else(|| CN_CHAT_BASE_DEFAULT.to_string());
            let domain = base
                .split("://")
                .nth(1)
                .unwrap_or("copilot.tencent.com")
                .to_string();
            (base, domain)
        }
    }
}

fn desktop_version(variant: WbVariant) -> &'static str {
    match variant {
        WbVariant::Ai => INTL_VERSION,
        WbVariant::Cn => CN_VERSION,
    }
}

fn desktop_ua(variant: WbVariant) -> String {
    let app = desktop_version(variant);
    let product = match variant {
        WbVariant::Ai => "workbuddy-ai",
        WbVariant::Cn => "WorkBuddy",
    };
    format!("WorkBuddy/{app} {product}/{app} CLI/{DESKTOP_CLI_VERSION}")
}

/// 32 位无连字符 hex（上游实测的 request / message id 形态）。
fn hex32() -> String {
    Uuid::new_v4().simple().to_string()
}

/// 带连字符 UUID（会话 id 形态）。
fn uuid_hyphenated() -> String {
    Uuid::new_v4().to_string()
}

/// 一轮对话的身份头。
///
/// `conversation_id` 由调用方按「API key + 会话指纹」稳定传入时，同一会话的
/// 多轮请求会复用同一个 id，出站形态更接近人类客户端；不传则每请求新生成。
pub fn identity_headers(
    acc: &ActiveAccount,
    identity: Identity,
    conversation_id: Option<&str>,
    model: Option<&str>,
) -> HashMap<String, String> {
    let (_, domain) = endpoint_for(acc.variant, identity);
    // 会话 ID 形态按身份区分：桌面端与 CLI 是带连字符的 UUID，VSCode 的
    // BaseAgent 用 uuidv4 去连字符后的 32 位 hex。
    let conv = conversation_id
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| match identity {
            Identity::Vscode => hex32(),
            _ => uuid_hyphenated(),
        });
    let msg_id = hex32();
    let turn_id = hex32();
    let trace = hex32();
    let span = hex32();
    let is_cn = acc.variant == WbVariant::Cn;

    let mut headers: HashMap<String, String> = HashMap::new();
    let mut put = |key: &str, value: String| {
        headers.insert(key.to_string(), value);
    };

    match identity {
        Identity::Desktop => {
            let version = desktop_version(acc.variant);
            put("X-Agent-Intent", "craft".into());
            put("X-Agent-Purpose", "conversation".into());
            put("X-Agent-Type", "main".into());
            put("X-IDE-Name", "WorkBuddy".into());
            put("X-IDE-Type", "WorkBuddy".into());
            put("X-IDE-Version", version.into());
            // 桌面端所有站点都发 true。
            put("X-Private-Data", "true".into());
            put("X-Product", "SaaS".into());
            put("User-Agent", desktop_ua(acc.variant));
            put("X-Request-ID", msg_id.clone());
        }
        Identity::Vscode => {
            put("X-Agent-Intent", "craft".into());
            // 官方扩展的主对话不发 X-Agent-Purpose（只有桌面端/CLI 发）。
            put("X-Agent-Type", "main".into());
            put("X-IDE-Type", VSCODE_PLATFORM.into());
            put("X-IDE-Name", VSCODE_PLATFORM.into());
            put("X-IDE-Version", VSCODE_IDE_VERSION.into());
            put("X-Product-Version", VSCODE_PRODUCT_VERSION.into());
            // HttpEnvRouteInterceptor：非开发者模式固定 production。
            put("X-Env-ID", VSCODE_ENV_ID.into());
            put("X-Product", "SaaS".into());
            put(
                "User-Agent",
                format!(
                    "{VSCODE_PLATFORM}/{VSCODE_IDE_VERSION} {VSCODE_PRODUCT_NAME}/{VSCODE_PRODUCT_VERSION}"
                ),
            );
            put("X-Request-Trace-Id", uuid_hyphenated());
            // 官方 BaseAgent：X-Request-ID 与 X-Conversation-Request-ID 是同一个
            // 本轮 requestId，而不是独立的消息 id。发错会让请求看起来不像插件。
            put("X-Request-ID", turn_id.clone());
            // 只有国际站发 X-Private-Data（国内站由 endpoint 判断不发）。
            if !is_cn {
                put("X-Private-Data", "true".into());
            }
            if let Some(model_id) = model.map(str::trim).filter(|v| !v.is_empty()) {
                let now = chrono::Utc::now().timestamp_millis().to_string();
                put("X-Model-ID", model_id.to_string());
                // 官方只在 model 请求上塞这两个毫秒时间戳。
                put("monitor_promptPrepareStartTime", now.clone());
                put("monitor_httpSendTime", now);
            }
        }
        Identity::Cli => {
            put("X-Agent-Intent", "craft".into());
            put("X-Agent-Purpose", "conversation".into());
            put("X-Agent-Runtime", CLI_AGENT_RUNTIME.into());
            put("X-Agent-Type", "main".into());
            put("X-IDE-Type", "CLI".into());
            put("X-IDE-Name", "CLI".into());
            put("X-IDE-Version", CLI_VERSION.into());
            put("X-Product", "SaaS".into());
            put("User-Agent", format!("CLI/{CLI_VERSION} CodeBuddy/{CLI_VERSION}"));
            // 官方 CLI 按「是否企业号」决定：企业号 true，个人号 false。
            put(
                "X-Private-Data",
                if acc.enterprise_id.is_empty() { "false" } else { "true" }.into(),
            );
            put("X-Request-ID", msg_id.clone());
        }
    }

    // 以下三种身份共有。
    put("X-Domain", domain);
    put("X-User-Id", acc.uid.clone());
    put("Authorization", format!("Bearer {}", acc.token));
    put("X-Conversation-ID", conv);
    put("X-Conversation-Request-ID", turn_id.clone());
    put("X-Conversation-Message-ID", msg_id);
    // 主对话只有桌面端与 CLI 带 Root-Request-ID（官方只在 subagent 上给 VSCode 带）。
    if identity != Identity::Vscode {
        put("X-Root-Request-ID", turn_id);
    }
    put("X-Trace-ID", trace.clone());
    put("traceparent", format!("00-{trace}-{span}-01"));
    put("b3", format!("{trace}-{span}-1"));
    put("X-B3-TraceId", trace);
    put("X-B3-SpanId", span);
    put("X-B3-Sampled", "1".into());
    // 静态 API Key 需要额外带一份，否则 /v3/config 少下发 4 个模型。
    if acc.is_static_key() {
        put("X-API-Key", acc.token.clone());
    }
    if !acc.enterprise_id.is_empty() {
        put("X-Enterprise-Id", acc.enterprise_id.clone());
        put("X-Tenant-Id", acc.enterprise_id.clone());
    }
    headers
}

/// 对客户端 payload 做上游兼容修补。
///
/// 只做「不做就一定炸」的几项，其余原样透传——修补越多，越容易在某个模型上
/// 改坏语义。完整的 tool_call 配对修复（相邻合并 / 结果重排）留待实测到 400
/// 再补。
pub fn build_body(payload: &Value) -> Value {
    let mut body = payload.clone();
    let Some(obj) = body.as_object_mut() else {
        return body;
    };

    // 私有标记字段（Responses 路径用的内部键）不属于上游协议。
    for key in obj.keys().filter(|k| k.starts_with('_')).cloned().collect::<Vec<_>>() {
        obj.remove(&key);
    }

    let model = obj
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    // 上游要求首条是 system。客户端通常自己带，缺了才补。
    let need_system = obj
        .get("messages")
        .and_then(Value::as_array)
        .map(|list| {
            list.first()
                .and_then(|m| m.get("role"))
                .and_then(Value::as_str)
                != Some("system")
        })
        .unwrap_or(false);
    if need_system {
        let mut messages = vec![json!({
            "role": "system",
            "content": "You are a helpful assistant.",
        })];
        if let Some(existing) = obj.get("messages").and_then(Value::as_array) {
            messages.extend(existing.iter().cloned());
        }
        obj.insert("messages".into(), Value::Array(messages));
    }

    // GPT 系上游要求 max_output_tokens >= 16；探测类请求常用 1，会被 400。
    if model.to_ascii_lowercase().starts_with("gpt-") {
        if let Some(mt) = obj.get("max_tokens").and_then(Value::as_i64) {
            if mt < 16 {
                obj.insert("max_tokens".into(), json!(16));
            }
        }
    }

    // deepseek 开思考时，上游要求每条 assistant 消息都带 reasoning_content，
    // 而客户端回传历史时通常把它丢了（只保留 content）。不补就整轮 400。
    if model.to_ascii_lowercase().starts_with("deepseek") && thinking_enabled(obj) {
        if let Some(list) = obj.get_mut("messages").and_then(Value::as_array_mut) {
            for msg in list.iter_mut() {
                let is_assistant =
                    msg.get("role").and_then(Value::as_str) == Some("assistant");
                if !is_assistant {
                    continue;
                }
                if let Some(map) = msg.as_object_mut() {
                    map.entry("reasoning_content")
                        .or_insert_with(|| Value::String(String::new()));
                }
            }
        }
    }

    body
}

fn thinking_enabled(obj: &serde_json::Map<String, Value>) -> bool {
    if let Some(kind) = obj
        .get("thinking")
        .and_then(|t| t.get("type"))
        .and_then(Value::as_str)
    {
        if kind.eq_ignore_ascii_case("enabled") {
            return true;
        }
        if kind.eq_ignore_ascii_case("disabled") {
            return false;
        }
    }
    // 没显式关掉就当开启（与 hub 的判定一致：effort=none 才算关）。
    obj.get("reasoning_effort")
        .or_else(|| obj.get("reasoningEffort"))
        .and_then(Value::as_str)
        .map(|v| !v.eq_ignore_ascii_case("none"))
        .unwrap_or(true)
}

/// 发起对话请求。调用方负责解析 SSE 或读 JSON。
pub async fn open_chat(
    acc: &ActiveAccount,
    body: &Value,
    conversation_id: Option<&str>,
    identity: Identity,
) -> Result<reqwest::Response, String> {
    let (base, _) = endpoint_for(acc.variant, identity);
    // VSCode 身份要在头上带 X-Model-ID，值就是本次请求的 model。
    let model = body.get("model").and_then(Value::as_str);
    let url = format!("{base}{CHAT_PATH}");
    let mut req = client()
        .post(&url)
        .header("Content-Type", "application/json")
        .header("Accept", "text/event-stream, application/json")
        .body(serde_json::to_vec(body).map_err(|e| format!("请求体序列化失败：{e}"))?);
    for (k, v) in identity_headers(acc, identity, conversation_id, model) {
        req = req.header(k, v);
    }
    req.send()
        .await
        .map_err(|e| format!("连接上游失败：{}", describe(&e)))
}

fn describe(err: &reqwest::Error) -> String {
    if err.is_timeout() {
        return "请求超时".to_string();
    }
    if err.is_connect() {
        return "无法建立连接（检查网络或代理）".to_string();
    }
    err.to_string()
}

/// 读上游的模型目录（桌面端 `/v3/config`）。失败返回空列表。
pub async fn fetch_models(acc: &ActiveAccount, identity: Identity) -> Vec<String> {
    let (base, _) = endpoint_for(acc.variant, identity);
    let mut headers = identity_headers(acc, identity, None, None);
    // `/v3/config` 的目录由 UA 决定：只有桌面 UA 返回完整清单。
    headers.insert(
        "Accept".into(),
        "application/json, text/plain, */*".into(),
    );
    headers.insert("Origin".into(), base.clone());
    headers.insert("Referer".into(), format!("{base}/"));
    let mut req = client().get(format!("{base}{CONFIG_PATH}"));
    for (k, v) in headers {
        req = req.header(k, v);
    }
    let Ok(resp) = req.send().await else {
        return Vec::new();
    };
    if !resp.status().is_success() {
        return Vec::new();
    }
    let Ok(payload) = resp.json::<Value>().await else {
        return Vec::new();
    };
    parse_catalog(&payload)
}

/// 从 `/v3/config` 响应里抽出模型 id。
///
/// 两个位置都要看，而且**顶层优先**：
///   - 顶层 `models`：对象数组（`{id, name, descriptionZh, ...}`），最全。
///     实测国内版有 60 条，含 kimi-k3.1-pro / hy3-b / hy3-c 这些新模型。
///   - `agents.<name>.models`：裸字符串数组，只有 16 条，是 agent 自己可用的
///     子集。
///
/// 之前只读后者，导致列表永远停在 16 个——用户看到的「模型不是最新的」
/// 一半是解析漏了，而不是缓存。
pub fn parse_catalog(payload: &Value) -> Vec<String> {
    let mut roots: Vec<&Value> = vec![payload];
    if let Some(data) = payload.get("data") {
        roots.push(data);
    }
    let mut out: Vec<String> = Vec::new();

    for root in &roots {
        collect_chat_model_ids(root.get("models"), &mut out);
    }
    // 顶层没有（老版本响应）才退到 agents。
    if out.is_empty() {
        for root in &roots {
            let agents = match root.get("agents") {
                Some(v) => v,
                None => continue,
            };
            let entries: Vec<&Value> = if let Some(map) = agents.as_object() {
                map.values().collect()
            } else if let Some(list) = agents.as_array() {
                list.iter().collect()
            } else {
                continue;
            };
            for entry in entries {
                collect_model_ids(entry.get("models"), &mut out);
            }
        }
    }
    out
}

/// 这个条目是不是「能拿来对话」的模型。
///
/// `/v3/config` 的顶层清单是全量目录，混着三类非对话模型：
///   - 视频/图像生成：带 `videoGeneration` / `imageGeneration`，或 tags 里
///     有 `text-to-video` 之类（kling-video-v3、hunyuan-image-alpha）
///   - 代码补全（FIM）：没有 `maxInputTokens`（codewise-*、completion-gf）
///   - 内部/实验：同上，缺语言模型的基本字段（wb-lite）
/// 列进模型选择器只会让人选中后拿到一个不能用 chat 接口的模型。
/// 虚拟别名：`auto` / `fast-model` 这类不是真模型，是「让上游按用途自己挑」
/// 的路由入口。对 API 调用方没有意义，却会挤占选择器。
const VIRTUAL_ALIASES: &[&str] = &[
    "auto",
    "default-model",
    "fast-model",
    "balanced-model",
    "primary-model",
    "deep-model",
];

/// 明确不是对话模型的 id 前缀（代码补全 / completion 系列）。
const NON_CHAT_PREFIXES: &[&str] = &["codewise-", "completion-"];

/// 明确不是对话模型的 id 后缀。
const NON_CHAT_SUFFIXES: &[&str] = &["-image-alpha", "-image-alpha-edit", "-taco-completion"];

/// 地区/付费变体后缀：`-sg` 是新加坡节点、`-x` 是同一模型的另一个 build。
/// 列表里已经有基名，留着只是噪音。
const VARIANT_SUFFIXES: &[&str] = &["-sg", "-x"];

/// 展示优先级：命中这张表的排在前面，其余保持上游原序。
///
/// 上游目录是按「内部登记顺序」返回的，新模型和老模型混在一起。hub 用
/// 一张人工维护的 `CN_UI_ORDER` 解决同一问题——这里取当前在售的主力模型，
/// 让选择器打开时先看到能用的东西，而不是 `deepseek-v3-0324`。
const UI_PRIORITY: &[&str] = &[
    "hy3",
    "deepseek-v4.1-flash",
    "glm-5.3",
    "glm-5.3-flash",
    "kimi-k3.1-pro",
    "kimi-k3-1",
    "kimi-k2.8-preview",
    "deepseek-v4-pro",
    "mimo-v2.6-pro",
    "minimax-m3.1-flash",
    "deepseek-v4-flash",
    "glm-5.2",
    "hy4-preview",
];

fn priority_of(id: &str) -> usize {
    UI_PRIORITY
        .iter()
        .position(|m| *m == id)
        .unwrap_or(UI_PRIORITY.len())
}

/// 这个 id 该不该出现在选择器里（按 id 判断，与配置里的字段无关）。
///
/// 规则照搬 workbuddy2api-hub 的 `curate_remote_catalog`：它同样是对着上游
/// 目录做黑名单式清理——说明「哪个是远古模型」上游并没有给出可靠标记，
/// 各家都只能按名字维护经验规则。
fn is_listable_model_id(id: &str) -> bool {
    let id = id.trim();
    if id.is_empty() || VIRTUAL_ALIASES.contains(&id) {
        return false;
    }
    if NON_CHAT_PREFIXES.iter().any(|p| id.starts_with(p)) {
        return false;
    }
    if NON_CHAT_SUFFIXES.iter().any(|s| id.ends_with(s)) {
        return false;
    }
    if VARIANT_SUFFIXES.iter().any(|s| id.ends_with(s)) {
        return false;
    }
    true
}

fn is_chat_model(m: &Value) -> bool {
    if m.get("videoGeneration").is_some() || m.get("imageGeneration").is_some() {
        return false;
    }
    if let Some(tags) = m.get("tags").and_then(Value::as_array) {
        for tag in tags.iter().filter_map(Value::as_str) {
            let t = tag.to_ascii_lowercase();
            if t.contains("video") || t.contains("image") {
                return false;
            }
        }
    }
    m.get("maxInputTokens").and_then(Value::as_i64).unwrap_or(0) > 0
}

/// 顶层 `models`：对象数组，按对话能力过滤后再收 id。
fn collect_chat_model_ids(models: Option<&Value>, out: &mut Vec<String>) {
    let Some(list) = models.and_then(Value::as_array) else {
        return;
    };
    for item in list {
        let Some(id) = item.get("id").and_then(Value::as_str).map(str::trim) else {
            continue;
        };
        if !is_listable_model_id(id) || !is_chat_model(item) {
            continue;
        }
        if !out.iter().any(|x| x == id) {
            out.push(id.to_string());
        }
    }
}

/// 模型数组可能是裸字符串，也可能是 `{id, name}` 对象，两种都收。
fn collect_model_ids(models: Option<&Value>, out: &mut Vec<String>) {
    let Some(list) = models.and_then(Value::as_array) else {
        return;
    };
    for item in list {
        let id = match item {
            Value::String(s) => Some(s.trim()),
            Value::Object(obj) => obj.get("id").and_then(Value::as_str).map(str::trim),
            _ => None,
        };
        if let Some(id) = id {
            if !id.is_empty() && !out.iter().any(|x| x == id) {
                out.push(id.to_string());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inserts_system_when_missing() {
        let body = json!({"model": "m", "messages": [{"role": "user", "content": "hi"}]});
        let out = build_body(&body);
        assert_eq!(out["messages"][0]["role"], "system");
        assert_eq!(out["messages"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn keeps_existing_system() {
        let body = json!({"model": "m", "messages": [{"role": "system", "content": "s"}]});
        let out = build_body(&body);
        assert_eq!(out["messages"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn clamps_tiny_max_tokens_for_gpt() {
        let body = json!({"model": "gpt-6-astra", "max_tokens": 1, "messages": []});
        assert_eq!(build_body(&body)["max_tokens"], 16);
        let body = json!({"model": "deepseek-v4.1-flash", "max_tokens": 1, "messages": []});
        assert_eq!(build_body(&body)["max_tokens"], 1);
    }

    #[test]
    fn backfills_reasoning_content_for_deepseek() {
        let body = json!({
            "model": "deepseek-v4.1-flash",
            "thinking": {"type": "enabled"},
            "messages": [
                {"role": "system", "content": "s"},
                {"role": "assistant", "content": "a"}
            ]
        });
        let out = build_body(&body);
        assert_eq!(out["messages"][1]["reasoning_content"], "");
    }

    #[test]
    fn drops_private_markers() {
        let body = json!({"model": "m", "messages": [], "_namespace_map": {"a": "b"}});
        assert!(build_body(&body).get("_namespace_map").is_none());
    }

    #[test]
    fn parses_catalog_from_agents() {
        let payload = json!({
            "data": {"agents": {"cli": {"models": ["a", "b"]}, "x": {"models": ["b", "c"]}}}
        });
        assert_eq!(parse_catalog(&payload), vec!["a", "b", "c"]);
    }
}
