//! 反代台账：API Key、逐次请求记录、聚合统计（SQLite）。
//!
//! 为什么必须要自己的台账：`token_stats` 扫的是客户端本地 JSONL，网关代发的
//! 请求根本不会出现在里面；`credit_usage` 是余额差值的分钟级推断。要回答
//! 「哪个 Key、哪个模型、哪次请求花了多少 token / 积分、首字多久」，
//! 只有网关自己在转发时记一笔才准确。
//!
//! 库文件：`~/.wb-switch/proxy.db`。单机个人使用，写量在每分钟几十行量级，
//! 用单连接 + `Mutex` 足够；不做连接池是为了避免 WAL 文件在退出时残留。

use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

use crate::modules::config::store_dir;

fn db_path() -> PathBuf {
    store_dir().join("proxy.db")
}

/// 打开台账文件并建表。
///
/// 任何失败都返回错误而不是 panic：网关跑在应用进程内，一个 `expect`
/// 会让整个 App 挂掉——磁盘满、目录只读、库文件被占用都会走到这里，
/// 而这些都不该是「关闭应用」的理由。
fn open() -> Result<Connection, String> {
    let path = db_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let c = Connection::open(&path)
        .map_err(|e| format!("无法打开台账 {}：{e}", path.display()))?;
    // WAL 让「统计查询」不被「写入」挡住；busy_timeout 兜住偶发并发写。
    let _ = c.pragma_update(None, "journal_mode", "WAL");
    let _ = c.pragma_update(None, "busy_timeout", 3000);
    init_schema(&c).map_err(|e| format!("初始化台账结构失败：{e}"))?;
    Ok(c)
}

fn conn() -> Result<MutexGuard<'static, Connection>, String> {
    // 缓存的是「连接或失败原因」而不是裸连接：失败也要留住，否则每次
    // 请求都会重试打开一个注定打不开的文件。
    static DB: OnceLock<Result<Mutex<Connection>, String>> = OnceLock::new();
    let cell = DB.get_or_init(|| open().map(Mutex::new));
    match cell {
        Ok(mutex) => mutex.lock().map_err(|_| "台账连接已损坏".to_string()),
        Err(err) => Err(err.clone()),
    }
}

fn init_schema(c: &Connection) -> rusqlite::Result<()> {
    c.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS keys (
            id           TEXT PRIMARY KEY,
            secret       TEXT NOT NULL UNIQUE,
            name         TEXT NOT NULL,
            enabled      INTEGER NOT NULL DEFAULT 1,
            models       TEXT,
            created_at   INTEGER NOT NULL,
            last_used_at INTEGER
        );
        CREATE TABLE IF NOT EXISTS requests (
            id                INTEGER PRIMARY KEY AUTOINCREMENT,
            ts                INTEGER NOT NULL,
            key_id            TEXT,
            account_uid       TEXT,
            account_label     TEXT,
            model             TEXT,
            stream            INTEGER NOT NULL DEFAULT 0,
            status            INTEGER NOT NULL,
            prompt_tokens     INTEGER NOT NULL DEFAULT 0,
            completion_tokens INTEGER NOT NULL DEFAULT 0,
            cached_tokens     INTEGER NOT NULL DEFAULT 0,
            total_tokens      INTEGER NOT NULL DEFAULT 0,
            credit            REAL NOT NULL DEFAULT 0,
            ttft_ms           INTEGER,
            total_ms          INTEGER,
            error             TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_requests_ts ON requests(ts);
        CREATE INDEX IF NOT EXISTS idx_requests_model ON requests(model, ts);
        CREATE TABLE IF NOT EXISTS settings (
            k TEXT PRIMARY KEY,
            v TEXT NOT NULL
        );
        "#,
    )
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// API Key
// ---------------------------------------------------------------------------

/// 新建一个 Key，返回 (id, secret)。
pub fn create_key(name: &str, models: Option<Vec<String>>) -> Result<(String, String), String> {
    let id = uuid::Uuid::new_v4().simple().to_string()[..12].to_string();
    let secret = format!("sk-wb-{}", uuid::Uuid::new_v4().simple());
    let models_json = models.map(|m| Value::Array(m.into_iter().map(Value::String).collect()));
    let c = conn()?;
    c.execute(
        "INSERT INTO keys (id, secret, name, enabled, models, created_at) VALUES (?1, ?2, ?3, 1, ?4, ?5)",
        params![
            id,
            secret,
            if name.trim().is_empty() { "默认 Key" } else { name.trim() },
            models_json.map(|v| v.to_string()),
            now_ms(),
        ],
    )
    .map_err(|e| format!("创建 Key 失败：{e}"))?;
    Ok((id, secret))
}

pub fn list_keys() -> Vec<Value> {
    let Ok(c) = conn() else {
        return Vec::new();
    };
    let mut stmt = match c.prepare(
        "SELECT id, name, enabled, models, created_at, last_used_at, substr(secret, 1, 12) FROM keys ORDER BY created_at DESC",
    ) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let rows = stmt.query_map([], |row| {
        Ok(json!({
            "id": row.get::<_, String>(0)?,
            "name": row.get::<_, String>(1)?,
            "enabled": row.get::<_, i64>(2)? == 1,
            "models": row.get::<_, Option<String>>(3)?
                .and_then(|s| serde_json::from_str::<Value>(&s).ok()),
            "createdAt": row.get::<_, i64>(4)?,
            "lastUsedAt": row.get::<_, Option<i64>>(5)?,
            "preview": row.get::<_, String>(6)?,
        }))
    });
    match rows {
        Ok(iter) => iter.filter_map(Result::ok).collect(),
        Err(_) => Vec::new(),
    }
}

/// 校验 Key。返回 (key_id, 允许模型列表)。
///
/// `None` 表示鉴权失败。允许模型为 `None` 表示不限。
pub fn verify_key(secret: &str) -> Option<(String, Option<Vec<String>>)> {
    let c = conn().ok()?;
    let row: Option<(String, i64, Option<String>)> = c
        .query_row(
            "SELECT id, enabled, models FROM keys WHERE secret = ?1",
            params![secret],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()
        .ok()?;
    let (id, enabled, models) = row?;
    if enabled != 1 {
        return None;
    }
    let _ = c.execute(
        "UPDATE keys SET last_used_at = ?1 WHERE id = ?2",
        params![now_ms(), id],
    );
    let models = models.and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok());
    Some((id, models))
}

pub fn set_key_enabled(id: &str, enabled: bool) -> Result<(), String> {
    let c = conn()?;
    c.execute(
        "UPDATE keys SET enabled = ?1 WHERE id = ?2",
        params![if enabled { 1 } else { 0 }, id],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

pub fn update_key_models(id: &str, models: Option<Vec<String>>) -> Result<(), String> {
    let c = conn()?;
    let value = models.map(|m| {
        Value::Array(m.into_iter().map(Value::String).collect()).to_string()
    });
    c.execute(
        "UPDATE keys SET models = ?1 WHERE id = ?2",
        params![value, id],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

pub fn delete_key(id: &str) -> Result<(), String> {
    let c = conn()?;
    c.execute("DELETE FROM keys WHERE id = ?1", params![id])
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// 读取 Key 的完整密钥（用于「复制」按钮，列表接口默认只给前缀）。
pub fn key_secret(id: &str) -> Option<String> {
    let c = conn().ok()?;
    c.query_row("SELECT secret FROM keys WHERE id = ?1", params![id], |r| {
        r.get::<_, String>(0)
    })
    .optional()
    .ok()?
}

pub fn key_count() -> usize {
    conn()
        .and_then(|c| {
            c.query_row("SELECT COUNT(*) FROM keys", [], |r| r.get::<_, i64>(0))
                .map_err(|e| e.to_string())
        })
        .map(|n| n as usize)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// 设置（账号策略等）
// ---------------------------------------------------------------------------

/// 读一段设置（JSON）。缺失或损坏时返回空对象。
pub fn load_settings(key: &str) -> Value {
    let Ok(c) = conn() else {
        return json!({});
    };
    let raw: Option<String> = c
        .query_row("SELECT v FROM settings WHERE k = ?1", params![key], |r| r.get(0))
        .optional()
        .ok()
        .flatten();
    match raw {
        Some(text) => serde_json::from_str(&text).unwrap_or_else(|_| json!({})),
        None => json!({}),
    }
}

pub fn save_settings(key: &str, value: &Value) -> Result<(), String> {
    let c = conn()?;
    c.execute(
        "INSERT INTO settings (k, v) VALUES (?1, ?2)
         ON CONFLICT(k) DO UPDATE SET v = excluded.v",
        params![key, value.to_string()],
    )
    .map_err(|e| format!("保存设置失败：{e}"))?;
    Ok(())
}

/// 今日各账号的请求数，供「最少请求」调度排序。
pub fn today_requests_by_account() -> std::collections::HashMap<String, i64> {
    let mut out = std::collections::HashMap::new();
    let Ok(c) = conn() else {
        return out;
    };
    let since = now_ms() - 86_400_000;
    let Ok(mut stmt) = c.prepare(
        "SELECT account_uid, COUNT(*) FROM requests WHERE ts >= ?1 AND account_uid IS NOT NULL GROUP BY account_uid",
    ) else {
        return out;
    };
    if let Ok(rows) = stmt.query_map(params![since], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
    }) {
        for row in rows.flatten() {
            out.insert(row.0, row.1);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// 请求记录
// ---------------------------------------------------------------------------

/// 一次请求的记账数据。
#[derive(Debug, Clone, Default)]
pub struct RequestRecord {
    pub key_id: Option<String>,
    pub account_uid: Option<String>,
    pub account_label: Option<String>,
    pub model: String,
    pub stream: bool,
    pub status: u16,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub cached_tokens: i64,
    pub total_tokens: i64,
    pub credit: f64,
    pub ttft_ms: Option<i64>,
    pub total_ms: Option<i64>,
    pub error: Option<String>,
}

/// 台账保留天数。一次对话一行，天天用也就每年几万行；保留 90 天足够
/// 支撑「近 7 日 / 本月」的视图，又不至于让库无限长。
const RETENTION_DAYS: i64 = 90;

/// 每 200 次写入顺手清一次过期记录。
///
/// 必须复用调用方已持有的连接：`conn()` 背后是 `std::sync::Mutex`，不可
/// 重入——在 `record()` 里再调一次 `conn()` 会把自己锁死，表现为整个网关
/// 服务静默挂起（每个请求都在等一把自己持有的锁）。
fn maybe_prune(c: &Connection) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    if COUNTER.fetch_add(1, Ordering::Relaxed) % 200 != 0 {
        return;
    }
    let cutoff = now_ms() - RETENTION_DAYS * 86_400_000;
    let _ = c.execute("DELETE FROM requests WHERE ts < ?1", params![cutoff]);
}

pub fn record(req: &RequestRecord) {
    let Ok(c) = conn() else {
        return;
    };
    let _ = c.execute(
        r#"INSERT INTO requests
           (ts, key_id, account_uid, account_label, model, stream, status,
            prompt_tokens, completion_tokens, cached_tokens, total_tokens,
            credit, ttft_ms, total_ms, error)
           VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)"#,
        params![
            now_ms(),
            req.key_id,
            req.account_uid,
            req.account_label,
            req.model,
            if req.stream { 1 } else { 0 },
            req.status as i64,
            req.prompt_tokens,
            req.completion_tokens,
            req.cached_tokens,
            req.total_tokens,
            req.credit,
            req.ttft_ms,
            req.total_ms,
            req.error,
        ],
    );
    maybe_prune(&c);
}

/// 汇总：请求数、token、积分、平均首字与吞吐。
pub fn summary(since_ms: i64) -> Value {
    summary_between(since_ms, i64::MAX)
}

/// 指定区间的汇总。`[since, until)` 左闭右开——按天看时必须这样切，
/// 否则 24:00 那条记录会同时算进两天。
pub fn summary_between(since_ms: i64, until_ms: i64) -> Value {
    let Ok(c) = conn() else {
        return json!({});
    };
    let row = c.query_row(
        r#"SELECT COUNT(*),
                  COALESCE(SUM(prompt_tokens),0),
                  COALESCE(SUM(completion_tokens),0),
                  COALESCE(SUM(cached_tokens),0),
                  COALESCE(SUM(total_tokens),0),
                  COALESCE(SUM(credit),0),
                  COALESCE(AVG(ttft_ms),0),
                  COALESCE(AVG(total_ms),0),
                  COALESCE(SUM(CASE WHEN status >= 400 THEN 1 ELSE 0 END),0)
           FROM requests WHERE ts >= ?1 AND ts < ?2"#,
        params![since_ms, until_ms],
        |r| {
            Ok(json!({
                "requests": r.get::<_, i64>(0)?,
                "promptTokens": r.get::<_, i64>(1)?,
                "completionTokens": r.get::<_, i64>(2)?,
                "cachedTokens": r.get::<_, i64>(3)?,
                "totalTokens": r.get::<_, i64>(4)?,
                "credit": r.get::<_, f64>(5)?,
                "avgTtftMs": r.get::<_, f64>(6).map(|v| v.round())?,
                "avgTotalMs": r.get::<_, f64>(7).map(|v| v.round())?,
                "errors": r.get::<_, i64>(8)?,
            }))
        },
    );
    row.unwrap_or_else(|_| json!({}))
}

/// 按模型聚合。带 p50 / p95 首字延迟——分位在 Rust 里排序算，
/// SQL 没有通用分位函数，硬塞进去反而难读。
pub fn by_model(since_ms: i64) -> Vec<Value> {
    by_model_between(since_ms, i64::MAX)
}

/// 指定区间的按模型聚合。带 p50 / p95 首字延迟——分位在 Rust 里排序算，
/// SQL 没有通用分位函数，硬塞进去反而难读。
pub fn by_model_between(since_ms: i64, until_ms: i64) -> Vec<Value> {
    let Ok(c) = conn() else {
        return Vec::new();
    };
    let mut stmt = match c.prepare(
        r#"SELECT model,
                  COUNT(*),
                  COALESCE(SUM(prompt_tokens),0),
                  COALESCE(SUM(completion_tokens),0),
                  COALESCE(SUM(credit),0),
                  COALESCE(SUM(total_tokens),0)
           FROM requests WHERE ts >= ?1 AND ts < ?2
           GROUP BY model ORDER BY 3 + 4 DESC"#,
    ) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let base: Vec<(String, i64, i64, i64, f64, i64)> = stmt
        .query_map(params![since_ms, until_ms], |r| {
            Ok((
                r.get::<_, Option<String>>(0)?.unwrap_or_default(),
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, f64>(4)?,
                r.get::<_, i64>(5)?,
            ))
        })
        .map(|iter| iter.filter_map(Result::ok).collect())
        .unwrap_or_default();

    base.into_iter()
        .map(|(model, requests, prompt, completion, credit, total)| {
            let ttfts = match c.prepare(
                "SELECT ttft_ms FROM requests WHERE ts >= ?1 AND ts < ?2 AND model = ?3 AND ttft_ms IS NOT NULL",
            ) {
                Ok(mut s) => s
                    .query_map(params![since_ms, until_ms, &model], |r| r.get::<_, i64>(0))
                    .map(|it| it.filter_map(Result::ok).collect::<Vec<_>>())
                    .unwrap_or_default(),
                Err(_) => Vec::new(),
            };
            let gen_ms = match c.query_row(
                r#"SELECT COALESCE(SUM(CASE WHEN ttft_ms IS NOT NULL AND total_ms IS NOT NULL
                          THEN MAX(total_ms - ttft_ms, 1) ELSE 0 END),0)
                   FROM requests WHERE ts >= ?1 AND ts < ?2 AND model = ?3"#,
                params![since_ms, until_ms, &model],
                |r| r.get::<_, i64>(0),
            ) {
                Ok(v) => v,
                Err(_) => 0,
            };
            json!({
                "model": model,
                "requests": requests,
                "promptTokens": prompt,
                "completionTokens": completion,
                "totalTokens": total,
                "credit": credit,
                "ttftP50": percentile(&ttfts, 0.50),
                "ttftP95": percentile(&ttfts, 0.95),
                "tokensPerSec": if gen_ms > 0 {
                    (completion as f64 * 1000.0 / gen_ms as f64 * 10.0).round() / 10.0
                } else { 0.0 },
            })
        })
        .collect()
}

fn percentile(values: &[i64], p: f64) -> Option<i64> {
    if values.is_empty() {
        return None;
    }
    let mut v = values.to_vec();
    v.sort_unstable();
    let idx = ((v.len() - 1) as f64 * p).round() as usize;
    v.get(idx).copied()
}

/// 按天序列（用于趋势图）。
pub fn daily(days: i64) -> Vec<Value> {
    let since = now_ms() - days.max(1) * 86_400_000;
    let Ok(c) = conn() else {
        return Vec::new();
    };
    let mut stmt = match c.prepare(
        r#"SELECT date(ts/1000, 'unixepoch', 'localtime') AS d,
                  COUNT(*), COALESCE(SUM(total_tokens),0), COALESCE(SUM(credit),0)
           FROM requests WHERE ts >= ?1 GROUP BY d ORDER BY d"#,
    ) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    stmt.query_map(params![since], |r| {
        Ok(json!({
            "date": r.get::<_, String>(0)?,
            "requests": r.get::<_, i64>(1)?,
            "tokens": r.get::<_, i64>(2)?,
            "credit": r.get::<_, f64>(3)?,
        }))
    })
    .map(|it| it.filter_map(Result::ok).collect())
    .unwrap_or_default()
}

/// 最近若干条请求（排障用）。
pub fn recent(limit: usize) -> Vec<Value> {
    recent_between(i64::MIN, i64::MAX, limit)
}

/// 指定区间内最近若干条请求。看某一天的明细时用它——不带区间的话，
/// 选的是「10-30」，列表里却是今天的最新几条，对不上号。
pub fn recent_between(since_ms: i64, until_ms: i64, limit: usize) -> Vec<Value> {
    let Ok(c) = conn() else {
        return Vec::new();
    };
    let mut stmt = match c.prepare(
        r#"SELECT ts, model, status, prompt_tokens, completion_tokens, credit,
                  ttft_ms, total_ms, account_label, error
           FROM requests WHERE ts >= ?1 AND ts < ?2
           ORDER BY id DESC LIMIT ?3"#,
    ) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    stmt.query_map(params![since_ms, until_ms, limit as i64], |r| {
        Ok(json!({
            "ts": r.get::<_, i64>(0)?,
            "model": r.get::<_, Option<String>>(1)?,
            "status": r.get::<_, i64>(2)?,
            "promptTokens": r.get::<_, i64>(3)?,
            "completionTokens": r.get::<_, i64>(4)?,
            "credit": r.get::<_, f64>(5)?,
            "ttftMs": r.get::<_, Option<i64>>(6)?,
            "totalMs": r.get::<_, Option<i64>>(7)?,
            "account": r.get::<_, Option<String>>(8)?,
            "error": r.get::<_, Option<String>>(9)?,
        }))
    })
    .map(|it| it.filter_map(Result::ok).collect())
    .unwrap_or_default()
}

/// 清空台账（换机 / 重置统计）。
pub fn clear() -> Result<(), String> {
    let c = conn()?;
    c.execute("DELETE FROM requests", [])
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentile_basics() {
        assert_eq!(percentile(&[], 0.5), None);
        assert_eq!(percentile(&[10], 0.95), Some(10));
        assert_eq!(percentile(&[1, 2, 3, 4, 5], 0.5), Some(3));
        assert_eq!(percentile(&[1, 2, 3, 4, 5], 0.95), Some(5));
    }

}
