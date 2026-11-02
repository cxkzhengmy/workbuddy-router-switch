//! 反代使用的「当前激活账号」解析。
//!
//! 与旧的 python 网关不同，这里不维护自己的账号池：直接复用项目已有的账号库
//! （`~/.wb-switch/accounts.json`）与官方登录态文件。语义只有一个——
//! **反代始终使用官方客户端的当前登录账号**，用户在项目里切号后立即生效。
//!
//! 为什么先读登录态而不是直接遍历账号库：账号库里可能有多条历史账号，
//! 「哪一条是正在用的」只有官方客户端的 auth 文件能回答。账号库只负责提供
//! 明文 token 与刷新凭据。

use serde_json::{json, Value};

use crate::modules::{account, auth_file, session, variant::WbVariant};

/// 一次请求要用的账号视图。
///
/// 只保留上游请求与记账需要的字段；完整记录留在 `raw` 里供刷新回写。
#[derive(Debug, Clone)]
pub struct ActiveAccount {
    pub uid: String,
    pub token: String,
    pub enterprise_id: String,
    pub domain: String,
    pub variant: WbVariant,
    /// 账号展示名（昵称 / 邮箱），记账与界面用。
    pub label: String,
    pub raw: Value,
}

impl ActiveAccount {
    /// 静态 API Key（`ck_` / `pt_` 开头）。官方 CLI 对这种凭据两个头都发，
    /// 只在 `Authorization` 里带会让 `/v3/config` 少下发 4 个模型，见
    /// wb_identity.build_identity_headers 的注释。
    pub fn is_static_key(&self) -> bool {
        self.token.starts_with("ck_") || self.token.starts_with("pt_")
    }
}

/// 解析当前激活账号。
///
/// 顺序：
/// 1. 官方登录态文件（`workbuddy-desktop*.info`）里的 `account.uid`；
/// 2. 账号库里按 uid 找到完整记录（含 refresh_token）；
/// 3. 账号库里没有这条时，用登录态文件本身转成一条临时记录
///    （`import_from_auth_file` 产出与账号库同构）；
/// 4. 都没有——才回退到账号库里第一条有明文 token 的账号。
///
/// 第 4 步是刻意的：用户可能只用本项目管账号、没装桌面客户端，这时
/// 「当前登录态」不存在，但项目里选中的账号仍然是明确的意图。
pub fn resolve() -> Result<ActiveAccount, String> {
    for variant in WbVariant::ALL {
        let Some(uid) = session::current_user_uid(variant) else {
            continue;
        };
        if let Some(found) = account::find_account(&uid) {
            return from_value(found);
        }
        if let Some(fallback) = auth_file::import_from_auth_file(variant) {
            if fallback.get("uid").and_then(Value::as_str) == Some(uid.as_str()) {
                return from_value(fallback);
            }
        }
    }

    let mut first_error: Option<String> = None;
    for candidate in account::load_accounts() {
        match from_value(candidate) {
            Ok(acc) => return Ok(acc),
            Err(err) => {
                first_error.get_or_insert(err);
            }
        }
    }
    Err(first_error.unwrap_or_else(|| {
        "没有可用账号：请先在「账号」页添加并登录一个 WorkBuddy 账号".to_string()
    }))
}

fn from_value(acc: Value) -> Result<ActiveAccount, String> {
    // 加密信封态无法解出明文 token，任何上游调用都会 401。必须先拦下来，
    // 否则用户看到的只是「网关 502」这种没有信息量的错误。
    if let Some(err) = account::envelope_token_error(&acc) {
        return Err(err);
    }
    let token = account::get_str(&acc, "access_token").unwrap_or_default();
    if token.trim().is_empty() {
        return Err("当前账号缺少 access_token，请重新登录后再试".to_string());
    }
    let uid = account::get_str(&acc, "uid").unwrap_or_else(|| "anonymous".to_string());
    let label = account::account_display_name(&acc);
    Ok(ActiveAccount {
        uid,
        token,
        enterprise_id: account::get_str(&acc, "enterpriseId")
            .or_else(|| account::get_str(&acc, "enterprise_id"))
            .unwrap_or_default(),
        domain: account::get_str(&acc, "domain").unwrap_or_default(),
        variant: WbVariant::from_account(&acc),
        label,
        raw: acc,
    })
}

/// token 失效时刷新一次并返回新账号视图。
///
/// 只由 401 分支调用：正常路径不应为了「保险」而每次刷新，那会给上游
/// 制造无谓的 refresh 流量，反而更容易被风控。
pub async fn refresh(acc: &ActiveAccount) -> Result<ActiveAccount, String> {
    let updated = crate::modules::refresh::refresh_account_token(acc.raw.clone()).await;
    // refresh_account_token 失败时不抛错，而是把 needs_relogin 写进记录。
    if updated
        .get("needs_relogin")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        let reason = updated
            .get("needs_relogin_reason")
            .and_then(Value::as_str)
            .unwrap_or("刷新失败");
        return Err(format!("账号凭据已失效：{reason}"));
    }
    from_value(updated)
}

// ---------------------------------------------------------------------------
// 账号策略：用哪个（或哪些）账号发请求
// ---------------------------------------------------------------------------

/// 策略在 `proxy.db` 的 settings 表里，键名固定。
pub const POLICY_KEY: &str = "account_policy";

/// 读账号策略，补齐默认值。
///
/// 默认 `current`：不配置时行为和以前一致（跟随官方客户端当前登录账号），
/// 只是现在可以改。
pub fn policy() -> Value {
    let raw = crate::modules::proxy::store::load_settings(POLICY_KEY);
    json!({
        "mode": raw.get("mode").and_then(Value::as_str).unwrap_or("current"),
        "fixed": raw.get("fixed").and_then(Value::as_str).unwrap_or(""),
        "pool": raw.get("pool").and_then(Value::as_array).cloned().unwrap_or_default(),
        "strategy": raw.get("strategy").and_then(Value::as_str).unwrap_or("round_robin"),
        "failover": raw.get("failover").and_then(Value::as_bool).unwrap_or(true),
        // 出站身份：上游看到的「哪种官方客户端」。默认跟随桌面端。
        "identity": super::upstream::Identity::parse(
            raw.get("identity").and_then(Value::as_str)
        ).as_str(),
    })
}

/// 保存账号策略。只接受已知取值，未知一律按默认处理——策略错了会导致
/// 请求发到用户没预期的账号上，宁可回落也不要「尽力而为」。
pub fn save_policy(input: &Value) -> Result<Value, String> {
    let mode = match input.get("mode").and_then(Value::as_str) {
        Some("fixed") => "fixed",
        Some("pool") => "pool",
        _ => "current",
    };
    let strategy = match input.get("strategy").and_then(Value::as_str) {
        Some("least_requests") => "least_requests",
        _ => "round_robin",
    };
    let pool: Vec<String> = input
        .get("pool")
        .and_then(Value::as_array)
        .map(|list| list.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default();
    let normalized = json!({
        "mode": mode,
        "fixed": input.get("fixed").and_then(Value::as_str).unwrap_or("").trim(),
        "pool": pool,
        "strategy": strategy,
        "failover": input.get("failover").and_then(Value::as_bool).unwrap_or(true),
        "identity": super::upstream::Identity::parse(
            input.get("identity").and_then(Value::as_str)
        ).as_str(),
    });
    if mode == "fixed" && normalized["fixed"].as_str().unwrap_or("").is_empty() {
        return Err("选择了「指定账号」但没有选具体账号".to_string());
    }
    if mode == "pool" && pool.is_empty() {
        return Err("选择了「账号池」但没有勾选任何账号".to_string());
    }
    // 引用必须现在就能解析到账号。留一个失效引用到运行时报错的话，
    // 用户看到的是「网关 503」，而真正的原因（那个账号被删了）
    // 要翻配置才知道。
    if mode == "fixed" {
        let reference = normalized["fixed"].as_str().unwrap_or("");
        if find_by_ref(reference).is_none() {
            return Err(format!("指定的账号已不存在（{reference}），请重新选择"));
        }
    }
    if mode == "pool" {
        let stale: Vec<&str> = pool
            .iter()
            .map(String::as_str)
            .filter(|reference| find_by_ref(reference).is_none())
            .collect();
        if !stale.is_empty() {
            return Err(format!(
                "账号池里有已失效的账号：{}，请重新勾选",
                stale.join("、")
            ));
        }
    }
    crate::modules::proxy::store::save_settings(POLICY_KEY, &normalized)?;
    Ok(policy())
}

/// 某个引用（uid 或本地 id）对应的账号记录。
fn find_by_ref(reference: &str) -> Option<Value> {
    let reference = reference.trim();
    if reference.is_empty() {
        return None;
    }
    crate::modules::account::find_account(reference)
}

/// 本轮请求可用的账号，按调度策略排好序。
///
/// 返回多个时由调用方负责失败切换；返回空表示策略配置的账号全都不可用，
/// 这时不回落到「当前账号」——用户明确指定了账号，静默换成别的账号
/// 比直接报错更难排查。
pub fn resolve_candidates() -> Result<Vec<ActiveAccount>, String> {
    let policy = policy();
    match policy.get("mode").and_then(Value::as_str).unwrap_or("current") {
        "fixed" => {
            let reference = policy.get("fixed").and_then(Value::as_str).unwrap_or("");
            let record =
                find_by_ref(reference).ok_or_else(|| "指定的账号已不存在，请在网关页重新选择".to_string())?;
            Ok(vec![from_value(record)?])
        }
        "pool" => {
            let picked: Vec<String> = policy
                .get("pool")
                .and_then(Value::as_array)
                .map(|list| list.iter().filter_map(Value::as_str).map(str::to_string).collect())
                .unwrap_or_default();
            let mut list: Vec<ActiveAccount> = Vec::new();
            let mut errors: Vec<String> = Vec::new();
            for reference in &picked {
                match find_by_ref(reference) {
                    Some(record) => match from_value(record) {
                        Ok(acc) => list.push(acc),
                        Err(err) => errors.push(err),
                    },
                    None => errors.push(format!("账号 {reference} 已不存在")),
                }
            }
            if list.is_empty() {
                return Err(if errors.is_empty() {
                    "账号池为空，请在网关页勾选至少一个账号".to_string()
                } else {
                    format!("账号池里没有可用账号：{}", errors.join("；"))
                });
            }
            reorder(&mut list, policy.get("strategy").and_then(Value::as_str).unwrap_or("round_robin"));
            Ok(list)
        }
        _ => Ok(vec![resolve()?]),
    }
}

/// 按策略给候选排序。
///
/// `round_robin` 用进程内计数器做起点轮转——不持久化是刻意的：重启后从
/// 第一个账号开始，比读一个可能已经过时的偏移量更好预测。
fn reorder(list: &mut [ActiveAccount], strategy: &str) {
    if list.len() <= 1 {
        return;
    }
    match strategy {
        "least_requests" => {
            let counts = crate::modules::proxy::store::today_requests_by_account();
            list.sort_by_key(|acc| counts.get(&acc.uid).copied().unwrap_or(0));
        }
        _ => {
            static RR: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let offset = RR.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % list.len();
            list.rotate_left(offset);
        }
    }
}

// ---------------------------------------------------------------------------
// 失败冷却
// ---------------------------------------------------------------------------

fn cooldowns() -> &'static std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>> {
    static MAP: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>,
    > = std::sync::OnceLock::new();
    MAP.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// 把账号短暂移出候选池。401 / 429 / 5xx 都会走到这里：
/// 继续拿同一个账号重试只会让问题更糟（限流加剧或凭据彻底失效）。
pub fn mark_cooldown(uid: &str, seconds: u64) {
    if uid.is_empty() {
        return;
    }
    if let Ok(mut map) = cooldowns().lock() {
        map.insert(uid.to_string(), std::time::Instant::now() + std::time::Duration::from_secs(seconds));
    }
}

pub fn is_cooling(uid: &str) -> bool {
    let Ok(mut map) = cooldowns().lock() else {
        return false;
    };
    match map.get(uid) {
        Some(until) if *until > std::time::Instant::now() => true,
        Some(_) => {
            map.remove(uid);
            false
        }
        None => false,
    }
}

/// 账号清单 + 状态，供网关页展示与勾选。
pub fn accounts_view() -> Value {
    let counts = crate::modules::proxy::store::today_requests_by_account();
    let current_uid = WbVariant::ALL
        .iter()
        .find_map(|variant| crate::modules::session::current_user_uid(*variant))
        .unwrap_or_default();
    let accounts: Vec<Value> = crate::modules::account::load_accounts()
        .iter()
        .map(|acc| {
            let uid = crate::modules::account::get_str(acc, "uid").unwrap_or_default();
            let local_id = crate::modules::account::get_str(acc, "id").unwrap_or_default();
            let usable = crate::modules::account::envelope_token_error(acc).is_none()
                && crate::modules::account::get_str(acc, "access_token").is_some()
                && !acc.get("needs_relogin").and_then(Value::as_bool).unwrap_or(false);
            json!({
                "ref": if uid.is_empty() { local_id.clone() } else { uid.clone() },
                "uid": uid,
                "id": local_id,
                "name": crate::modules::account::account_display_name(acc),
                "variant": crate::modules::account::variant_of(acc).as_str(),
                "needsRelogin": acc.get("needs_relogin").and_then(Value::as_bool).unwrap_or(false),
                "usable": usable,
                "isCurrent": !uid.is_empty() && uid == current_uid,
                "cooling": is_cooling(&uid),
                "todayRequests": counts.get(&uid).copied().unwrap_or(0),
            })
        })
        .collect();
    json!({ "policy": policy(), "accounts": accounts })
}
