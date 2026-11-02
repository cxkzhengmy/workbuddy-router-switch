//! 内置 AI 网关（Rust 原生，替代外置的 workbuddy2api-hub）。
//!
//! 设计边界（刻意的减法）：只做 API Key、OpenAI 兼容反代、模型目录、
//! 请求台账（用量 + 速度）、积分联动、账号跟随。不做多账号池调度、风控连坐、
//! 模型别名魔法、看板 UI —— 那些是外置 hub 的职责，本项目用不到，
//! 而每多一项就多一处与上游协议漂移的维护面。
//!
//! 账号来源：始终跟随项目里「当前激活账号」，切号立即生效，不额外维护一份账号池。

pub mod account;
pub mod catalog;
pub mod external;
pub mod server;
pub mod store;
pub mod upstream;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::sync::watch;

/// 默认端口。沿用外置 hub 的 8788，下游客户端的 base_url 不用改。
pub const DEFAULT_PORT: u16 = 8788;

struct Running {
    port: u16,
    tx: watch::Sender<bool>,
    started_at: Instant,
}

/// 服务代次。每次 start / shutdown 都会推进，用于判断「收尾的任务是否
/// 仍然代表当前实例」。
static GENERATION: AtomicU64 = AtomicU64::new(0);

fn running() -> &'static Mutex<Option<Running>> {
    static R: OnceLock<Mutex<Option<Running>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(None))
}

pub fn is_running() -> bool {
    running()
        .lock()
        .map(|g| g.is_some())
        .unwrap_or(false)
}

pub fn current_port() -> u16 {
    running()
        .lock()
        .ok()
        .and_then(|g| g.as_ref().map(|r| r.port))
        .unwrap_or(DEFAULT_PORT)
}

/// 启动网关。已启动时幂等返回。
pub async fn start(port: u16) -> Result<Value, String> {
    if is_running() {
        return Ok(json!({
            "ok": true,
            "alreadyRunning": true,
            "port": current_port(),
        }));
    }
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .map_err(|e| format!("端口 {port} 被占用，无法启动网关：{e}"))?;
    let (tx, rx) = watch::channel(false);
    // 每次启动领一个代号：服务退出时只清理自己那一轮的状态。
    // 否则「停止 → 立即启动」时，旧任务收尾会把新实例的 Running 抹掉，
    // 界面变成「未运行」但端口其实在监听。
    let generation = GENERATION.fetch_add(1, Ordering::Relaxed) + 1;
    tokio::spawn(async move {
        if let Err(err) = server::serve_listener(listener, rx).await {
            eprintln!("[proxy] 服务退出：{err}");
        }
        if GENERATION.load(Ordering::Relaxed) == generation {
            if let Ok(mut guard) = running().lock() {
                *guard = None;
            }
        }
    });
    if let Ok(mut guard) = running().lock() {
        *guard = Some(Running {
            port,
            tx,
            started_at: Instant::now(),
        });
    }

    // 等端口真正可用再返回，避免界面显示「已启动」但第一个请求就连接被拒。
    for _ in 0..40 {
        if probe_health(port).await {
            return Ok(json!({ "ok": true, "port": port, "alreadyRunning": false }));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // 起来了但 /health 不应答：可能是账号层卡住，服务本身仍在监听。
    Ok(json!({
        "ok": true,
        "port": port,
        "alreadyRunning": false,
        "warning": "服务已监听但健康检查未通过，请检查账号状态",
    }))
}

/// 停止网关。同步接口，供应用退出钩子直接调用。
pub fn shutdown() {
    if let Ok(mut guard) = running().lock() {
        if let Some(run) = guard.take() {
            let _ = run.tx.send(true);
        }
    }
    // 代号推进一格：让正在收尾的旧任务知道「已经不是我这一轮了」，
    // 不要回来清理状态。
    GENERATION.fetch_add(1, Ordering::Relaxed);
}

async fn probe_health(port: u16) -> bool {
    let url = format!("http://127.0.0.1:{port}/health");
    match tokio::time::timeout(
        Duration::from_millis(800),
        upstream::client().get(&url).send(),
    )
    .await
    {
        Ok(Ok(resp)) => resp.status().is_success(),
        _ => false,
    }
}

/// 运行状态（界面顶部卡片）。
pub fn status() -> Value {
    let guard = running().lock().ok();
    let info = guard.as_ref().and_then(|g| g.as_ref());
    let port = info.map(|r| r.port).unwrap_or(DEFAULT_PORT);
    json!({
        "running": info.is_some(),
        "port": port,
        "baseUrl": format!("http://127.0.0.1:{port}/v1"),
        "keyCount": store::key_count(),
        "uptimeSecs": info.map(|r| r.started_at.elapsed().as_secs()).unwrap_or(0),
    })
}

// ---------------------------------------------------------------------------
// API Key 管理
// ---------------------------------------------------------------------------

pub fn create_key(name: &str, models: Option<Vec<String>>) -> Result<Value, String> {
    let (id, secret) = store::create_key(name, models)?;
    Ok(json!({ "id": id, "secret": secret }))
}

pub fn list_keys() -> Value {
    Value::Array(store::list_keys())
}

pub fn reveal_key(id: &str) -> Result<Value, String> {
    match store::key_secret(id) {
        Some(secret) => Ok(json!({ "secret": secret })),
        None => Err("Key 不存在".to_string()),
    }
}

pub fn set_key_enabled(id: &str, enabled: bool) -> Result<Value, String> {
    store::set_key_enabled(id, enabled)?;
    Ok(json!({ "ok": true }))
}

pub fn set_key_models(id: &str, models: Option<Vec<String>>) -> Result<Value, String> {
    store::update_key_models(id, models)?;
    Ok(json!({ "ok": true }))
}

pub fn delete_key(id: &str) -> Result<Value, String> {
    store::delete_key(id)?;
    Ok(json!({ "ok": true }))
}

// ---------------------------------------------------------------------------
// 账号策略
// ---------------------------------------------------------------------------

/// 账号清单 + 当前策略，供网关页展示与勾选。
pub fn account_config() -> Value {
    account::accounts_view()
}

/// 保存账号策略（跟随当前账号 / 指定账号 / 账号池）。
pub fn save_account_config(input: &Value) -> Result<Value, String> {
    account::save_policy(input)?;
    Ok(account::accounts_view())
}

// ---------------------------------------------------------------------------
// 外部上游（OpenCode 等）
// ---------------------------------------------------------------------------

pub fn external_config() -> Value {
    external::view()
}

pub fn save_external_config(input: &Value) -> Result<Value, String> {
    external::save(input)
}

/// 拉上游模型目录，供页面勾选。
pub async fn external_models() -> Value {
    match external::fetch_models().await {
        Ok(models) => json!({ "ok": true, "models": models }),
        Err(err) => json!({ "ok": false, "error": err, "models": [] }),
    }
}

// ---------------------------------------------------------------------------
// 统计
// ---------------------------------------------------------------------------

/// 首页概览：今日汇总、按模型速度、按天趋势、最近请求。
pub fn overview(days: i64) -> Value {
    let today = store::now_ms() - 86_400_000;
    json!({
        "today": store::summary(today),
        "allTime": store::summary(0),
        "models": store::by_model(today),
        "daily": store::daily(days.clamp(1, 90)),
        "recent": store::recent(50),
    })
}

pub fn clear_history() -> Result<Value, String> {
    store::clear()?;
    Ok(json!({ "ok": true }))
}

// ---------------------------------------------------------------------------
// 积分（跟随当前账号）
// ---------------------------------------------------------------------------

/// 当前账号的积分余额。`force=true` 时打上游，否则走 60 秒缓存。
pub async fn credits(force: bool) -> Value {
    static CACHE: OnceLock<Mutex<Option<(Instant, Value)>>> = OnceLock::new();
    let cell = CACHE.get_or_init(|| Mutex::new(None));
    if !force {
        if let Ok(guard) = cell.lock() {
            if let Some((at, value)) = guard.as_ref() {
                if at.elapsed() < Duration::from_secs(60) {
                    return value.clone();
                }
            }
        }
    }
    let acc = match account::resolve() {
        Ok(a) => a,
        Err(err) => return json!({ "ok": false, "error": err }),
    };
    let value = crate::modules::credits::get_credit_expiry(&acc.raw).await;
    if let Ok(mut guard) = cell.lock() {
        *guard = Some((Instant::now(), value.clone()));
    }
    value
}

// ---------------------------------------------------------------------------
// 模型
// ---------------------------------------------------------------------------

pub async fn models(force: bool) -> Value {
    let identity = upstream::Identity::parse(
        account::policy().get("identity").and_then(Value::as_str),
    );
    let (list, err) = catalog::list_for_policy(identity, force).await;
    match err {
        Some(message) if list.is_empty() => json!({ "ok": false, "error": message, "models": [] }),
        _ => json!({ "ok": true, "models": list }),
    }
}
