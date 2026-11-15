//! Trae 反代引擎：把 OpenAI 协议翻译成 Trae IDE 的私有协议。
//!
//! 移植自 `@magpie-community/opencode-trae-auth`（MIT，v0.2.38）的
//! `index.mjs`。那边是 opencode 插件、靠 `auth.loader` 的自定义 fetch 在
//! 进程内拦截；这里换成我们自己的网关，在 `external.rs` 后面做同样的事。
//!
//! 与豆包通道的本质区别：豆包那边是标准 OpenAI 接口，原样透传即可；Trae
//! 只有一套 IDE 私有协议——签名鉴权、设备绑定、私有 SSE 事件——所以这里
//! 必须逐层翻译：
//!
//! ```text
//! OpenAI 请求 → trae_messages/chat_body → POST /api/agent/v3/llm_utils_chat
//!            ← Trae 私有 SSE（output/token_usage/done/error）← parts 转换
//! ```
//!
//! 三个容易踩的点，都是从参考实现里学来的：
//!
//! 1. **设备必须稳定**。登录时生成 device_id / machine_id / P-256 密钥并
//!    持久化，之后每次请求都带同一套。每次换新设备，请求会被直接丢弃。
//! 2. **续签分两条路**。优先走 `/trae/api/v3/oauth/ExchangeToken` 并用设备
//!    私钥签 `DeviceProof`——这条路 refresh token 不消耗、JWT 绑设备；被拒
//!    才回退 `/cloudide/…`，那条 refresh 一次性作废且签出的 JWT 不带设备。
//! 3. **版本号是准入条件**。Trae 只把新模型给足够新的客户端，所以这里伪装成
//!    TRAE SOLO CN 0.1.69；拿老的 IDE 版本号会看不到新模型。
//!
//! 没有移植过来的部分（参考实现里有、这里没有）：Max 模式的 `-max` 合成模型、
//! 各家用文本写工具调用的方言（DeepSeek DSML / Qwen `<function=>` /
//! Anthropic `<function_calls>` / OpenAI `{"tool_calls":…}` 文本块）、
//! Kimi 的 moonshot schema 重写、签到与用量页。原生工具调用是完整支持的。

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use axum::body::Bytes;
use base64::engine::general_purpose::{STANDARD as B64, URL_SAFE_NO_PAD};
use base64::Engine as _;
use futures_util::StreamExt;
use ring::rand::{SecureRandom, SystemRandom};
use ring::signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_ASN1_SIGNING};
use serde_json::{json, Map, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, Mutex as TokioMutex};

use crate::modules::proxy::store;

/// 设置键。换名字而不是复用 `external_doubao`：两份配置的字段完全不同，
/// 继承过来只会让界面显示一堆对不上的空值。
pub const CONFIG_KEY: &str = "external_trae";

/// 对客户端暴露的模型命名空间。
pub const PREFIX: &str = "trae/";

/// JWT 距到期不足这个时长就先续签再发请求。
const EARLY_MS: i64 = 2 * 60 * 1000;

/// 登录等待上限：用户要开浏览器、登录、可能还要过二次验证。
const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// 模型目录缓存时长。目录接口要 1–2 个 20s 超时，问得太勤没意义。
const CATALOG_TTL_MS: i64 = 5 * 60 * 1000;

// ---------------------------------------------------------------------------
// realm 表
// ---------------------------------------------------------------------------

/// 兜底模型：目录拉不到时用它，保证界面不至于空着。
pub struct FallbackModel {
    pub id: &'static str,
    pub name: &'static str,
    pub context: u64,
    pub sees: bool,
}

/// 一个 Trae 部署（trae.cn / trae.ai）的全部差异都在这张表里。
pub struct Realm {
    pub id: &'static str,
    pub name: &'static str,
    /// 授权页所在站点。
    pub web: &'static str,
    /// 令牌与账号接口。
    pub auth: &'static str,
    /// 模型网关（默认区域）。
    pub api: &'static str,
    /// 国际版的第二个部署（美区）。
    pub api_us: Option<&'static str>,
    pub client_id: &'static str,
    pub app_id: &'static str,
    pub client_version: &'static str,
    pub client_version_code: &'static str,
    pub plugin_version: &'static str,
    pub brand: &'static str,
    /// device_id 的位数。CN 是 16，Global 是 19。
    pub device_digits: usize,
    /// 是否支持设备绑定续签（目前只有 CN）。
    pub device_exchange: bool,
    /// 国际版的列表会标 `is_invisible_to_user`，那种条目它的选择器本身也不显示。
    pub hides_invisible: bool,
    pub functions: &'static [&'static str],
    pub fallback: &'static [FallbackModel],
}

const CN_FUNCTIONS: &[&str] = &[
    "chat_v3",
    "solo_work_lite",
    "solo_agent",
    "solo_agent_lite",
    "solo_coder",
];

const CN_FALLBACK: &[FallbackModel] = &[
    FallbackModel { id: "glm-5.2", name: "GLM-5.2", context: 200_000, sees: false },
    FallbackModel { id: "glm-5", name: "GLM-5", context: 128_000, sees: false },
    FallbackModel { id: "kimi-k2.6", name: "Kimi K2.6", context: 256_000, sees: true },
    FallbackModel { id: "qwen-3.7-plus", name: "Qwen 3.7 Plus", context: 128_000, sees: true },
    FallbackModel { id: "DeepSeek-V4-Pro", name: "DeepSeek V4 Pro", context: 128_000, sees: false },
    FallbackModel { id: "DeepSeek-V4-Flash", name: "DeepSeek V4 Flash", context: 128_000, sees: false },
];

const GLOBAL_FALLBACK: &[FallbackModel] = &[
    FallbackModel { id: "gpt-5.4", name: "GPT-5.4", context: 128_000, sees: false },
    FallbackModel { id: "gemini-3-flash", name: "Gemini 3 Flash", context: 1_000_000, sees: false },
    FallbackModel { id: "kimi-k2.5", name: "Kimi K2.5", context: 256_000, sees: false },
    FallbackModel { id: "minimax-m2", name: "MiniMax M2", context: 128_000, sees: false },
    FallbackModel { id: "deepseek-v3.2", name: "DeepSeek V3.2", context: 128_000, sees: false },
];

const REALM_CN: Realm = Realm {
    id: "trae-cn",
    name: "Trae CN",
    web: "https://www.trae.cn",
    auth: "https://api.trae.cn",
    api: "https://trae-api-cn.mchost.guru",
    api_us: None,
    client_id: "ono9krqynydwx5",
    app_id: "6eefa01c-1036-4c7e-9ca5-d891f63bfcd8",
    client_version: "0.1.69",
    client_version_code: "20260917",
    plugin_version: "2.3.24254",
    brand: "ASUS TUF Gaming A15 FA507RM_FA507RM",
    device_digits: 16,
    device_exchange: true,
    hides_invisible: false,
    functions: CN_FUNCTIONS,
    fallback: CN_FALLBACK,
};

const REALM_GLOBAL: Realm = Realm {
    id: "trae-global",
    name: "Trae Global",
    web: "https://www.trae.ai",
    auth: "https://growsg-normal.trae.ai",
    api: "https://coresg-normal.trae.ai",
    api_us: Some("https://coreva-normal.trae.ai"),
    client_id: "ono9krqynydwx5",
    app_id: "6eefa01c-1036-4c7e-9ca5-d891f63bfcd8",
    client_version: "0.1.69",
    client_version_code: "20260917",
    plugin_version: "2.3.62834",
    brand: "ASUS TUF Gaming A15 FA507RM_FA507RM",
    device_digits: 19,
    device_exchange: false,
    hides_invisible: true,
    functions: CN_FUNCTIONS,
    fallback: GLOBAL_FALLBACK,
};

pub fn realm(id: &str) -> Option<&'static Realm> {
    match id {
        "trae-cn" => Some(&REALM_CN),
        "trae-global" => Some(&REALM_GLOBAL),
        _ => None,
    }
}

pub fn realm_ids() -> [&'static str; 2] {
    ["trae-cn", "trae-global"]
}

// ---------------------------------------------------------------------------
// 账号与设备
// ---------------------------------------------------------------------------

/// 一次登录拿到的全部状态，包括设备。整体存进 settings。
#[derive(Clone, Default)]
pub struct Account {
    pub realm: String,
    pub token: String,
    pub refresh: String,
    /// 毫秒时间戳。
    pub expires: i64,
    pub uid: String,
    pub name: String,
    pub device_id: String,
    pub machine_id: String,
    /// SPKI PEM，续签时上报给 Trae。
    pub device_public_key: String,
    /// PKCS#8 PEM，只在本机用来签 DeviceProof。
    pub device_private_key: String,
    /// 登录时它自己报的模型网关（企业租户会给别的域名）。
    pub api: String,
    pub region: String,
}

impl Account {
    fn to_json(&self) -> Value {
        json!({
            "realm": self.realm,
            "token": self.token,
            "refresh": self.refresh,
            "expires": self.expires,
            "uid": self.uid,
            "name": self.name,
            "deviceId": self.device_id,
            "machineId": self.machine_id,
            "devicePublicKey": self.device_public_key,
            "devicePrivateKey": self.device_private_key,
            "api": self.api,
            "region": self.region,
        })
    }

    fn from_json(v: &Value) -> Option<Self> {
        let token = str_of(v, "token");
        if token.is_empty() {
            return None;
        }
        Some(Self {
            realm: str_of(v, "realm"),
            token,
            refresh: str_of(v, "refresh"),
            expires: num_of(v, "expires"),
            uid: str_of(v, "uid"),
            name: str_of(v, "name"),
            device_id: str_of(v, "deviceId"),
            machine_id: str_of(v, "machineId"),
            device_public_key: str_of(v, "devicePublicKey"),
            device_private_key: str_of(v, "devicePrivateKey"),
            api: str_of(v, "api"),
            region: str_of(v, "region"),
        })
    }
}

fn str_of(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

fn num_of(v: &Value, key: &str) -> i64 {
    v.get(key).and_then(Value::as_i64).unwrap_or(0)
}

// ---------------------------------------------------------------------------
// 配置读写
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Config {
    pub enabled: bool,
    pub realm: String,
    pub models: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self { enabled: false, realm: REALM_CN.id.to_string(), models: Vec::new() }
    }
}

pub fn config() -> Config {
    let raw = store::load_settings(CONFIG_KEY);
    let default = Config::default();
    let wanted = raw.get("realm").and_then(Value::as_str).unwrap_or(&default.realm).to_string();
    Config {
        enabled: raw.get("enabled").and_then(Value::as_bool).unwrap_or(default.enabled),
        realm: if realm(&wanted).is_some() { wanted } else { default.realm },
        models: raw
            .get("models")
            .and_then(Value::as_array)
            .map(|list| list.iter().filter_map(Value::as_str).map(str::to_string).collect())
            .unwrap_or_default(),
    }
}

/// 当前登录的账号，不管配置里启用没启用。
pub fn account() -> Option<Account> {
    let raw = store::load_settings(CONFIG_KEY);
    raw.get("account").and_then(Account::from_json)
}

fn save_account(acc: Option<&Account>) -> Result<(), String> {
    let mut raw = store::load_settings(CONFIG_KEY);
    let obj = raw.as_object_mut().ok_or("配置存档损坏")?;
    match acc {
        Some(a) => {
            obj.insert("account".into(), a.to_json());
        }
        None => {
            obj.remove("account");
        }
    }
    store::save_settings(CONFIG_KEY, &raw)
}

fn save_config_merged(cfg: &Config) -> Result<(), String> {
    let mut raw = store::load_settings(CONFIG_KEY);
    let obj = raw.as_object_mut().ok_or("配置存档损坏")?;
    obj.insert("enabled".into(), json!(cfg.enabled));
    obj.insert("realm".into(), json!(cfg.realm));
    obj.insert("models".into(), json!(cfg.models));
    store::save_settings(CONFIG_KEY, &raw)
}

// ---------------------------------------------------------------------------
// 加密与随机
// ---------------------------------------------------------------------------

/// P-256 SPKI 的固定 DER 前缀：OID id-ecPublicKey + OID prime256v1，
/// 后面直接接 65 字节的未压缩点（0x04 || X || Y）。
/// ring 只给原始点，PEM 头得自己拼。
const SPKI_P256_PREFIX: [u8; 26] = [
    0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08, 0x2a,
    0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00,
];

fn rng() -> &'static SystemRandom {
    static RNG: OnceLock<SystemRandom> = OnceLock::new();
    RNG.get_or_init(SystemRandom::new)
}

fn random_bytes(n: usize) -> Vec<u8> {
    let mut buf = vec![0u8; n];
    // SystemRandom 失败意味着系统熵源坏了，直接 panic 比静默产出可预测值安全。
    rng().fill(&mut buf).expect("system RNG unavailable");
    buf
}

fn random_hex(n: usize) -> String {
    random_bytes(n).iter().map(|b| format!("{b:02x}")).collect()
}

/// n 位数字串，首位不为 0（Trae 的设备 id 是数字，前导 0 会少一位）。
fn random_digits(n: usize) -> String {
    random_bytes(n)
        .iter()
        .enumerate()
        .map(|(i, b)| {
            let d = if i == 0 { (b % 9) + 1 } else { b % 10 };
            char::from(b'0' + d)
        })
        .collect()
}

fn pem(label: &str, der: &[u8]) -> String {
    let b64 = B64.encode(der);
    let mut out = format!("-----BEGIN {label}-----\n");
    for chunk in b64.as_bytes().chunks(64) {
        out.push_str(&String::from_utf8_lossy(chunk));
        out.push('\n');
    }
    out.push_str(&format!("-----END {label}-----\n"));
    out
}

fn pem_body(text: &str) -> Option<Vec<u8>> {
    let b64: String = text
        .lines()
        .filter(|line| !line.starts_with("-----"))
        .collect::<String>()
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    B64.decode(b64).ok()
}

#[derive(Clone, Default)]
struct Device {
    device_id: String,
    machine_id: String,
    public_pem: String,
    private_pem: String,
}

/// 新设备：一个 id、一个 machine id、一对 P-256 密钥。
fn new_device(r: &Realm) -> Device {
    let mut d = Device {
        device_id: random_digits(r.device_digits),
        machine_id: random_hex(16),
        ..Default::default()
    };
    if let Some((private_pem, public_pem)) = new_device_key() {
        d.private_pem = private_pem;
        d.public_pem = public_pem;
    }
    d
}

fn new_device_key() -> Option<(String, String)> {
    let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, rng()).ok()?;
    let key = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, pkcs8.as_ref(), rng()).ok()?;
    let point = key.public_key().as_ref();
    let mut spki = Vec::with_capacity(SPKI_P256_PREFIX.len() + point.len());
    spki.extend_from_slice(&SPKI_P256_PREFIX);
    spki.extend_from_slice(point);
    Some((pem("PRIVATE KEY", pkcs8.as_ref()), pem("PUBLIC KEY", &spki)))
}

/// 续签的 DeviceProof：设备私钥对「方法/路径/客户端/refresh/时间戳/nonce」
/// 逐行拼接的文本做 ECDSA-SHA256，签名是 DER，base64 后上报。
fn device_proof(path: &str, client_id: &str, refresh: &str, private_pem: &str) -> Option<Value> {
    let der = pem_body(private_pem)?;
    let key = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &der, rng()).ok()?;
    let at = chrono::Utc::now().timestamp();
    let nonce = random_hex(16);
    let text = format!("POST\n{path}\n{client_id}\n{refresh}\n{at}\n{nonce}");
    let sig = key.sign(rng(), text.as_bytes()).ok()?;
    Some(json!({
        "Signature": B64.encode(sig.as_ref()),
        "Timestamp": at,
        "Nonce": nonce,
    }))
}

// ---------------------------------------------------------------------------
// 时间与 JWT
// ---------------------------------------------------------------------------

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Trae 的过期时间字段可能是秒、毫秒或 ISO 字符串。
fn when_of(v: &Value) -> i64 {
    match v {
        Value::Number(n) => {
            let raw = n.as_i64().unwrap_or(0);
            if raw <= 0 {
                0
            } else if raw < 10_000_000_000 {
                raw * 1000
            } else {
                raw
            }
        }
        Value::String(s) => {
            if let Ok(n) = s.parse::<i64>() {
                return if n < 10_000_000_000 { n * 1000 } else { n };
            }
            chrono::DateTime::parse_from_rfc3339(s).map(|d| d.timestamp_millis()).unwrap_or(0)
        }
        _ => 0,
    }
}

/// 从 JWT 的 payload 里读 exp，作为没有显式过期时间时的兜底。
fn jwt_exp(token: &str) -> i64 {
    let mut parts = token.split('.');
    let (Some(_), Some(payload), ..) = (parts.next(), parts.next(), parts.next()) else {
        return 0;
    };
    let Ok(raw) = URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')) else {
        return 0;
    };
    let Ok(v) = serde_json::from_slice::<Value>(&raw) else {
        return 0;
    };
    v.get("exp").and_then(Value::as_i64).map(|s| s * 1000).unwrap_or(0)
}

// ---------------------------------------------------------------------------
// HTTP 客户端
// ---------------------------------------------------------------------------

/// Trae CN 是国内域名，走系统代理反而会坏（这台机器常设着 10808/28714）。
/// 国际版相反：直连未必通，让 reqwest 按环境变量决定。
fn client_for(r: &Realm) -> reqwest::Client {
    let builder = reqwest::Client::builder().connect_timeout(Duration::from_secs(10));
    let builder = if r.id == REALM_CN.id { builder.no_proxy() } else { builder };
    builder
        .timeout(Duration::from_secs(120))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

/// 把 Trae 各种形状的错误统一成 (code, message)。
fn error_of(v: &Value) -> (String, String) {
    let code = v
        .get("code")
        .or_else(|| v.get("Code"))
        .or_else(|| v.get("error_code"))
        .map(|c| match c {
            Value::String(s) => s.clone(),
            Value::Number(n) => n.to_string(),
            _ => String::new(),
        })
        .unwrap_or_default();
    let message = v
        .get("message")
        .or_else(|| v.get("Message"))
        .or_else(|| v.get("msg"))
        .or_else(|| v.get("error"))
        .map(|m| match m {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        })
        .unwrap_or_default();
    (code, message)
}

fn lapsed_code(code: &str) -> bool {
    matches!(code, "1001" | "401" | "20101")
}

fn quota_code(code: &str) -> bool {
    matches!(code, "4008" | "1005" | "4011")
}

/// 换一个 function 可能有戏：这个账号的 chat_v3 不接这个模型。
fn wrong_function(code: &str) -> bool {
    matches!(code, "4001" | "4023" | "1005")
}

// ---------------------------------------------------------------------------
// 令牌续签
// ---------------------------------------------------------------------------

/// 全局续签锁。Trae Global 每次续签都会作废旧 refresh token，两个请求同时
/// 续签会让后一个拿着已作废的令牌去换，直接把账号搞挂。
fn renew_lock() -> &'static TokioMutex<()> {
    static LOCK: OnceLock<TokioMutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| TokioMutex::new(()))
}

/// 回退路径：`/cloudide/api/v3/trae/oauth/ExchangeToken`。
/// 这条每次发新 refresh 并作废旧的，签出的 JWT 不带设备。
async fn exchange(acc: &Account, r: &Realm, client_id: &str) -> Result<Account, String> {
    let url = format!("{}/cloudide/api/v3/trae/oauth/ExchangeToken", r.auth);
    let body = json!({
        "ClientID": client_id,
        "RefreshToken": acc.refresh,
        // Trae 自己的客户端就是这么发的：这个字段确实是字面量 "-"。
        "ClientSecret": "-",
        "UserID": "",
    });
    let resp = client_for(r)
        .post(&url)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("{}: 续签请求失败：{e}", r.name))?;
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    let result = v.get("Result").or_else(|| v.get("result")).unwrap_or(&v);
    let token = result
        .get("Token")
        .or_else(|| result.get("token"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if (200..300).contains(&status) && !token.is_empty() {
        let info = result.get("UserInfo").or_else(|| result.get("userInfo")).cloned().unwrap_or(Value::Null);
        return Ok(Account {
            token: token.clone(),
            refresh: result
                .get("RefreshToken")
                .or_else(|| result.get("refreshToken"))
                .and_then(Value::as_str)
                .unwrap_or(&acc.refresh)
                .to_string(),
            expires: fresh_expiry(result, &token),
            uid: if str_of(&info, "UserID").is_empty() { acc.uid.clone() } else { str_of(&info, "UserID") },
            api: if str_of(&info, "Host").is_empty() { acc.api.clone() } else { str_of(&info, "Host") },
            ..acc.clone()
        });
    }
    let (code, message) = error_of(&v);
    let msg = if message.is_empty() { text.chars().take(200).collect() } else { message };
    if matches!(status, 400 | 401 | 403) || code == "10101" {
        return Err(format!("{} 的登录已失效（{msg}），请重新登录", r.name));
    }
    Err(format!("{}: 续签失败 {status} {msg}", r.name))
}

fn fresh_expiry(result: &Value, token: &str) -> i64 {
    let explicit = when_of(result.get("TokenExpireAt").or_else(|| result.get("tokenExpireAt")).unwrap_or(&Value::Null));
    if explicit > 0 {
        explicit
    } else {
        let from_jwt = jwt_exp(token);
        if from_jwt > 0 {
            from_jwt
        } else {
            now_ms() + 3600_000
        }
    }
}

/// 设备绑定续签：`/trae/api/v3/oauth/ExchangeToken` + DeviceInfo + DeviceProof。
/// refresh token 不消耗，JWT 绑在这台设备上——Trae 的签到类接口只认这种。
async fn bound_exchange(acc: &Account, r: &Realm) -> Result<Account, String> {
    let path = "/trae/api/v3/oauth/ExchangeToken";
    let url = format!("{}{path}", r.auth);
    // 依次试 TRAE SOLO CN 与 Trae CN IDE 两个客户端身份，先能用的为准。
    let identities = [
        ("en1oxy7wnw8j9n", "SOLO_PC", "0.1.64"),
        (r.client_id, "IDE_PC", "3.3.99"),
    ];
    let mut last = format!("{}: 设备绑定续签没有可用的客户端身份", r.name);
    for (client_id, platform, version) in identities {
        let Some(proof) = device_proof(path, client_id, &acc.refresh, &acc.device_private_key) else {
            last = format!("{}: 设备私钥不可用，无法签名", r.name);
            continue;
        };
        let body = json!({
            "ClientID": client_id,
            "ClientSecret": "",
            "RefreshToken": acc.refresh,
            "DeviceInfo": {
                "DeviceID": acc.device_id,
                "MachineID": acc.machine_id,
                "PlatformCode": platform,
                "DeviceType": "PC",
                "DeviceName": "",
                "DeviceModel": "",
                "ClientVersion": version,
                "DevicePublicKey": acc.device_public_key,
                "DeviceBrand": r.brand,
                "DeviceCPU": "AMD",
                "OSInfo": "Windows",
                "OSVersion": "10.0.22631",
            },
            "DeviceProof": proof,
            "IDEVersion": version,
        });
        let mut req = client_for(r).post(&url).json(&body);
        if !acc.token.is_empty() {
            req = req.header("Authorization", format!("Cloud-IDE-JWT {}", acc.token));
        }
        let resp = match req.send().await {
            Ok(resp) => resp,
            Err(e) => {
                last = format!("{}: 设备绑定续签请求失败：{e}", r.name);
                continue;
            }
        };
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let result = v.get("Result").or_else(|| v.get("result")).cloned().unwrap_or(Value::Null);
        let token = str_of(&result, "Token");
        if (200..300).contains(&status) && !token.is_empty() {
            return Ok(Account {
                token: token.clone(),
                refresh: {
                    let next = str_of(&result, "RefreshToken");
                    if next.is_empty() { acc.refresh.clone() } else { next }
                },
                expires: fresh_expiry(&result, &token),
                ..acc.clone()
            });
        }
        let (_, message) = error_of(&v);
        last = format!("{}: 设备绑定续签 {status} {message}", r.name);
    }
    Err(last)
}

/// 拿一个还有余量的 token。距到期不足 `EARLY_MS` 就续。
async fn fresh() -> Result<Account, String> {
    let acc = account().ok_or("还没有登录 Trae，请先在「外部模型」页登录")?;
    let r = realm(&acc.realm).ok_or("账号记录的 realm 不认识，请重新登录")?;
    if acc.expires - now_ms() > EARLY_MS {
        return Ok(acc);
    }
    let _guard = renew_lock().lock().await;
    // 等锁期间别的请求可能已经续过了，重新读一次再决定。
    let acc = account().ok_or("还没有登录 Trae，请先在「外部模型」页登录")?;
    if acc.expires - now_ms() > EARLY_MS {
        return Ok(acc);
    }
    let renewed = if r.device_exchange && !acc.device_private_key.is_empty() {
        match bound_exchange(&acc, r).await {
            Ok(next) => next,
            Err(_) => exchange(&acc, r, r.client_id).await?,
        }
    } else {
        exchange(&acc, r, r.client_id).await?
    };
    save_account(Some(&renewed))?;
    Ok(renewed)
}

// ---------------------------------------------------------------------------
// 登录：官方授权页 + 127.0.0.1 回调
// ---------------------------------------------------------------------------

#[derive(Default)]
struct SignInSlot {
    realm: String,
    url: String,
    /// idle / waiting / done / failed
    state: String,
    message: String,
}

fn sign_in_slot() -> &'static Mutex<SignInSlot> {
    static SLOT: OnceLock<Mutex<SignInSlot>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(SignInSlot::default()))
}

fn set_sign_in(realm_id: &str, url: &str, state: &str, message: &str) {
    if let Ok(mut slot) = sign_in_slot().lock() {
        slot.realm = realm_id.to_string();
        slot.url = url.to_string();
        slot.state = state.to_string();
        slot.message = message.to_string();
    }
}

pub fn sign_in_state() -> Value {
    let slot = sign_in_slot().lock().ok();
    let (state, url, message, realm_id) = match slot.as_deref() {
        Some(s) if !s.state.is_empty() => (s.state.clone(), s.url.clone(), s.message.clone(), s.realm.clone()),
        _ => ("idle".to_string(), String::new(), String::new(), String::new()),
    };
    json!({ "state": state, "url": url, "message": message, "realm": realm_id })
}

/// 开始登录：绑一个本机随机端口接回调，返回要用户打开的授权页地址。
///
/// 回调必须落在 127.0.0.1——Trae 的授权页只接受这种回调，别的一律不带令牌。
pub async fn sign_in_start(realm_id: &str) -> Result<Value, String> {
    let r = realm(realm_id).ok_or_else(|| format!("未知的 Trae 部署：{realm_id}"))?;

    // 已经在等就复用，避免用户连点两下起两个监听。
    {
        let slot = sign_in_slot().lock().map_err(|_| "登录状态锁被污染")?;
        if slot.state == "waiting" && slot.realm == r.id && !slot.url.is_empty() {
            return Ok(json!({ "url": slot.url, "realm": r.id, "reused": true }));
        }
    }

    let device = new_device(r);
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.map_err(|e| format!("绑定本机回调端口失败：{e}"))?;
    let port = listener.local_addr().map_err(|e| format!("读取回调端口失败：{e}"))?.port();
    let callback = format!("http://127.0.0.1:{port}/authorize");

    let trace = uuid::Uuid::new_v4().to_string();
    let query = [
        ("login_version", "1"),
        ("auth_from", "solo"),
        ("login_channel", "native_ide"),
        ("plugin_version", r.plugin_version),
        ("auth_type", "local"),
        ("client_id", r.client_id),
        ("redirect", "0"),
        ("login_trace_id", &trace),
        ("auth_callback_url", &callback),
        ("machine_id", &device.machine_id),
        ("device_id", &device.device_id),
        ("x_device_id", &device.device_id),
        ("x_machine_id", &device.machine_id),
        ("x_device_brand", r.brand),
        ("x_device_type", "windows"),
        ("x_os_version", "Windows 10 Pro"),
        ("x_env", ""),
        ("x_app_version", r.client_version),
        ("x_app_type", "stable"),
        // 企业（SaaS / SSO）登录也开放，个人账号不受影响。
        ("hide_saas_login", "false"),
    ]
    .iter()
    .map(|(k, v)| format!("{k}={}", urlencoded(v)))
    .collect::<Vec<_>>()
    .join("&");
    let url = format!("{}/authorization?{query}", r.web);

    set_sign_in(r.id, &url, "waiting", "");
    let realm_id = r.id.to_string();
    let started_url = url.clone();
    tokio::spawn(async move {
        let outcome = accept_callback(listener, r, device).await;
        match outcome {
            Ok(acc) => {
                let who = if acc.name.is_empty() { acc.uid.clone() } else { acc.name.clone() };
                match save_account(Some(&acc)) {
                    Ok(()) => set_sign_in(&realm_id, &started_url, "done", &format!("{who} 已登录")),
                    Err(e) => set_sign_in(&realm_id, &started_url, "failed", &e),
                }
            }
            Err(e) => set_sign_in(&realm_id, &started_url, "failed", &e),
        }
    });

    Ok(json!({ "url": url, "realm": r.id, "reused": false }))
}

pub fn sign_out() -> Value {
    let _ = save_account(None);
    set_sign_in("", "", "idle", "");
    view()
}

// ---------------------------------------------------------------------------
// 回调服务器（手写最小 HTTP/1.1，够用且不用把 axum 拉进登录流程）
// ---------------------------------------------------------------------------

async fn accept_callback(listener: TcpListener, r: &'static Realm, device: Device) -> Result<Account, String> {
    let deadline = tokio::time::Instant::now() + SIGN_IN_TIMEOUT;
    // 浏览器可能先来一个 favicon 之类的请求，最多接 8 个，直到命中 /authorize。
    for _ in 0..8 {
        let accepted = tokio::time::timeout_at(deadline, listener.accept()).await;
        let (mut sock, _) = match accepted {
            Ok(Ok(pair)) => pair,
            Ok(Err(e)) => return Err(format!("回调连接失败：{e}")),
            Err(_) => return Err("登录超时，请重新发起登录".to_string()),
        };
        let mut buf = vec![0u8; 16 * 1024];
        let n = match tokio::time::timeout(Duration::from_secs(10), sock.read(&mut buf)).await {
            Ok(Ok(n)) => n,
            _ => 0,
        };
        let head = String::from_utf8_lossy(&buf[..n]).to_string();
        let mut lines = head.lines();
        let first = lines.next().unwrap_or("");
        let mut pieces = first.split_whitespace();
        let method = pieces.next().unwrap_or("GET");
        let target = pieces.next().unwrap_or("/");
        let (path, query) = match target.split_once('?') {
            Some((p, q)) => (p, q),
            None => (target, ""),
        };

        if method.eq_ignore_ascii_case("OPTIONS") {
            let _ = respond(&mut sock, 204, "text/plain", "").await;
            continue;
        }
        if path != "/authorize" {
            let _ = respond(&mut sock, 404, "text/plain; charset=utf-8", "not found").await;
            continue;
        }

        let q: HashMap<String, String> = query
            .split('&')
            .filter(|pair| !pair.is_empty())
            .map(|pair| match pair.split_once('=') {
                Some((k, v)) => (urldecoded(k), urldecoded(v)),
                None => (urldecoded(pair), String::new()),
            })
            .collect();

        if let Some(err) = q.get("error") {
            let msg = q.get("error_description").cloned().unwrap_or_else(|| err.clone());
            let _ = respond(&mut sock, 200, "text/html; charset=utf-8", &page_html(false, "登录没有完成", &msg)).await;
            return Err(msg);
        }
        if !q.contains_key("userJwt") && !q.contains_key("refreshToken") {
            let msg = format!("{} 没有回传令牌，请重新登录", r.web);
            let _ = respond(&mut sock, 200, "text/html; charset=utf-8", &page_html(false, "登录没有完成", &msg)).await;
            return Err(msg);
        }

        match signed_in(&q, &device, r).await {
            Ok(acc) => {
                let who = if acc.name.is_empty() { acc.uid.clone() } else { acc.name.clone() };
                let text = format!("{who} 已登录，可以关闭这个页面了。");
                let _ = respond(&mut sock, 200, "text/html; charset=utf-8", &page_html(true, "登录成功", &text)).await;
                return Ok(acc);
            }
            Err(e) => {
                let _ = respond(&mut sock, 200, "text/html; charset=utf-8", &page_html(false, "登录没有完成", &e)).await;
                return Err(e);
            }
        }
    }
    Err("回调端口收到 8 个无效请求后放弃".to_string())
}

async fn respond(sock: &mut tokio::net::TcpStream, status: u16, content_type: &str, body: &str) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        204 => "No Content",
        404 => "Not Found",
        _ => "OK",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nAccess-Control-Allow-Origin: *\r\nConnection: close\r\n\r\n",
        body.len()
    );
    sock.write_all(head.as_bytes()).await?;
    sock.write_all(body.as_bytes()).await?;
    sock.flush().await
}

fn page_html(ok: bool, title: &str, text: &str) -> String {
    let color = if ok { "#0f6e56" } else { "#a32d2d" };
    format!(
        "<!doctype html><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width\"><title>{title}</title>\
<style>body{{font:15px/1.6 -apple-system,system-ui,sans-serif;margin:0;display:grid;place-items:center;min-height:100vh;background:#f6f6f4;color:#222}}\
@media(prefers-color-scheme:dark){{body{{background:#1c1c1c;color:#eee}}}}main{{max-width:420px;padding:24px}}\
h1{{font-size:20px;margin:0 0 8px;color:{color}}}p{{margin:0;opacity:.75}}</style>\
<main><h1>{title}</h1><p>{text}</p></main>"
    )
}

/// 处理回调：把授权页传来的 userJwt / userInfo 变成一份可用的账号。
async fn signed_in(q: &HashMap<String, String>, device: &Device, r: &Realm) -> Result<Account, String> {
    let jwt = parse_json(q.get("userJwt").map(String::as_str).unwrap_or(""));
    let info = parse_json(q.get("userInfo").map(String::as_str).unwrap_or(""));
    let host = q.get("host").cloned().unwrap_or_else(|| str_of(&info, "Host"));

    let mut acc = Account {
        realm: r.id.to_string(),
        token: str_or(&jwt, &["Token", "token"]),
        refresh: {
            let from_jwt = str_or(&jwt, &["RefreshToken", "refreshToken"]);
            if from_jwt.is_empty() {
                q.get("refreshToken").cloned().unwrap_or_default()
            } else {
                from_jwt
            }
        },
        expires: when_of(jwt.get("TokenExpireAt").or_else(|| jwt.get("tokenExpireAt")).unwrap_or(&Value::Null)),
        uid: {
            let from_info = str_or(&info, &["UserID", "userId"]);
            if from_info.is_empty() {
                q.get("userId").cloned().unwrap_or_default()
            } else {
                from_info
            }
        },
        name: str_or(&info, &["ScreenName", "screenName"]),
        device_id: device.device_id.clone(),
        machine_id: device.machine_id.clone(),
        device_public_key: device.public_pem.clone(),
        device_private_key: device.private_pem.clone(),
        api: host,
        region: {
            let region = str_or(&info, &["region", "aiRegion", "Region", "AIRegion"]);
            region.trim().to_string()
        },
    };

    let client_id = {
        let from_jwt = str_or(&jwt, &["ClientID", "clientId"]);
        if from_jwt.is_empty() {
            r.client_id.to_string()
        } else {
            from_jwt
        }
    };

    // 授权页没直接给 JWT、只给了 refresh：先换一次。
    if acc.token.is_empty() && !acc.refresh.is_empty() {
        acc = exchange(&acc, r, &client_id).await?;
    }
    if acc.token.is_empty() {
        return Err(format!("{} 没有回传 JWT", r.web));
    }
    if acc.refresh.is_empty() {
        return Err(format!("{} 没有回传 refresh token", r.web));
    }

    // 页面给的 JWT 不绑设备：立刻按设备重签一次，否则签到类接口会拒它。
    // 重签失败不让登录失败——聊天还能用，只是少了那条能力。
    if r.device_exchange && !acc.device_private_key.is_empty() {
        if let Ok(next) = bound_exchange(&acc, r).await {
            acc.token = next.token;
            acc.refresh = next.refresh;
            acc.expires = if next.expires > 0 { next.expires } else { acc.expires };
        }
    }
    if acc.expires <= 0 {
        acc.expires = now_ms() + 3600_000;
    }
    Ok(acc)
}

fn parse_json(text: &str) -> Value {
    if text.trim().is_empty() {
        Value::Null
    } else {
        serde_json::from_str(text).unwrap_or(Value::Null)
    }
}

fn str_or(v: &Value, keys: &[&str]) -> String {
    keys.iter().find_map(|k| v.get(*k).and_then(Value::as_str)).unwrap_or("").to_string()
}

fn urlencoded(text: &str) -> String {
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

fn urldecoded(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(b) => {
                        out.push(b);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

// ---------------------------------------------------------------------------
// 模型目录
// ---------------------------------------------------------------------------

/// 一个可聊的模型。
#[derive(Clone)]
pub struct TraeModel {
    pub id: String,
    pub name: String,
    pub context: u64,
    pub sees: bool,
}

/// 一个模型该走哪个 function、上报哪个 `__dev` 模型名。
#[derive(Clone, Default)]
struct Route {
    function: String,
    model_name: String,
    most: i64,
}

struct Catalog {
    at: i64,
    models: Vec<TraeModel>,
    routes: HashMap<String, Route>,
}

fn catalog_cache() -> &'static TokioMutex<Option<Catalog>> {
    static CACHE: OnceLock<TokioMutex<Option<Catalog>>> = OnceLock::new();
    CACHE.get_or_init(|| TokioMutex::new(None))
}

fn model_host(acc: &Account, r: &Realm) -> String {
    // 国际版按区域选部署：美区账号的模型在 coreva，其余在 coresg。
    if let Some(us) = r.api_us {
        let region = acc.region.to_ascii_uppercase();
        let named_us = acc.api.to_ascii_lowercase().contains("coreva")
            || acc.api.to_ascii_lowercase().contains("api-us");
        if named_us || region == "US" || region.starts_with("US-") || region.starts_with("US_") {
            return us.to_string();
        }
    }
    r.api.to_string()
}

fn ide_headers(acc: &Account, r: &Realm) -> Vec<(String, String)> {
    vec![
        ("Content-Type".into(), "application/json".into()),
        ("Authorization".into(), format!("Cloud-IDE-JWT {}", acc.token)),
        ("X-Cloudide-Token".into(), acc.token.clone()),
        ("x-ide-token".into(), acc.token.clone()),
        ("x-uid".into(), acc.uid.clone()),
        ("x-app-id".into(), r.app_id.into()),
        ("x-device-id".into(), acc.device_id.clone()),
        ("x-machine-id".into(), acc.machine_id.clone()),
        ("x-request-id".into(), uuid::Uuid::new_v4().to_string()),
        ("x-app-version".into(), r.client_version.into()),
        ("x-app-version-code".into(), r.client_version_code.into()),
        ("x-ide-version".into(), r.client_version.into()),
        ("x-ide-version-code".into(), r.client_version_code.into()),
        ("x-ide-version-type".into(), "stable".into()),
        ("x-device-cpu".into(), "AMD".into()),
        ("x-device-brand".into(), r.brand.into()),
        ("x-device-type".into(), "windows".into()),
        ("x-os-version".into(), "Windows 10".into()),
        ("x-system-type".into(), "Windows".into()),
    ]
}

fn apply_headers(mut req: reqwest::RequestBuilder, headers: &[(String, String)]) -> reqwest::RequestBuilder {
    for (k, v) in headers {
        req = req.header(k.as_str(), v.as_str());
    }
    req
}

/// 账号自己的模型目录。拉不到就退回 realm 的兜底表，让界面不至于空着。
pub async fn fetch_models() -> Result<Vec<TraeModel>, String> {
    let (catalog, error) = catalog(false).await;
    if let Some(c) = catalog {
        if !c.models.is_empty() {
            return Ok(c.models);
        }
    }
    Err(error.unwrap_or_else(|| "目录为空".to_string()))
}

/// 额度（积分）用量，来自 Trae 账单页那个接口。
///
/// 接口在 `auth` 域名下而不是聊天网关：`POST {auth}/trae/api/v2/pay/ide_user_ent_usage`，
/// body 必须带 `require_usage: true`——少了它只回额度上限、不回已用量。
///
/// 这里的「积分」是 **Trae 账号自己的额度**，跟 WorkBuddy 的 credit 是两套东西。
/// 走 Trae 通道花的是这个额度，不扣 WorkBuddy 积分，所以「积分计费」页里看不到
/// 这部分消耗；反过来这里的数字也不含 WorkBuddy 官方通道的用量。
pub async fn entitlement() -> Result<Value, String> {
    let acc = fresh().await?;
    // 缓存按 uid 区分：同一台机器可能先后登过不同账号，额度不能串。
    {
        let cache = entitlement_cache().lock().await;
        if let Some(c) = cache.as_ref() {
            if c.uid == acc.uid && now_ms() - c.at < ENTITLEMENT_TTL_MS {
                return Ok(c.value.clone());
            }
        }
    }
    let r = realm(&acc.realm).ok_or_else(|| "账号记录的 realm 不认识".to_string())?;
    let url = format!("{}/trae/api/v2/pay/ide_user_ent_usage", r.auth);
    let client = client_for(r);
    let headers = ide_headers(&acc, r);
    let body = json!({ "require_usage": true, "req_source": 0 });
    let raw = post_json(&client, &headers, &url, &body).await?;
    let value = summarize_entitlement(&raw);
    let mut cache = entitlement_cache().lock().await;
    *cache = Some(CachedEntitlement { at: now_ms(), uid: acc.uid.clone(), value: value.clone() });
    Ok(value)
}

/// 把账单接口的返回压成界面要的几个数。
///
/// 口径照 Trae 自己的账单页：把有限额的包累加成总量，`credits_limit == -1`
/// 记为无限；已用取各包的 `usage.credits_amount`（没用过的包这里是空对象）。
/// 顶层 `usage_summary` 更直接，但不是每个账号都有，所以只当校对用于兜底。
///
/// `end_time` 是**秒**级时间戳，输出统一成毫秒，与 `expiresAt` 等字段一致。
fn summarize_entitlement(v: &Value) -> Value {
    let now = chrono::Utc::now().timestamp();
    let packs = v.get("user_entitlement_pack_list").and_then(Value::as_array);

    let mut total = 0.0_f64;
    let mut used = 0.0_f64;
    let mut unlimited = false;
    // 最早到期：只算「还有剩余、且还没过期」的包——已经用完或已过期的包
    // 到期与否都不改变账号上的额度，不该拿来当续期日。
    let mut soonest: i64 = 0;
    let mut lapsing = 0.0_f64;
    let mut items: Vec<Value> = Vec::new();

    for p in packs.into_iter().flatten() {
        let bi = p.get("entitlement_base_info").unwrap_or(&Value::Null);
        let quota = bi.get("quota").unwrap_or(&Value::Null);
        let limit = f64_of(quota, "credits_limit");
        let spent = p
            .get("usage")
            .and_then(|u| f64_of(u, "credits_amount"))
            .unwrap_or(0.0);
        let end = i64_of(bi, "end_time")
            .or_else(|| i64_of(bi, "expire_time"))
            .or_else(|| i64_of(p, "end_time"))
            .unwrap_or(0);

        // Trae 用 -1 表示无限额度，不能当负数累加掉。
        if let Some(l) = limit {
            if l < 0.0 {
                unlimited = true;
            } else {
                total += l;
            }
        }
        if spent > 0.0 {
            used += spent;
        }
        // 还有剩余、且未过期，才参与「最早到期」的判定。
        let still_useful = match limit {
            Some(l) if l < 0.0 => true,
            Some(l) => l > 0.0 && spent < l,
            None => false,
        };
        if still_useful && end > now {
            let rest = match limit {
                Some(l) if l < 0.0 => 0.0,
                Some(l) => (l - spent.max(0.0)).max(0.0),
                None => 0.0,
            };
            if soonest == 0 || end < soonest {
                soonest = end;
                lapsing = rest;
            } else if end == soonest {
                lapsing += rest;
            }
        }

        items.push(json!({
            "label": str_of(p, "display_desc"),
            "group": str_of(p, "group_name"),
            "limit": limit.filter(|l| *l >= 0.0),
            "used": spent,
            "expiresAt": if end > 0 { json!(end * 1000) } else { Value::Null },
        }));
    }

    // 顶层字段当校对：它给的总额与包累加不一致时（比如账号有我们没列出的包）
    // 以顶层为准，因为它就是账单页上显示的那个数。
    let summary = v.get("usage_summary").unwrap_or(&Value::Null);
    if let (Some(t), Some(c)) = (f64_of(summary, "total_amount"), f64_of(summary, "consumed_amount")) {
        if t > 0.0 {
            total = t;
            used = c;
        }
    }
    let left = v
        .get("total_balance")
        .and_then(Value::as_f64)
        .unwrap_or_else(|| (total - used).max(0.0));
    let ratio = if total > 0.0 { (used / total).clamp(0.0, 1.0) } else { 0.0 };

    json!({
        "ok": true,
        "total": total,
        "used": used,
        "left": left,
        "ratio": ratio,
        "unlimited": unlimited,
        "expiresAt": if soonest > 0 { json!(soonest * 1000) } else { Value::Null },
        "lapsing": lapsing,
        "packs": items,
        "fetchedAt": now * 1000,
    })
}

struct CachedEntitlement {
    at: i64,
    uid: String,
    value: Value,
}

fn entitlement_cache() -> &'static TokioMutex<Option<CachedEntitlement>> {
    static CACHE: OnceLock<TokioMutex<Option<CachedEntitlement>>> = OnceLock::new();
    CACHE.get_or_init(|| TokioMutex::new(None))
}

/// 额度缓存时长。账单页的数字不会一秒一变，页面反复刷新时没必要每次都打接口。
const ENTITLEMENT_TTL_MS: i64 = 60 * 1000;

fn f64_of(v: &Value, key: &str) -> Option<f64> {
    let raw = v.get(key)?;
    raw.as_f64()
        .or_else(|| raw.as_str().and_then(|s| s.trim().parse::<f64>().ok()))
}

fn i64_of(v: &Value, key: &str) -> Option<i64> {
    let raw = v.get(key)?;
    raw.as_i64()
        .or_else(|| raw.as_f64().map(|f| f as i64))
        .or_else(|| raw.as_str().and_then(|s| s.trim().parse::<i64>().ok()))
}

/// 只读缓存，不发起任何网络请求。
///
/// 聊天路径必须用这个而不是 [`catalog`]：目录接口在 Trae 慢的时候要吃掉
/// 两个 20 秒超时，把它挂在首字之前等于让用户干等 40 秒。缓存里没有就退回
/// 默认 function——聊天本身不需要目录也能答，只是少了「这个模型走哪个
/// function」的优化。
async fn cached_catalog() -> Option<Catalog> {
    let cache = catalog_cache().lock().await;
    cache.as_ref().map(|c| Catalog { at: c.at, models: c.models.clone(), routes: c.routes.clone() })
}

async fn catalog(force: bool) -> (Option<Catalog>, Option<String>) {
    let mut cache = catalog_cache().lock().await;
    if !force {
        if let Some(c) = cache.as_ref() {
            if now_ms() - c.at < CATALOG_TTL_MS {
                return (Some(Catalog { at: c.at, models: c.models.clone(), routes: c.routes.clone() }), None);
            }
        }
    }
    let acc = match fresh().await {
        Ok(a) => a,
        Err(e) => return (None, Some(e)),
    };
    let r = match realm(&acc.realm) {
        Some(r) => r,
        None => return (None, Some("账号记录的 realm 不认识".to_string())),
    };
    match read_catalog(&acc, r).await {
        Ok((models, routes)) => {
            let catalog = Catalog { at: now_ms(), models: models.clone(), routes: routes.clone() };
            *cache = Some(Catalog { at: catalog.at, models, routes });
            (Some(catalog), None)
        }
        Err(e) => (None, Some(e)),
    }
}

/// 问一次目录：`batch_get_detail_param` 一次拿到所有 function 的列表，
/// 它给空就退回 `get_detail_param` 逐个问。
async fn read_catalog(acc: &Account, r: &Realm) -> Result<(Vec<TraeModel>, HashMap<String, Route>), String> {
    let host = model_host(acc, r);
    let client = client_for(r);
    let headers = ide_headers(acc, r);

    let mut lists: HashMap<String, Vec<Value>> = HashMap::new();
    let batch_body = json!({
        "functions": r.functions,
        "agent_type": "",
        "current_config_info": { "config_name": "", "is_custom_model": false },
        "mode_type": 0,
        "access_type": 0,
        "ab_force_vids": "",
        "ab_autotest_advanced_mode": 0,
        "show_custom_model": true,
    });
    if let Ok(v) = post_json(&client, &headers, &format!("{host}/api/ide/v1/batch_get_detail_param"), &batch_body).await {
        let groups = v
            .get("function_configs")
            .or_else(|| v.get("data").and_then(|d| d.get("function_configs")))
            .and_then(Value::as_array);
        if let Some(groups) = groups {
            for g in groups {
                let name = str_of(g, "function");
                if !r.functions.contains(&name.as_str()) {
                    continue;
                }
                if let Some(items) = g.get("config_info_list").and_then(Value::as_array) {
                    let entry = lists.entry(name).or_default();
                    for item in items {
                        if !str_of(item, "config_name").is_empty() {
                            entry.push(item.clone());
                        }
                    }
                }
            }
        }
    }

    if lists.is_empty() {
        let single_body = |function: &str| {
            json!({
                "function": function,
                "config_names": null,
                "need_prompt": false,
                "current_config_info": null,
                "poly_prompt": true,
                "mode_type": null,
                "agent_type": null,
            })
        };
        for function in r.functions {
            if let Ok(v) = post_json(&client, &headers, &format!("{host}/api/ide/v1/get_detail_param"), &single_body(function)).await {
                let items = v
                    .get("config_info_list")
                    .or_else(|| v.get("data").and_then(|d| d.get("config_info_list")))
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let kept: Vec<Value> = items.into_iter().filter(|m| !str_of(m, "config_name").is_empty()).collect();
                if !kept.is_empty() {
                    lists.insert((*function).to_string(), kept);
                }
            }
        }
    }

    if lists.is_empty() {
        return Err(format!("{} 没有给出模型目录（账号可能已失效）", r.name));
    }

    // 同一个模型可能出现在多个 function 的列表里。选中哪一条决定了请求
    // 走哪个 function，也决定了 Trae 用量页把它记在哪个名字下。判据顺序：
    // 带 `__dev` 模型的优先（那才是真能服务的条目），其次没被别的模型冒名，
    // 再次名字和多数列表一致。
    let mut names: HashMap<String, HashMap<String, usize>> = HashMap::new();
    for function in r.functions {
        for m in lists.get(*function).map(Vec::as_slice).unwrap_or(&[]) {
            if !chat_model(m, r) {
                continue;
            }
            let id = str_of(m, "config_name");
            let name = display_name(m);
            *names.entry(id).or_default().entry(name).or_insert(0) += 1;
        }
    }
    let mut agreed: HashMap<String, String> = HashMap::new();
    for (id, by) in &names {
        let top = by.values().copied().max().unwrap_or(0);
        let winners: Vec<&String> = by.iter().filter(|(_, n)| **n == top).map(|(n, _)| n).collect();
        agreed.insert(id.clone(), if winners.len() == 1 { winners[0].clone() } else { String::new() });
    }

    let mut best: HashMap<String, (u32, Value, String)> = HashMap::new();
    for function in r.functions {
        for m in lists.get(*function).map(Vec::as_slice).unwrap_or(&[]) {
            if !chat_model(m, r) {
                continue;
            }
            let id = str_of(m, "config_name");
            let name = display_name(m);
            let misnamed = names.get(&id).map(|b| b.len() > 1).unwrap_or(false)
                && lists
                    .get(*function)
                    .map(|l| {
                        l.iter().any(|o| {
                            chat_model(o, r)
                                && str_of(o, "config_name") != id
                                && display_name(o) == name
                        })
                    })
                    .unwrap_or(false);
            let score = (if dev_model(m).is_some() { 2 } else { 0 })
                + (if misnamed { 0 } else { 1 })
                + (if agreed.get(&id).map(|a| a == &name).unwrap_or(false) { 1 } else { 0 });
            match best.get(&id) {
                Some((was, _, _)) if *was >= score => {}
                _ => {
                    best.insert(id, (score, m.clone(), (*function).to_string()));
                }
            }
        }
    }

    let mut models = Vec::new();
    let mut routes = HashMap::new();
    for (id, (_, entry, function)) in best {
        let dev = dev_model(&entry);
        let context = entry
            .get("context_window_tokens")
            .and_then(|t| t.get("dev"))
            .and_then(Value::as_u64)
            .or_else(|| entry.get("prompt_max_tokens").and_then(Value::as_u64))
            .unwrap_or(128_000);
        let sees = sees_of(&entry).unwrap_or(false);
        models.push(TraeModel {
            name: display_name(&entry),
            id: id.clone(),
            context,
            sees,
        });
        routes.insert(
            id,
            Route {
                function,
                model_name: dev.map(|d| str_of(d, "model_name")).unwrap_or_default(),
                most: dev.map(|d| d.get("max_tokens").and_then(Value::as_i64).unwrap_or(0)).unwrap_or(0),
            },
        );
    }
    models.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    Ok((models, routes))
}

async fn post_json(
    client: &reqwest::Client,
    headers: &[(String, String)],
    url: &str,
    body: &Value,
) -> Result<Value, String> {
    let resp = apply_headers(client.post(url), headers)
        .json(body)
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .map_err(|e| format!("{url}: {e}"))?;
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    if status == 401 {
        return Err("Trae 的登录已失效，请重新登录".to_string());
    }
    if !(200..300).contains(&status) {
        let (code, message) = error_of(&v);
        if lapsed_code(&code) {
            return Err("Trae 的登录已失效，请重新登录".to_string());
        }
        return Err(format!("{url} 返回 {status} {message}"));
    }
    Ok(v)
}

/// 列表里能聊的条目：排除 IDE 自己的辅助项（summary、fast_apply 之类）、
/// 关掉的配置、自定义模型占位。
fn chat_model(m: &Value, r: &Realm) -> bool {
    let id = str_of(m, "config_name");
    if id.is_empty() || id.to_ascii_lowercase().starts_with("custom_model") {
        return false;
    }
    if let Some(usage) = m.get("usage").and_then(Value::as_str) {
        if usage != "chat_completion" {
            return false;
        }
    }
    if m.get("config_switch").and_then(Value::as_bool) == Some(false) {
        return false;
    }
    if m.get("is_custom_model").and_then(Value::as_bool) == Some(true) {
        return false;
    }
    if r.hides_invisible && m.get("is_invisible_to_user").and_then(Value::as_bool) == Some(true) {
        return false;
    }
    true
}

fn display_name(m: &Value) -> String {
    let dc = m.get("display_config");
    let pick = |k: &str| -> Option<String> {
        m.get(k)
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| dc.and_then(|d| d.get(k)).and_then(Value::as_str).map(str::to_string))
    };
    pick("display_name")
        .or_else(|| pick("display_model_name"))
        .unwrap_or_else(|| str_of(m, "config_name"))
}

fn dev_model(m: &Value) -> Option<&Value> {
    m.get("model_detail_list").and_then(Value::as_array)?.iter().find(|d| {
        d.get("model_name").and_then(Value::as_str).map(|n| n.ends_with("__dev")).unwrap_or(false)
    })
}

fn sees_of(m: &Value) -> Option<bool> {
    m.get("multimodal")
        .and_then(Value::as_bool)
        .or_else(|| m.get("display_config").and_then(|d| d.get("multimodal")).and_then(Value::as_bool))
}

// ---------------------------------------------------------------------------
// 请求转换：OpenAI → Trae
// ---------------------------------------------------------------------------

const TOOL_OPEN: &str = "<tool_call>";
const TOOL_CLOSE: &str = "</tool_call>";

/// 一条消息拆成正文与图片。模型看不见图时用占位文字，别把图发过去浪费带宽。
fn parts_of(content: &Value, sees: bool) -> (String, Vec<Value>) {
    match content {
        Value::String(s) => (s.clone(), Vec::new()),
        Value::Array(items) => {
            let mut text = Vec::new();
            let mut images = Vec::new();
            for p in items {
                match p.get("type").and_then(Value::as_str).unwrap_or("") {
                    "text" | "input_text" => {
                        if let Some(t) = p.get("text").and_then(Value::as_str) {
                            if !t.is_empty() {
                                text.push(t.to_string());
                            }
                        }
                    }
                    "image_url" | "input_image" => {
                        if sees {
                            if let Some(url) = image_url(p) {
                                images.push(json!({ "type": "image_url", "image_url": { "url": url } }));
                            }
                        } else {
                            text.push("[an image, which this model can't see]".to_string());
                        }
                    }
                    _ => {}
                }
            }
            (text.join("\n"), images)
        }
        Value::Null => (String::new(), Vec::new()),
        other => (other.to_string(), Vec::new()),
    }
}

fn image_url(p: &Value) -> Option<String> {
    match p.get("image_url") {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        Some(v) => v.get("url").and_then(Value::as_str).map(str::to_string),
        None => p.get("url").and_then(Value::as_str).map(str::to_string),
    }
}

/// 中间态：文本 / 图片 / 工具调用还分开，最后一并折进 Trae 的形状。
struct Turn {
    role: String,
    text: String,
    images: Vec<Value>,
    calls: Option<Vec<Value>>,
    tool_call_id: String,
}

/// OpenAI 的对话转成 Trae 的。
///
/// 几个 Trae 的硬约束决定了这里的写法：
/// - 不接受连续两条同角色消息，所以 assistant 要折叠、同角色要合并；
/// - 工具结果必须紧跟在发起它的调用之后，中间不能插任何消息（否则 11148）；
/// - 工具结果没有正文会被整条请求拒掉（4027），所以要给占位。
fn trae_messages(req: &Value, sees: bool) -> Vec<Value> {
    let native = req.get("tools").and_then(Value::as_array).map(|a| !a.is_empty()).unwrap_or(false)
        && req.get("tool_choice").and_then(Value::as_str) != Some("none");
    let messages = req.get("messages").and_then(Value::as_array).cloned().unwrap_or_default();

    let mut names: HashMap<String, String> = HashMap::new();
    let mut dropped: Vec<String> = Vec::new();
    let mut turns: Vec<Turn> = Vec::new();
    let mut pending_images: Vec<Value> = Vec::new();

    for m in &messages {
        let role = match m.get("role").and_then(Value::as_str).unwrap_or("user") {
            "developer" => "system",
            other => other,
        };
        let (text, images) = parts_of(m.get("content").unwrap_or(&Value::Null), sees);
        let calls = m.get("tool_calls").and_then(Value::as_array).cloned().unwrap_or_default();

        if role == "assistant" && !calls.is_empty() && !native {
            // 不带工具的历史：把调用写回文本块，和过去发给模型的样子一致。
            let mut blocks = Vec::new();
            for c in &calls {
                let name = c
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if let Some(id) = c.get("id").and_then(Value::as_str) {
                    names.insert(id.to_string(), name.to_string());
                }
                let mut args = c
                    .get("function")
                    .and_then(|f| f.get("arguments"))
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                if let Some(s) = args.as_str() {
                    if let Ok(parsed) = serde_json::from_str::<Value>(s) {
                        args = parsed;
                    }
                }
                blocks.push(format!("{TOOL_OPEN}{}{TOOL_CLOSE}", json!({ "name": name, "arguments": args })));
            }
            let joined = std::iter::once(text.clone())
                .chain(blocks)
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join("\n");
            turns.push(Turn { role: "assistant".into(), text: joined, images, calls: None, tool_call_id: String::new() });
            continue;
        }

        if role == "assistant" && !calls.is_empty() {
            let mut kept = Vec::new();
            for c in &calls {
                let name = c
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
                    .or_else(|| c.get("name").and_then(Value::as_str))
                    .unwrap_or("")
                    .trim()
                    .to_string();
                // 没有名字的调用 Trae 直接不收，连它对应的结果一起丢掉。
                if name.is_empty() {
                    if let Some(id) = c.get("id").and_then(Value::as_str) {
                        dropped.push(id.to_string());
                    }
                    continue;
                }
                let id = c
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("call_{}", kept.len() + 1));
                names.insert(id.clone(), name.clone());
                let mut args = c
                    .get("function")
                    .and_then(|f| f.get("arguments"))
                    .or_else(|| c.get("arguments"))
                    .cloned()
                    .unwrap_or_else(|| json!("{}"));
                if !args.is_string() {
                    args = json!(args.to_string());
                }
                kept.push(json!({
                    "id": id,
                    "type": "function",
                    "function_call": { "name": name, "arguments": args },
                }));
            }
            if !kept.is_empty() {
                turns.push(Turn { role: "assistant".into(), text, images, calls: Some(kept), tool_call_id: String::new() });
            } else if !text.is_empty() || !images.is_empty() {
                turns.push(Turn { role: "assistant".into(), text, images, calls: None, tool_call_id: String::new() });
            }
            continue;
        }

        if role == "assistant" {
            flush_images(&mut turns, &mut pending_images);
            turns.push(Turn { role: "assistant".into(), text, images, calls: None, tool_call_id: String::new() });
            continue;
        }

        if role == "tool" {
            let id = m.get("tool_call_id").and_then(Value::as_str).unwrap_or("").to_string();
            if dropped.iter().any(|d| d == &id) {
                continue;
            }
            if !native || !names.contains_key(&id) {
                flush_images(&mut turns, &mut pending_images);
                let who = names.get(&id).cloned().unwrap_or_else(|| {
                    m.get("name").and_then(Value::as_str).unwrap_or("the tool").to_string()
                });
                let label = if id.is_empty() { "?".to_string() } else { id.clone() };
                turns.push(Turn {
                    role: "user".into(),
                    text: format!("Result of {who} (call {label}):\n{text}"),
                    images,
                    calls: None,
                    tool_call_id: String::new(),
                });
                continue;
            }
            // 工具的图片不能放在 tool 消息里（部分模型读不到），留到这一批
            // 结果之后再以 user 消息补上。
            if !images.is_empty() {
                pending_images.extend(images.iter().cloned());
            }
            let body = if !text.is_empty() {
                text
            } else if !images.is_empty() {
                "(the result's image follows)".to_string()
            } else {
                "(no output)".to_string()
            };
            turns.push(Turn { role: "tool".into(), text: body, images: Vec::new(), calls: None, tool_call_id: id });
            continue;
        }

        flush_images(&mut turns, &mut pending_images);
        turns.push(Turn {
            role: if role == "system" { "system".into() } else { "user".into() },
            text,
            images,
            calls: None,
            tool_call_id: String::new(),
        });
    }
    flush_images(&mut turns, &mut pending_images);

    // 折叠：连续的 assistant 合成一条（文本相接、调用合并）；同一角色的
    // 普通消息也合并。工具结果自成一条，不参与合并。
    let mut merged: Vec<Turn> = Vec::new();
    for turn in turns {
        if turn.role == "tool" {
            merged.push(turn);
            continue;
        }
        if turn.role == "assistant" {
            let can_fold = merged.last().map(|l| l.role == "assistant").unwrap_or(false);
            if can_fold {
                let last = merged.last_mut().expect("checked");
                last.text = join_text(&last.text, &turn.text);
                last.images.extend(turn.images.iter().cloned());
                match (&mut last.calls, turn.calls) {
                    (Some(existing), Some(more)) => existing.extend(more),
                    (slot @ None, Some(more)) => *slot = Some(more),
                    _ => {}
                }
                if last.text.is_empty() && last.images.is_empty() && last.calls.is_none() {
                    merged.pop();
                }
                continue;
            }
            if turn.text.is_empty() && turn.images.is_empty() && turn.calls.is_none() {
                continue;
            }
            merged.push(turn);
            continue;
        }
        if turn.text.is_empty() && turn.images.is_empty() {
            continue;
        }
        let can_merge = merged
            .last()
            .map(|l| l.role == turn.role && l.role != "system")
            .unwrap_or(false);
        if can_merge {
            let last = merged.last_mut().expect("checked");
            last.text = join_text(&last.text, &turn.text);
            last.images.extend(turn.images.iter().cloned());
        } else {
            merged.push(turn);
        }
    }

    merged
        .into_iter()
        .map(|t| {
            let mut content: Vec<Value> = Vec::new();
            if !t.text.is_empty() {
                content.push(json!({ "type": "text", "text": t.text }));
            }
            content.extend(t.images.iter().cloned());
            let mut obj = Map::new();
            obj.insert("role".into(), json!(t.role));
            if let Some(calls) = t.calls {
                obj.insert("content".into(), if content.is_empty() { Value::Null } else { json!(content) });
                obj.insert("tool_calls".into(), json!(calls));
            } else {
                obj.insert("content".into(), json!(content));
                if t.role == "tool" {
                    obj.insert("tool_call_id".into(), json!(t.tool_call_id));
                }
            }
            Value::Object(obj)
        })
        .collect()
}

fn flush_images(turns: &mut Vec<Turn>, pending: &mut Vec<Value>) {
    if pending.is_empty() {
        return;
    }
    turns.push(Turn {
        role: "user".into(),
        text: String::new(),
        images: std::mem::take(pending),
        calls: None,
        tool_call_id: String::new(),
    });
}

fn join_text(a: &str, b: &str) -> String {
    match (a.is_empty(), b.is_empty()) {
        (true, _) => b.to_string(),
        (_, true) => a.to_string(),
        _ => format!("{a}\n\n{b}"),
    }
}

/// Trae 侧的工具定义：parameters 必须是 JSON 字符串，不是对象。
fn native_tools(tools: &[Value]) -> Vec<Value> {
    tools
        .iter()
        .map(|t| {
            let f = t.get("function").unwrap_or(t);
            let params = f.get("parameters").cloned().unwrap_or_else(|| json!({}));
            json!({
                "type": "function",
                "function": {
                    "name": f.get("name").cloned().unwrap_or(Value::Null),
                    "description": f.get("description").cloned().unwrap_or_else(|| json!("")),
                    "parameters": if params.is_string() { params } else { json!(params.to_string()) },
                }
            })
        })
        .collect()
}

/// 请求里的推理强度映射到 Trae 的词表。它不认识的等级宁可丢掉：
/// 发一个不支持的值会被整条拒掉，而不发只是按模型默认跑。
fn trae_effort(value: &Value) -> Option<String> {
    let raw = value.as_str()?.to_ascii_lowercase();
    let mapped = match raw.as_str() {
        "light" | "low" | "minimal" => "light",
        "medium" | "mid" => "medium",
        "high" => "high",
        "extra_high" | "xhigh" | "max" => "extra_high",
        _ => return None,
    };
    Some(mapped.to_string())
}

fn chat_body(req: &Value, function: &str, model: &str, route: &Route, session: &str, sees: bool) -> Value {
    let mut body = Map::new();
    body.insert("messages".into(), json!(trae_messages(req, sees)));
    body.insert("function".into(), json!(function));
    body.insert("config_name".into(), json!(model));
    body.insert("model".into(), json!(model));
    if !route.model_name.is_empty() {
        body.insert("model_name".into(), json!(route.model_name));
    }
    // Trae 只有 SSE，非流式由我们自己聚合。
    body.insert("stream".into(), json!(true));
    body.insert("request_id".into(), json!(uuid::Uuid::new_v4().to_string()));
    body.insert(
        "session_id".into(),
        json!(if session.is_empty() { uuid::Uuid::new_v4().to_string() } else { session.to_string() }),
    );

    if let Some(effort) = trae_effort(req.get("reasoning_effort").or_else(|| req.get("reasoningEffort")).unwrap_or(&Value::Null)) {
        body.insert("reasoning_effort".into(), json!(effort));
    }
    if let Some(max) = req.get("max_completion_tokens").or_else(|| req.get("max_tokens")).and_then(Value::as_i64) {
        if max > 0 {
            let capped = if route.most > 0 { max.min(route.most) } else { max };
            body.insert("max_tokens".into(), json!(capped));
        }
    }
    if let Some(t) = req.get("temperature").and_then(Value::as_f64) {
        body.insert("temperature".into(), json!(t));
    }
    if let Some(tools) = req.get("tools").and_then(Value::as_array) {
        if !tools.is_empty() && req.get("tool_choice").and_then(Value::as_str) != Some("none") {
            body.insert("tools".into(), json!(native_tools(tools)));
            if let Some(choice) = req.get("tool_choice") {
                let name = choice
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
                    .or_else(|| choice.get("name").and_then(Value::as_str));
                body.insert(
                    "tool_choice".into(),
                    match name {
                        Some(n) => json!(n),
                        None => choice.clone(),
                    },
                );
            }
            if let Some(parallel) = req.get("parallel_tool_calls").and_then(Value::as_bool) {
                body.insert("parallel_tool_calls".into(), json!(parallel));
            }
        }
    }
    Value::Object(body)
}

// ---------------------------------------------------------------------------
// 响应转换：Trae 私有 SSE → OpenAI
// ---------------------------------------------------------------------------

/// 一个 Trae 事件。
struct Ev {
    name: String,
    data: Value,
}

/// Trae 的 SSE 是标准行协议，只是事件名和字段都是它自己的。
struct SseReader {
    stream: Pin<Box<dyn futures_util::Stream<Item = Result<Bytes, reqwest::Error>> + Send>>,
    buf: String,
    event: String,
    finished: bool,
}

impl SseReader {
    fn new(resp: reqwest::Response) -> Self {
        Self { stream: Box::pin(resp.bytes_stream()), buf: String::new(), event: String::new(), finished: false }
    }

    async fn next_event(&mut self) -> Option<Ev> {
        loop {
            // 先把缓冲区里完整的行吃掉。
            while let Some(idx) = self.buf.find('\n') {
                let line = self.buf[..idx].trim_end_matches('\r').to_string();
                self.buf.drain(..=idx);
                if line.is_empty() {
                    // 空行 = 一个事件结束。
                    continue;
                }
                if line.starts_with(':') {
                    continue;
                }
                if let Some(rest) = line.strip_prefix("event:") {
                    self.event = rest.trim().to_string();
                    continue;
                }
                if let Some(rest) = line.strip_prefix("data:") {
                    let raw = rest.trim_start();
                    return Some(self.emit(raw));
                }
            }
            if self.finished {
                let rest = self.buf.trim().to_string();
                self.buf.clear();
                if let Some(data) = rest.strip_prefix("data:") {
                    return Some(self.emit(data.trim_start()));
                }
                return None;
            }
            match self.stream.next().await {
                Some(Ok(chunk)) => self.buf.push_str(&String::from_utf8_lossy(&chunk)),
                Some(Err(_)) | None => self.finished = true,
            }
        }
    }

    fn emit(&mut self, raw: &str) -> Ev {
        let name = std::mem::take(&mut self.event);
        if raw == "[DONE]" {
            return Ev { name: "done".to_string(), data: Value::String("[DONE]".into()) };
        }
        let data: Value = serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string()));
        let name = if name.is_empty() {
            match &data {
                Value::Object(o) => {
                    let obj = Value::Object(o.clone());
                    let by_event = str_of(&obj, "event");
                    if by_event.is_empty() {
                        str_of(&obj, "type")
                    } else {
                        by_event
                    }
                }
                _ => String::new(),
            }
        } else {
            name
        };
        Ev { name, data }
    }
}

/// 事件名归一：`TokenUsage` / `token-usage` 都变成 `token_usage`。
fn event_name(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    let mut prev_lower = false;
    for ch in s.chars() {
        if ch.is_ascii_uppercase() && prev_lower {
            out.push('_');
        }
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_lower = ch.is_ascii_lowercase() || ch.is_ascii_digit();
        } else if !out.ends_with('_') {
            out.push('_');
        }
    }
    out.trim_matches('_').to_string()
}

fn is_answer_event(name: &str) -> bool {
    matches!(
        name,
        "" | "output"
            | "message"
            | "delta"
            | "content"
            | "text"
            | "answer"
            | "chunk"
            | "output_delta"
            | "message_delta"
    )
}

fn finish_of(d: &Value) -> String {
    let raw = d
        .get("finish_reason")
        .or_else(|| d.get("stop_reason"))
        .or_else(|| d.get("finishReason"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    match raw.as_str() {
        "length" | "max_tokens" | "max_output_tokens" | "max_length" => "length".to_string(),
        "content_filter" | "sensitive" => "content_filter".to_string(),
        _ => String::new(),
    }
}

/// Trae 的 token 计数搬成 OpenAI 的形状。
///
/// 缓存计数有个坑：Trae 有时明说不缓存读了多少、写多少（两个 0），但它
/// 内部确实按缓存价计费。把 0 直接当「0% 命中」会让台账显示错，所以标记
/// `cache_unknown`，让上层知道这项是未知而不是真的没命中。
fn tokens_of(u: &Value) -> Option<Value> {
    let n = |keys: &[&str]| -> i64 {
        keys.iter()
            .find_map(|k| u.get(*k))
            .and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)))
            .unwrap_or(0)
    };
    let prompt = n(&["prompt_tokens", "input_tokens", "promptTokens", "inputTokens"]);
    let completion = n(&["completion_tokens", "output_tokens", "completionTokens", "outputTokens"]);
    if prompt == 0 && completion == 0 {
        return None;
    }
    let total = {
        let t = n(&["total_tokens", "totalTokens"]);
        if t > 0 { t } else { prompt + completion }
    };
    let mut out = Map::new();
    out.insert("prompt_tokens".into(), json!(prompt));
    out.insert("completion_tokens".into(), json!(completion));
    out.insert("total_tokens".into(), json!(total));

    let cached = u
        .get("prompt_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(Value::as_i64)
        .unwrap_or(0)
        .max(n(&["prompt_cache_hit_tokens", "cached_tokens"]))
        .max(n(&["cache_read_input_tokens", "cacheReadInputTokens"]));
    let written = n(&["cache_creation_input_tokens", "cacheCreationInputTokens"]);
    if cached > 0 || written > 0 {
        let mut details = Map::new();
        details.insert("cached_tokens".into(), json!(cached));
        if written > 0 {
            details.insert("cache_write_tokens".into(), json!(written));
        }
        out.insert("prompt_tokens_details".into(), Value::Object(details));
    } else {
        out.insert("cache_unknown".into(), json!(true));
    }
    let reasoning = n(&["reasoning_tokens"])
        .max(u.get("completion_tokens_details").and_then(|d| d.get("reasoning_tokens")).and_then(Value::as_i64).unwrap_or(0));
    if reasoning > 0 {
        out.insert("completion_tokens_details".into(), json!({ "reasoning_tokens": reasoning }));
    }
    Some(Value::Object(out))
}

/// 推理内容去重：Trae 会把同一段推理重发一遍，也可能一次发整段快照。
struct Thoughts {
    all: String,
    last: String,
}

impl Thoughts {
    fn new() -> Self {
        Self { all: String::new(), last: String::new() }
    }

    fn add(&mut self, chunk: &str) -> String {
        if chunk == self.last && chunk.chars().count() >= 8 {
            return String::new();
        }
        if self.all.chars().count() >= 16 && chunk.chars().count() >= 16 && self.all.ends_with(chunk) {
            return String::new();
        }
        let mut piece = chunk.to_string();
        if self.all.chars().count() >= 16 && chunk.starts_with(&self.all) {
            piece = chunk.chars().skip(self.all.chars().count()).collect();
        }
        self.all.push_str(&piece);
        self.last = chunk.to_string();
        piece
    }
}

/// 原生工具调用：Trae 分多个事件流式给出同一个调用，先是名字、再是不断
/// 增长的参数快照（或增量片段），所以要按 id / index 归并，等整条答完再发。
struct NativeCalls {
    calls: Vec<NativeCall>,
}

struct NativeCall {
    id: String,
    index: Option<i64>,
    name: String,
    arguments: String,
}

impl NativeCalls {
    fn new() -> Self {
        Self { calls: Vec::new() }
    }

    fn push(&mut self, tc: &Value) {
        let f = tc.get("function").or_else(|| tc.get("function_call")).unwrap_or(tc);
        let name = f
            .get("name")
            .and_then(Value::as_str)
            .or_else(|| tc.get("tool_name").and_then(Value::as_str))
            .unwrap_or("")
            .to_string();
        let index = tc.get("index").and_then(Value::as_i64);
        let args = f
            .get("arguments")
            .or_else(|| f.get("args"))
            .or_else(|| tc.get("params"))
            .or_else(|| tc.get("input"))
            .or_else(|| tc.get("parameters"))
            .map(|v| if v.is_string() { v.as_str().unwrap_or("").to_string() } else { v.to_string() })
            .unwrap_or_default();
        let id = tc
            .get("id")
            .and_then(Value::as_str)
            .or_else(|| tc.get("tool_call_id").and_then(Value::as_str))
            .unwrap_or("")
            .to_string();
        if name.is_empty() && index.is_none() && id.is_empty() && args.is_empty() {
            return;
        }

        let found = self
            .calls
            .iter()
            .position(|c| (!id.is_empty() && c.id == id) || (id.is_empty() && index.is_some() && c.index == index));
        let found = match found {
            Some(i) => Some(i),
            None if id.is_empty() && index.is_none() => {
                // 没带 id 也没带 index 的片段，归给还在写的那个调用。
                self.calls.iter().rposition(|c| {
                    !is_done(&c.arguments)
                        && (c.name == name || name.is_empty())
                        && (c.arguments == args || args.starts_with(&c.arguments))
                })
            }
            None => None,
        };
        let Some(i) = found else {
            if name.is_empty() {
                return;
            }
            self.calls.push(NativeCall { id, index, name, arguments: args });
            return;
        };

        let call = &mut self.calls[i];
        if call.name.is_empty() {
            call.name = name;
        } else if !name.is_empty() && name.starts_with(&call.name) && name != call.name {
            call.name = name;
        }
        if call.index.is_none() && index.is_some() {
            call.index = index;
        }
        let prev = call.arguments.clone();
        if args.is_empty() || prev.starts_with(&args) {
            return;
        }
        if args.starts_with(&prev) || is_done(&prev) {
            call.arguments = args;
        } else {
            call.arguments.push_str(&args);
        }
    }

    fn take(&self) -> Vec<(String, String, String)> {
        self.calls
            .iter()
            .filter(|c| !c.name.is_empty())
            .map(|c| {
                (
                    if c.id.is_empty() { format!("call_{}", uuid::Uuid::new_v4().simple()) } else { c.id.clone() },
                    c.name.clone(),
                    if c.arguments.trim().is_empty() { "{}".to_string() } else { c.arguments.clone() },
                )
            })
            .collect()
    }
}

/// 参数是否已经收全。JSON 对象写完就是收全。
fn is_done(args: &str) -> bool {
    let t = args.trim();
    t.ends_with('}') && serde_json::from_str::<Value>(t).is_ok()
}

/// 文本写出来的工具调用。
///
/// 只处理我们自己发给它的那一种 `<tool_call>{json}</tool_call>`，以及 GLM 的
/// `<tool_call>name<arg_key>k</arg_key><arg_value>v</arg_value></tool_call>` 模板。
/// 参考实现还认 DeepSeek DSML、Qwen、Anthropic 等方言，这里没移植——原生工具
/// 调用是主路径，这些方言只在模型无视原生定义时才会出现。
struct TextTools {
    buf: String,
    fence: bool,
}

impl TextTools {
    fn new() -> Self {
        Self { buf: String::new(), fence: false }
    }

    fn push(&mut self, s: &str, end: bool) -> Vec<TextOut> {
        self.buf.push_str(s);
        let mut out = Vec::new();
        // 卡在一段可能长成开标签的尾巴上时，先不发，等下一个事件补齐。
        let mut holding = false;
        loop {
            let lower = self.buf.to_ascii_lowercase();
            let Some(open) = lower.find("<tool_") else {
                break;
            };
            // 标签前面的文本正常输出；顺手统计代码围栏，围栏里的不算调用。
            let head = self.buf[..open].to_string();
            self.note(&head);
            if !head.is_empty() {
                out.push(TextOut::Text(head));
            }
            self.buf = self.buf[open..].to_string();

            let lower = self.buf.to_ascii_lowercase();
            let Some(close) = lower.find("</tool_call>").or_else(|| lower.find("</toolcall>")) else {
                if end {
                    let rest = std::mem::take(&mut self.buf);
                    if self.fence {
                        out.push(TextOut::Text(rest));
                    } else {
                        out.push(TextOut::Error(
                            "模型写出的工具调用被截断，不作为回答发出去".to_string(),
                        ));
                    }
                } else {
                    // 开标签之后还没收尾：整块留着，别把半个调用当正文发出去。
                    holding = true;
                }
                break;
            };
            let close_len = if lower[close..].starts_with("</tool_call>") { 12 } else { 11 };
            let body = self.buf[..close].to_string();
            let body = body
                .replacen("<tool_call>", "", 1)
                .replacen("<toolcall>", "", 1)
                .trim()
                .to_string();
            self.buf = self.buf[close + close_len..].to_string();
            if self.fence {
                out.push(TextOut::Text(format!("<tool_call>{body}</tool_call>")));
                continue;
            }
            match parse_text_call(&body) {
                Some((name, args)) => out.push(TextOut::Call(name, args)),
                None => out.push(TextOut::Text(format!("<tool_call>{body}</tool_call>"))),
            }
        }
        if !holding && !self.buf.is_empty() {
            // 没找到开标签：能确定是正文的部分发出去，只把可能是标签前缀的
            // 尾巴留住（`<`、`<to`、`<tool_ca`…）。
            let hold = if end { 0 } else { partial_open(&self.buf) };
            let cut = self.buf.len() - hold;
            if cut > 0 {
                let text = self.buf[..cut].to_string();
                self.buf = self.buf[cut..].to_string();
                self.note(&text);
                out.push(TextOut::Text(text));
            }
        }
        out
    }

    /// 记下代码围栏的开合状态：围栏里的 `<tool_call>` 是正文，不是调用。
    fn note(&mut self, text: &str) {
        self.fence ^= text.matches("```").count() % 2 == 1;
    }
}

/// 缓冲区末尾可能是开标签前缀的长度（要留住，不能当正文发）。
fn partial_open(buf: &str) -> usize {
    const TAGS: [&str; 2] = ["<tool_call>", "<toolcall>"];
    let Some(lt) = buf.rfind('<') else { return 0 };
    let tail = &buf[lt..];
    if TAGS.iter().any(|t| t.starts_with(tail)) {
        buf.len() - lt
    } else {
        0
    }
}

fn parse_text_call(body: &str) -> Option<(String, String)> {
    // ① 整段就是一个 JSON：{"name":…,"arguments":{…}}
    if let Ok(v) = serde_json::from_str::<Value>(body) {
        let name = v.get("name").and_then(Value::as_str).unwrap_or("").to_string();
        if !name.is_empty() {
            let args = v.get("arguments").or_else(|| v.get("parameters")).cloned().unwrap_or_else(|| json!({}));
            let args = if args.is_string() { args.as_str().unwrap_or("{}").to_string() } else { args.to_string() };
            return Some((name, args));
        }
    }
    // ② GLM 的模板：第一行是名字，后面全是 arg_key/arg_value 对
    let head_end = body.find("<arg_key>")?;
    let name = body[..head_end].trim().lines().next().unwrap_or("").trim().to_string();
    if name.is_empty() || name.contains('{') {
        return None;
    }
    let mut args = Map::new();
    let rest = &body[head_end..];
    let mut cursor = rest;
    while let Some(k_start) = cursor.find("<arg_key>") {
        let after_key = &cursor[k_start + "<arg_key>".len()..];
        let Some(k_end) = after_key.find("</arg_key>") else { break };
        let key = after_key[..k_end].trim().to_string();
        let after_val_open = &after_key[k_end + "</arg_key>".len()..];
        let Some(v_open) = after_val_open.find("<arg_value>") else { break };
        let after_val = &after_val_open[v_open + "<arg_value>".len()..];
        let Some(v_end) = after_val.find("</arg_value>") else { break };
        let raw = after_val[..v_end].to_string();
        let value = serde_json::from_str::<Value>(&raw).unwrap_or(json!(raw));
        args.insert(key, value);
        cursor = &after_val[v_end + "</arg_value>".len()..];
    }
    if args.is_empty() {
        return None;
    }
    Some((name, Value::Object(args).to_string()))
}

/// 我们发出去的请求里声明了哪些工具，用来校正模型写错的大小写/标点。
fn tool_named(name: &str, tools: &[Value]) -> String {
    let norm = |s: &str| s.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_ascii_lowercase();
    let want = norm(name);
    for t in tools {
        let f = t.get("function").unwrap_or(t);
        if let Some(real) = f.get("name").and_then(Value::as_str) {
            if norm(real) == want {
                return real.to_string();
            }
        }
    }
    name.to_string()
}

// ---------------------------------------------------------------------------
// 对外：一次对话
// ---------------------------------------------------------------------------

/// 转换完的响应。流式给的是**已经是 OpenAI 格式**的 SSE 字节。
pub enum Reply {
    Json(Value),
    Sse(mpsc::Receiver<Result<Bytes, std::convert::Infallible>>),
}

/// 一次对话的结果：文本、推理、工具调用、用量、结束原因，或错误。
#[derive(Default)]
struct Answer {
    text: String,
    reasoning: String,
    calls: Vec<(String, String, String)>,
    usage: Option<Value>,
    finish: String,
    error: Option<(String, String)>,
}

/// 逐个事件解析成 Answer 的五种成分。
struct Converter {
    reader: SseReader,
    tools: Vec<Value>,
    text_tools: TextTools,
    native: NativeCalls,
    thoughts: Thoughts,
    usage: Option<Value>,
    finish: String,
}

impl Converter {
    fn new(reader: SseReader, tools: Vec<Value>) -> Self {
        Self {
            reader,
            tools,
            text_tools: TextTools::new(),
            native: NativeCalls::new(),
            thoughts: Thoughts::new(),
            usage: None,
            finish: String::new(),
        }
    }

    /// 读一个事件，返回这一次要发出去的东西。None 表示流结束。
    async fn step(&mut self) -> Option<Vec<TextOut>> {
        let ev = self.reader.next_event().await?;
        Some(self.handle(ev))
    }

    fn handle(&mut self, ev: Ev) -> Vec<TextOut> {
        let mut out = Vec::new();
        let name = event_name(&ev.name);
        if ev.data == Value::String("[DONE]".into()) {
            self.finish = self.finish.clone();
            return out;
        }
        let d = match &ev.data {
            Value::Object(o) => o.clone(),
            _ => Map::new(),
        };

        let (code, message) = error_of(&Value::Object(d.clone()));
        let has_body = d.contains_key("response") || d.contains_key("content");
        if name == "error" || (name != "output" && !code.is_empty() && !message.is_empty() && !has_body) {
            let msg = if message.is_empty() { "Trae 返回了错误".to_string() } else { message };
            out.push(TextOut::Error(msg));
            return out;
        }
        if name == "token_usage" {
            let usage = d.get("usage").cloned().unwrap_or_else(|| Value::Object(d.clone()));
            if let Some(u) = tokens_of(&usage) {
                self.usage = Some(u);
            }
            return out;
        }
        if matches!(name.as_str(), "done" | "response_done" | "stream_done") {
            if let Some(u) = d.get("usage").and_then(tokens_of) {
                self.usage = Some(u);
            }
            let f = finish_of(&Value::Object(d.clone()));
            if !f.is_empty() {
                self.finish = f;
            }
            return out;
        }
        if !is_answer_event(&name) {
            return out;
        }

        let delta = d.get("delta").cloned().unwrap_or(Value::Null);
        // 推理字段按 Trae 下发的顺序找，空串不算（它常在第一个字段塞空串）。
        let said = [
            d.get("reasoning_content").and_then(Value::as_str),
            d.get("reasoning").and_then(Value::as_str),
            delta.get("reasoning_content").and_then(Value::as_str),
            delta.get("reasoning").and_then(Value::as_str),
        ]
        .into_iter()
        .flatten()
        .find(|s| !s.is_empty())
        .unwrap_or("")
        .to_string();
        if !said.is_empty() {
            let piece = self.thoughts.add(&said);
            if !piece.is_empty() {
                out.push(TextOut::Thinking(piece));
            }
        }

        let text = [
            d.get("response").and_then(Value::as_str),
            d.get("content").and_then(Value::as_str),
            delta.get("content").and_then(Value::as_str),
            d.get("delta").and_then(Value::as_str),
        ]
        .into_iter()
        .flatten()
        .next()
        .unwrap_or("")
        .to_string();

        // IDE 自己的进度提示，不是模型说的话。
        if text.starts_with("Building prompt:") || text.starts_with("Completed building prompt") {
            // 丢掉
        } else if !text.is_empty() {
            out.extend(self.text_tools.push(&text, false));
        }

        if let Some(calls) = d.get("tool_calls").and_then(Value::as_array) {
            for c in calls {
                self.native.push(c);
            }
        }
        if let Some(u) = d.get("usage").and_then(tokens_of) {
            self.usage = Some(u);
        }
        let f = finish_of(&Value::Object(d));
        if !f.is_empty() {
            self.finish = f;
        }
        out
    }

    /// 流结束：把缓冲里的文本、以及所有原生调用交出去。
    fn finish(&mut self) -> Vec<TextOut> {
        let mut out = self.text_tools.push("", true);
        for (id, name, args) in self.native.take() {
            out.push(TextOut::NativeCall(id, name, args));
        }
        out
    }

    fn rename(&self, name: &str) -> String {
        tool_named(name, &self.tools)
    }
}

enum TextOut {
    Text(String),
    Thinking(String),
    /// 从正文里解析出来的调用：还没有 id，出去时再补。
    Call(String, String),
    /// 原生调用，Trae 自己给了 id。
    NativeCall(String, String, String),
    Error(String),
}

// 两种调用形态统一成 (id, name, args)
impl TextOut {
    fn call_parts(&self) -> Option<(String, String, String)> {
        match self {
            TextOut::Call(name, args) => {
                Some((format!("call_{}", uuid::Uuid::new_v4().simple()), name.clone(), args.clone()))
            }
            TextOut::NativeCall(id, name, args) => Some((id.clone(), name.clone(), args.clone())),
            _ => None,
        }
    }
}

/// Trae 的一次对话。
///
/// `stream` 只影响返回形状：Trae 侧永远是 SSE，非流式由这里聚合。
pub async fn forward(body: &Value, model: &str, stream: bool) -> Result<Reply, (u16, String)> {
    let acc = fresh().await.map_err(|e| (401, e))?;
    let r = realm(&acc.realm).ok_or((500, "账号记录的 realm 不认识".to_string()))?;

    // 目录只用来找「这个模型该走哪个 function、报什么 __dev 名」。这里只读
    // 缓存，不读网络：拉不到就退回默认的第一个 function，请求照发。
    let cat = cached_catalog().await;
    let route = cat
        .as_ref()
        .and_then(|c| c.routes.get(model).cloned())
        .unwrap_or_else(|| Route { function: r.functions[0].to_string(), model_name: String::new(), most: 0 });
    let sees = cat
        .as_ref()
        .and_then(|c| c.models.iter().find(|m| m.id == model).map(|m| m.sees))
        .unwrap_or(false);
    let session = body
        .get("user")
        .and_then(Value::as_str)
        .map(|u| format!("{}:{u}", acc.uid))
        .unwrap_or_else(|| acc.uid.clone());

    let host = model_host(&acc, r);
    let url = format!("{host}/api/agent/v3/llm_utils_chat");
    let client = client_for(r);
    let headers = ide_headers(&acc, r);
    let tools = body.get("tools").and_then(Value::as_array).cloned().unwrap_or_default();

    // 这个模型不在 chat_v3 的菜里时，换下一个 function 再试。
    let mut order: Vec<String> = Vec::new();
    if !route.function.is_empty() {
        order.push(route.function.clone());
    }
    for f in r.functions {
        if !order.iter().any(|x| x == f) {
            order.push((*f).to_string());
        }
    }

    let mut last: Option<(u16, String)> = None;
    for function in order {
        let payload = chat_body(body, &function, model, &route, &session, sees);
        let mut req = apply_headers(client.post(&url), &headers)
            .header("Accept", "text/event-stream")
            .json(&payload);
        req = req.timeout(Duration::from_secs(if stream { 600 } else { 300 }));
        let resp = match req.send().await {
            Ok(resp) => resp,
            Err(e) => {
                last = Some((502, format!("连接 Trae 失败：{e}")));
                continue;
            }
        };

        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            let text = resp.text().await.unwrap_or_default();
            let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            let (code, message) = error_of(&v);
            let msg = if message.is_empty() { text.chars().take(300).collect() } else { message };
            let mapped = if lapsed_code(&code) || status == 401 {
                401
            } else if quota_code(&code) || status == 429 {
                429
            } else {
                status
            };
            last = Some((mapped, format!("{}: {msg}", r.name)));
            // 4001/4023/1005 是这个 function 不接这个模型，换一个可能就行。
            if !(status == 400 && wrong_function(&code)) && !wrong_function(&code) {
                break;
            }
            continue;
        }

        let content_type = resp.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
        if !content_type.contains("event-stream") && content_type.contains("json") {
            let text = resp.text().await.unwrap_or_default();
            let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            let (code, message) = error_of(&v);
            last = Some((502, format!("{}: {message}", r.name)));
            if wrong_function(&code) {
                continue;
            }
            break;
        }

        if stream {
            return Ok(Reply::Sse(start_stream(resp, tools, model, body, r.id)));
        }
        return Ok(Reply::Json(collect(resp, tools, model, body, r.id).await?));
    }

    Err(last.unwrap_or((502, format!("{}: 没有可用的 function", r.name))))
}

/// 非流式：把 Trae 的 SSE 全部读完，聚合成一个 OpenAI 响应体。
async fn collect(
    resp: reqwest::Response,
    tools: Vec<Value>,
    model: &str,
    req: &Value,
    realm_id: &str,
) -> Result<Value, (u16, String)> {
    let mut conv = Converter::new(SseReader::new(resp), tools);
    let mut answer = Answer::default();
    while let Some(items) = conv.step().await {
        for item in items {
            apply(&mut answer, item, &conv);
        }
        if answer.error.is_some() {
            break;
        }
    }
    for item in conv.finish() {
        apply(&mut answer, item, &conv);
    }
    finish_answer(&mut answer, &conv);
    if let Some((message, _code)) = &answer.error {
        return Err((502, format!("{realm_id}: {message}")));
    }

    let mut message = Map::new();
    message.insert("role".into(), json!("assistant"));
    if answer.calls.is_empty() {
        message.insert("content".into(), json!(answer.text));
    } else {
        message.insert(
            "content".into(),
            if answer.text.is_empty() { Value::Null } else { json!(answer.text) },
        );
        message.insert(
            "tool_calls".into(),
            json!(answer
                .calls
                .iter()
                .map(|(id, name, args)| json!({
                    "id": id,
                    "type": "function",
                    "function": { "name": name, "arguments": args },
                }))
                .collect::<Vec<_>>()),
        );
    }
    if !answer.reasoning.is_empty() {
        message.insert("reasoning_content".into(), json!(answer.reasoning));
    }

    let finish = if !answer.calls.is_empty() {
        "tool_calls".to_string()
    } else if answer.finish.is_empty() {
        "stop".to_string()
    } else {
        answer.finish.clone()
    };
    let mut out = Map::new();
    out.insert("id".into(), json!(format!("chatcmpl-{}", uuid::Uuid::new_v4().simple())));
    out.insert("object".into(), json!("chat.completion"));
    out.insert("created".into(), json!(chrono::Utc::now().timestamp()));
    out.insert("model".into(), json!(model));
    out.insert(
        "choices".into(),
        json!([{
            "index": 0,
            "message": Value::Object(message),
            "finish_reason": finish,
        }]),
    );
    if let Some(usage) = answer.usage {
        out.insert("usage".into(), usage);
    }
    let _ = req;
    Ok(Value::Object(out))
}

/// 流式：起一个任务把 Trae 的事件翻成 OpenAI 的 chunk。
fn start_stream(
    resp: reqwest::Response,
    tools: Vec<Value>,
    model: &str,
    req: &Value,
    realm_id: &'static str,
) -> mpsc::Receiver<Result<Bytes, std::convert::Infallible>> {
    let (tx, rx) = mpsc::channel::<Result<Bytes, std::convert::Infallible>>(64);
    let id = format!("chatcmpl-{}", uuid::Uuid::new_v4().simple());
    let created = chrono::Utc::now().timestamp();
    let model = model.to_string();
    let want_usage = req.get("stream_options").and_then(|o| o.get("include_usage")).and_then(Value::as_bool).unwrap_or(true);
    let _ = realm_id;

    tokio::spawn(async move {
        let mut conv = Converter::new(SseReader::new(resp), tools);
        let mut calls = 0usize;
        let mut failed = false;
        if tx.send(Ok(chunk(&id, created, &model, json!({ "role": "assistant", "content": "" }), None))).await.is_err() {
            return;
        }
        loop {
            let Some(items) = conv.step().await else { break };
            for item in items {
                match item {
                    TextOut::Text(t) => {
                        if tx.send(Ok(chunk(&id, created, &model, json!({ "content": t }), None))).await.is_err() {
                            return;
                        }
                    }
                    TextOut::Thinking(t) => {
                        if tx.send(Ok(chunk(&id, created, &model, json!({ "reasoning_content": t }), None))).await.is_err() {
                            return;
                        }
                    }
                    other => {
                        if let Some((call_id, name, args)) = other.call_parts() {
                            let name = conv.rename(&name);
                            let delta = json!({
                                "tool_calls": [{
                                    "index": calls,
                                    "id": call_id,
                                    "type": "function",
                                    "function": { "name": name, "arguments": args },
                                }]
                            });
                            calls += 1;
                            if tx.send(Ok(chunk(&id, created, &model, delta, None))).await.is_err() {
                                return;
                            }
                        } else if let TextOut::Error(message) = other {
                            let body = json!({
                                "error": { "message": format!("{realm_id}: {message}"), "type": "api_error" }
                            });
                            let line = format!("data: {body}\n\n");
                            let _ = tx.send(Ok(Bytes::from(line))).await;
                            failed = true;
                        }
                    }
                }
            }
            if failed {
                break;
            }
        }
        if !failed {
            for item in conv.finish() {
                if let Some((call_id, name, args)) = item.call_parts() {
                    let name = conv.rename(&name);
                    let delta = json!({
                        "tool_calls": [{
                            "index": calls,
                            "id": call_id,
                            "type": "function",
                            "function": { "name": name, "arguments": args },
                        }]
                    });
                    calls += 1;
                    if tx.send(Ok(chunk(&id, created, &model, delta, None))).await.is_err() {
                        return;
                    }
                } else if let TextOut::Error(message) = item {
                    let body = json!({
                        "error": { "message": format!("{realm_id}: {message}"), "type": "api_error" }
                    });
                    let _ = tx.send(Ok(Bytes::from(format!("data: {body}\n\n")))).await;
                    failed = true;
                }
            }
        }
        if !failed {
            let finish = if calls > 0 {
                "tool_calls"
            } else if conv.finish.is_empty() {
                "stop"
            } else {
                conv.finish.as_str()
            };
            let _ = tx.send(Ok(chunk(&id, created, &model, json!({}), Some(finish)))).await;
            if want_usage {
                if let Some(usage) = conv.usage.clone() {
                    let body = json!({
                        "id": id, "object": "chat.completion.chunk", "created": created,
                        "model": model, "choices": [], "usage": usage,
                    });
                    let _ = tx.send(Ok(Bytes::from(format!("data: {body}\n\n")))).await;
                }
            }
        }
        let _ = tx.send(Ok(Bytes::from("data: [DONE]\n\n"))).await;
    });
    rx
}

fn chunk(id: &str, created: i64, model: &str, delta: Value, finish: Option<&str>) -> Bytes {
    let body = json!({
        "id": id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }],
    });
    Bytes::from(format!("data: {body}\n\n"))
}

fn apply(answer: &mut Answer, item: TextOut, conv: &Converter) {
    match item {
        TextOut::Text(t) => answer.text.push_str(&t),
        TextOut::Thinking(t) => answer.reasoning.push_str(&t),
        TextOut::Error(message) => {
            if answer.error.is_none() {
                answer.error = Some((message, String::new()));
            }
        }
        other => {
            if let Some((id, name, args)) = other.call_parts() {
                answer.calls.push((id, conv.rename(&name), args));
            }
        }
    }
}

fn finish_answer(answer: &mut Answer, conv: &Converter) {
    if let Some(usage) = conv.usage.clone() {
        answer.usage = Some(usage);
    }
    if !conv.finish.is_empty() {
        answer.finish = conv.finish.clone();
    }
}

// ---------------------------------------------------------------------------
// 视图
// ---------------------------------------------------------------------------

pub fn view() -> Value {
    let cfg = config();
    let acc = account();
    let r = realm(&cfg.realm).or(Some(&REALM_CN));
    let fallback: Vec<Value> = r
        .map(|r| {
            r.fallback
                .iter()
                .map(|m| json!({ "id": m.id, "name": m.name, "context": m.context, "sees": m.sees }))
                .collect()
        })
        .unwrap_or_default();
    json!({
        "enabled": cfg.enabled,
        "realm": cfg.realm,
        "realms": realm_ids()
            .iter()
            .filter_map(|id| realm(id))
            .map(|r| json!({ "id": r.id, "name": r.name, "web": r.web, "endpoint": r.api }))
            .collect::<Vec<_>>(),
        "models": cfg.models,
        "prefix": PREFIX,
        "provider": "trae",
        "signedIn": acc.is_some(),
        "accountName": acc.as_ref().map(|a| if a.name.is_empty() { a.uid.clone() } else { a.name.clone() }).unwrap_or_default(),
        "accountUid": acc.as_ref().map(|a| a.uid.clone()).unwrap_or_default(),
        "accountRealm": acc.as_ref().map(|a| a.realm.clone()).unwrap_or_default(),
        "expiresAt": acc.as_ref().map(|a| a.expires).unwrap_or(0),
        "deviceId": acc.as_ref().map(|a| a.device_id.clone()).unwrap_or_default(),
        "fallbackModels": fallback,
        "signIn": sign_in_state(),
    })
}

pub fn save(input: &Value) -> Result<Value, String> {
    let mut cfg = config();
    if let Some(enabled) = input.get("enabled").and_then(Value::as_bool) {
        cfg.enabled = enabled;
    }
    if let Some(id) = input.get("realm").and_then(Value::as_str) {
        if realm(id.trim()).is_none() {
            return Err(format!("未知的 Trae 部署：{id}"));
        }
        cfg.realm = id.trim().to_string();
    }
    if let Some(list) = input.get("models").and_then(Value::as_array) {
        cfg.models = list.iter().filter_map(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect();
    }
    if cfg.enabled && cfg.models.is_empty() {
        return Err("启用前至少要勾选一个模型".to_string());
    }
    if cfg.enabled && account().is_none() {
        return Err("还没有登录 Trae，请先登录再启用".to_string());
    }
    save_config_merged(&cfg)?;
    Ok(view())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_names_are_normalised() {
        assert_eq!(event_name("TokenUsage"), "token_usage");
        assert_eq!(event_name("token-usage"), "token_usage");
        assert_eq!(event_name("output"), "output");
        assert_eq!(event_name(""), "");
        assert_eq!(event_name("soloAgentDelta"), "solo_agent_delta");
    }

    /// 一个包。`limit` 为 None 时不带 `credits_limit`（免费包就是这样）。
    fn pack(label: &str, group: &str, limit: Option<i64>, used: Option<i64>, end: i64) -> Value {
        let mut quota = serde_json::Map::new();
        if let Some(l) = limit {
            quota.insert("credits_limit".into(), json!(l));
        }
        let mut usage = serde_json::Map::new();
        if let Some(u) = used {
            // 用过的包才有 credits_amount；没用过时这个对象是空的。
            usage.insert("credits_amount".into(), json!(u));
        }
        json!({
            "display_desc": label,
            "group_name": group,
            "entitlement_base_info": {
                "quota": Value::Object(quota),
                "start_time": end - 2_592_000,
                "end_time": end,
            },
            "usage": Value::Object(usage),
        })
    }

    /// 一组未来时间戳（秒）。用远未来避免测试随真实日期腐坏。
    ///
    /// 注意：断言时不要把 `future(n)` 再算一遍去比，跨秒会抖；从输出的
    /// `packs[].expiresAt` 里取回期望值。
    fn future(days: i64) -> i64 {
        chrono::Utc::now().timestamp() + days * 86_400
    }

    /// 从输出里取第 i 个包的到期时间（毫秒），作为「最早到期」的期望值。
    fn pack_expiry(v: &Value, i: usize) -> Option<i64> {
        v["packs"][i]["expiresAt"].as_i64()
    }

    #[test]
    fn entitlement_adds_up_finite_packs() {
        // 形状照真实返回：一个月登录赠送 500 + 三个签到 100，外加一个没有
        // credits_limit 的免费包（它只带功能开关，不参与额度累加）。
        let raw = json!({
            "total_balance": 800,
            "usage_summary": { "consumed_amount": 0, "consumption_ratio": 0, "total_amount": 800 },
            "user_entitlement_pack_list": [
                pack("免费", "", None, None, future(15)),
                pack("每月登录赠送", "每月登录积分", Some(500), None, future(15)),
                pack("签到奖励", "每日签到", Some(100), None, future(9)),
                pack("签到奖励", "每日签到", Some(100), None, future(11)),
                pack("签到奖励", "每日签到", Some(100), None, future(31)),
            ],
        });
        let v = summarize_entitlement(&raw);
        assert_eq!(v["ok"], json!(true));
        assert_eq!(v["total"], json!(800.0));
        assert_eq!(v["used"], json!(0.0));
        assert_eq!(v["left"], json!(800.0));
        assert_eq!(v["unlimited"], json!(false));
        // 免费包没有上限，不该占一个到期位；最早到期是第 2 个包（未来 9 天）。
        assert_eq!(v["expiresAt"].as_i64(), pack_expiry(&v, 2));
        assert_eq!(v["lapsing"], json!(100.0));
        assert_eq!(v["packs"].as_array().map(|a| a.len()), Some(5));
        // 没有上限的包输出 null，前端据此说「不计额度」。
        assert!(v["packs"][0]["limit"].is_null());
    }

    #[test]
    fn entitlement_treats_minus_one_as_unlimited() {
        let raw = json!({
            "user_entitlement_pack_list": [
                pack("企业版", "订阅", Some(-1), Some(4000), future(30)),
                pack("签到奖励", "每日签到", Some(100), None, future(5)),
            ],
        });
        let v = summarize_entitlement(&raw);
        assert_eq!(v["unlimited"], json!(true));
        // -1 不能当负数累加掉，有限额的那 100 仍要算上。
        assert_eq!(v["total"], json!(100.0));
        assert_eq!(v["used"], json!(4000.0));
        // 无限包不计 rest，最早到期落在签到包上（第 1 个）。
        assert_eq!(v["expiresAt"].as_i64(), pack_expiry(&v, 1));
        assert_eq!(v["lapsing"], json!(100.0));
        assert!(v["packs"][0]["limit"].is_null());
    }

    #[test]
    fn entitlement_ignores_spent_or_expired_packs_for_renewal() {
        let raw = json!({
            "user_entitlement_pack_list": [
                // 已经用完：到期与否都不改变额度，不该当续期日。
                pack("签到奖励", "每日签到", Some(100), Some(100), future(2)),
                // 已经过期。
                pack("签到奖励", "每日签到", Some(100), None, chrono::Utc::now().timestamp() - 86_400),
                // 这个才是真正还有剩余、且尚未到期的最近一个。
                pack("每月登录赠送", "每月登录积分", Some(500), Some(80), future(20)),
            ],
        });
        let v = summarize_entitlement(&raw);
        assert_eq!(v["total"], json!(700.0));
        assert_eq!(v["used"], json!(180.0));
        assert_eq!(v["expiresAt"].as_i64(), pack_expiry(&v, 2));
        assert_eq!(v["lapsing"], json!(420.0));
    }

    #[test]
    fn entitlement_prefers_summary_over_pack_sum() {
        // 顶层 usage_summary 是账单页上显示的那个数，与包累加不一致时以它为准。
        let raw = json!({
            "total_balance": 615.5,
            "usage_summary": { "consumed_amount": 184.5, "consumption_ratio": 23, "total_amount": 800 },
            "user_entitlement_pack_list": [ pack("签到奖励", "每日签到", Some(100), Some(50), future(10)) ],
        });
        let v = summarize_entitlement(&raw);
        assert_eq!(v["total"], json!(800.0));
        assert_eq!(v["used"], json!(184.5));
        // left 用顶层余额，而不是 total - used 重算。
        assert_eq!(v["left"], json!(615.5));
        assert_eq!(v["ratio"], json!(184.5 / 800.0));
    }

    #[test]
    fn entitlement_survives_empty_answer() {
        // 没登录/接口改版时不能 panic，界面靠 ok 字段决定显示什么。
        let v = summarize_entitlement(&json!({}));
        assert_eq!(v["ok"], json!(true));
        assert_eq!(v["total"], json!(0.0));
        assert_eq!(v["used"], json!(0.0));
        assert_eq!(v["packs"].as_array().map(|a| a.len()), Some(0));
        assert!(v["expiresAt"].is_null());
    }

    #[test]
    fn finish_reasons_map_to_openai() {
        assert_eq!(finish_of(&json!({"finish_reason":"max_tokens"})), "length");
        assert_eq!(finish_of(&json!({"stop_reason":"sensitive"})), "content_filter");
        assert_eq!(finish_of(&json!({"finish_reason":"stop"})), "");
    }

    #[test]
    fn cache_counts_are_marked_unknown_when_trae_gives_none() {
        let u = tokens_of(&json!({"prompt_tokens":1717,"completion_tokens":2,"total_tokens":1719})).unwrap();
        assert_eq!(u["cache_unknown"], true);
        assert!(u.get("prompt_tokens_details").is_none());

        let u = tokens_of(&json!({"prompt_tokens":10,"completion_tokens":3,"cache_read_input_tokens":8})).unwrap();
        assert_eq!(u["prompt_tokens_details"]["cached_tokens"], 8);
    }

    #[test]
    fn token_counts_ignore_empty_events() {
        assert!(tokens_of(&json!({"prompt_tokens":0,"completion_tokens":0})).is_none());
    }

    #[test]
    fn reasoning_is_deduplicated() {
        let mut th = Thoughts::new();
        assert_eq!(th.add("hello world!!"), "hello world!!");
        assert_eq!(th.add("hello world!!"), "");
    }

    #[test]
    fn text_tool_call_is_parsed() {
        let mut tt = TextTools::new();
        let out = tt.push("<tool_call>{\"name\":\"bash\",\"arguments\":{\"command\":\"ls\"}}</tool_call>", true);
        let calls: Vec<_> = out.iter().filter_map(TextOut::call_parts).collect();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].1, "bash");
        assert!(calls[0].2.contains("ls"));
    }

    #[test]
    fn glm_style_text_call_is_parsed() {
        let mut tt = TextTools::new();
        let out = tt.push(
            "<tool_call>bash<arg_key>command</arg_key><arg_value>ls -l</arg_value></tool_call>",
            true,
        );
        let calls: Vec<_> = out.iter().filter_map(TextOut::call_parts).collect();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].1, "bash");
        assert!(calls[0].2.contains("ls -l"));
    }

    #[test]
    fn truncated_call_is_an_error_not_an_answer() {
        let mut tt = TextTools::new();
        let out = tt.push("<tool_call>{\"name\":\"bash\",\"arguments\":{\"comm", true);
        assert!(out.iter().any(|o| matches!(o, TextOut::Error(_))));
    }

    #[test]
    fn plain_text_passes_through() {
        let mut tt = TextTools::new();
        let out = tt.push("hello ", false);
        let text: String = out
            .iter()
            .filter_map(|o| if let TextOut::Text(t) = o { Some(t.clone()) } else { None })
            .collect();
        assert_eq!(text, "hello ");
    }

    #[test]
    fn consecutive_assistant_turns_fold_into_one() {
        let req = json!({
            "messages": [
                { "role": "system", "content": "be nice" },
                { "role": "user", "content": "hi" },
                { "role": "assistant", "content": "let me look" },
                { "role": "assistant", "content": null, "tool_calls": [
                    { "id": "c1", "type": "function", "function": { "name": "bash", "arguments": "{\"command\":\"ls\"}" } }
                ]},
                { "role": "tool", "tool_call_id": "c1", "content": "a.txt" }
            ],
            "tools": [{ "type": "function", "function": { "name": "bash", "parameters": { "type": "object" } } }]
        });
        let out = trae_messages(&req, false);
        // system / user / assistant(folded) / tool
        assert_eq!(out.len(), 4, "{out:#?}");
        assert_eq!(out[2]["role"], "assistant");
        assert!(out[2]["tool_calls"].is_array());
        assert_eq!(out[3]["role"], "tool");
    }

    #[test]
    fn empty_tool_result_gets_a_placeholder() {
        let req = json!({
            "messages": [
                { "role": "user", "content": "hi" },
                { "role": "assistant", "content": null, "tool_calls": [
                    { "id": "c1", "type": "function", "function": { "name": "bash", "arguments": "{}" } }
                ]},
                { "role": "tool", "tool_call_id": "c1", "content": "" }
            ],
            "tools": [{ "type": "function", "function": { "name": "bash", "parameters": { "type": "object" } } }]
        });
        let out = trae_messages(&req, false);
        let tool_turn = out.iter().find(|m| m["role"] == "tool").expect("tool turn");
        assert_eq!(tool_turn["content"][0]["text"], "(no output)");
    }

    #[test]
    fn tool_parameters_go_out_as_a_json_string() {
        let tools = vec![json!({
            "type": "function",
            "function": { "name": "bash", "description": "run", "parameters": { "type": "object", "properties": {} } }
        })];
        let out = native_tools(&tools);
        assert!(out[0]["function"]["parameters"].is_string());
    }

    #[test]
    fn effort_maps_only_known_levels() {
        assert_eq!(trae_effort(&json!("low")).as_deref(), Some("light"));
        assert_eq!(trae_effort(&json!("high")).as_deref(), Some("high"));
        assert_eq!(trae_effort(&json!("xhigh")).as_deref(), Some("extra_high"));
        assert_eq!(trae_effort(&json!("turbo")), None);
    }

    #[test]
    fn device_ids_have_the_right_shape() {
        let cn = new_device(&REALM_CN);
        assert_eq!(cn.device_id.len(), 16);
        assert!(cn.device_id.chars().all(|c| c.is_ascii_digit()));
        assert!(!cn.device_id.starts_with('0'));
        assert_eq!(cn.machine_id.len(), 32);
        let global = new_device(&REALM_GLOBAL);
        assert_eq!(global.device_id.len(), 19);
    }

    #[test]
    fn device_key_round_trips_through_pem() {
        let (private, public) = new_device_key().expect("key pair");
        assert!(private.contains("BEGIN PRIVATE KEY"));
        assert!(public.contains("BEGIN PUBLIC KEY"));
        assert_eq!(pem_body(&private).map(|b| b.len() > 100), Some(true));
        // 公钥是固定前缀 + 65 字节未压缩点
        let spki = pem_body(&public).expect("public der");
        assert_eq!(spki.len(), SPKI_P256_PREFIX.len() + 65);
        assert_eq!(&spki[..SPKI_P256_PREFIX.len()], &SPKI_P256_PREFIX);
        assert_eq!(spki[SPKI_P256_PREFIX.len()], 0x04);
    }

    #[test]
    fn proof_signs_the_documented_text() {
        let (private, _public) = new_device_key().expect("key pair");
        let proof = device_proof("/trae/api/v3/oauth/ExchangeToken", "ono9krqynydwx5", "r3f", &private)
            .expect("proof");
        assert!(proof["Signature"].as_str().unwrap().len() > 40);
        assert!(proof["Timestamp"].as_i64().unwrap() > 1_700_000_000);
        assert_eq!(proof["Nonce"].as_str().unwrap().len(), 32);
    }

    #[test]
    fn query_encoding_round_trips() {
        let raw = "http://127.0.0.1:1234/authorize?a=b c&d=中文";
        let encoded = urlencoded(raw);
        assert!(!encoded.contains(' '));
        assert_eq!(urldecoded(&encoded), raw);
    }

    #[test]
    fn jwt_exp_is_read_from_the_payload() {
        // {"exp":1700000000}
        let token = "h.eyJleHAiOjE3MDAwMDAwMDB9.s";
        assert_eq!(jwt_exp(token), 1_700_000_000_000);
        assert_eq!(jwt_exp("not-a-jwt"), 0);
    }

    #[test]
    fn expiry_accepts_seconds_millis_and_iso() {
        assert_eq!(when_of(&json!(1_700_000_000)), 1_700_000_000_000);
        assert_eq!(when_of(&json!(1_700_000_000_000i64)), 1_700_000_000_000);
        assert_eq!(when_of(&json!("2026-11-15T00:00:00Z")), 1_794_700_800_000);
        assert_eq!(when_of(&json!(0)), 0);
    }

    #[test]
    fn sign_in_url_carries_the_local_callback() {
        // 授权页只接受 127.0.0.1 回调，别的主机名它不带令牌。
        let callback = "http://127.0.0.1:4321/authorize";
        let encoded = urlencoded(callback);
        assert!(encoded.starts_with("http%3A%2F%2F127.0.0.1%3A"));
    }

    /// 真起一次登录监听，然后从授权页地址里取出回调端口，模拟浏览器打过去。
    ///
    /// 这条覆盖的是「登录只在本机收尾」的那一整段：绑端口、解析 HTTP、
    /// 解析 query、把结果写进状态机。不需要真账号——授权页拒绝也是一种回调。
    #[tokio::test]
    async fn sign_in_callback_reports_an_error_over_the_local_socket() {
        let started = sign_in_start("trae-cn").await.expect("登录应该能发起");
        let url = started["url"].as_str().expect("授权页地址").to_string();
        let encoded = url
            .split("auth_callback_url=")
            .nth(1)
            .and_then(|rest| rest.split('&').next())
            .expect("授权页地址里应该带回调地址");
        let callback = urldecoded(encoded);
        assert!(callback.starts_with("http://127.0.0.1:"), "回调必须落在 127.0.0.1：{callback}");
        let port: u16 = callback
            .rsplit(':')
            .next()
            .and_then(|p| p.split('/').next())
            .and_then(|p| p.parse().ok())
            .expect("回调端口");

        let mut sock = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.expect("连上回调端口");
        let req = "GET /authorize?error=access_denied&error_description=nope HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n";
        sock.write_all(req.as_bytes()).await.expect("写入请求");
        let mut buf = Vec::new();
        sock.read_to_end(&mut buf).await.expect("读取响应");
        let text = String::from_utf8_lossy(&buf).to_string();
        assert!(text.contains("200 OK"), "回调应该回一个说明页：{text}");
        assert!(text.contains("登录没有完成"), "页面应该说明登录没完成：{text}");

        let mut state = String::new();
        for _ in 0..100 {
            state = sign_in_state()["state"].as_str().unwrap_or("").to_string();
            if state == "failed" || state == "done" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(state, "failed");
        assert_eq!(sign_in_state()["message"].as_str().unwrap_or(""), "nope");
    }
}
