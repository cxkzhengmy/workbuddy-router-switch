//! 积分计费核算：从本地会话 JSONL 反推「每个模型每百万 token 实扣积分」。
//!
//! 官方只公布了各模型的**积分消耗倍率**，没有公布基准单价，也没有公布缓存
//! token 的计价规则。所以这个模块不臆造「官方单价」，只输出可复核的原始计数
//! （实扣积分 / 各类 token / 记录数 / 逐日明细），单价与倍率归一交给前端按用户
//! 选定的口径计算。这样任何结论都能回到本地账本逐条核对。
//!
//! 数据源：`providerData.rawUsage.credit`（每条模型调用的实扣积分，浮点）。
//! 与 `token_stats` 共用解码、去重、文件遍历和缓存指纹，保证两个页面的 token
//! 数字不会互相打架。
//!
//! 隐私：只读数值字段，不返回消息正文、工具参数、绝对路径或认证信息。

use chrono::Local;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use super::config::{atomic_write, now_ms, store_dir};
use super::token_stats::{
    credit_record_usage, data_fingerprint, files, model, timestamp, variant_source_roots, Usage,
};
use super::variant::WbVariant;

/// 官方公布的积分消耗倍率（相对基准单价）。键用本地日志里的模型 id。
///
/// 来源：官方「模型积分消耗倍率」说明表。0.00x 表示官方标注为不消耗积分。
/// 这张表可以在界面里编辑，编辑结果存 `~/.wb-switch/credit-rate-multipliers.json`。
pub const DEFAULT_MULTIPLIERS: &[(&str, f64)] = &[
    ("hy4", 0.00),
    ("hy3", 0.00),
    ("deepseek-v4.1-flash", 0.11),
    ("glm-5.3", 0.69),
    ("glm-5.3-flash", 0.08),
    ("glm-5.2", 0.79),
    ("space-bunny", 0.08),
    ("minimax-m3.1-flash", 0.08),
    ("kimi-k3.1-pro", 0.97),
];

const MULTIPLIER_FILE: &str = "credit-rate-multipliers.json";
const BILLING_CACHE_FILE: &str = "credit_billing_cache.json";
/// 自定义倍率表的条数上限，防止误传一个巨大对象撑爆配置文件。
const MAX_MULTIPLIER_ENTRIES: usize = 200;

#[derive(Clone, Copy, Debug, Default)]
struct Bucket {
    records: u64,
    credit: f64,
    input: u64,
    output: u64,
    read: u64,
    write: u64,
}

impl Bucket {
    fn add(&mut self, credit: f64, usage: Usage) {
        self.records = self.records.saturating_add(1);
        self.credit += credit;
        self.input = self.input.saturating_add(usage.input);
        self.output = self.output.saturating_add(usage.output);
        self.read = self.read.saturating_add(usage.read);
        self.write = self.write.saturating_add(usage.write);
    }

    fn value(&self, name: &str) -> Value {
        json!({
            "model": name,
            "records": self.records,
            // 浮点累加会有 1e-10 级噪声，落到 4 位小数再展示/存储。
            "credit": round4(self.credit),
            "input": self.input,
            "output": self.output,
            "cacheRead": self.read,
            "cacheWrite": self.write,
        })
    }
}

fn round4(value: f64) -> f64 {
    (value * 10_000.0).round() / 10_000.0
}

fn bucket_map(map: HashMap<String, Bucket>) -> Vec<Value> {
    let mut values: Vec<Value> = map
        .iter()
        .map(|(name, bucket)| bucket.value(name))
        .collect();
    values.sort_by(|left, right| {
        right["credit"]
            .as_f64()
            .unwrap_or(0.0)
            .partial_cmp(&left["credit"].as_f64().unwrap_or(0.0))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                left["model"]
                    .as_str()
                    .unwrap_or("")
                    .cmp(right["model"].as_str().unwrap_or(""))
            })
    });
    values
}

/// 一条调用实扣的积分。只认非负有限数；字符串数字按日志历史格式兼容。
fn credit(value: &Value) -> Option<f64> {
    let provider = value.get("providerData")?;
    let candidate = provider
        .get("rawUsage")
        .and_then(|raw| raw.get("credit"))
        .or_else(|| provider.get("usage").and_then(|usage| usage.get("credit")))
        .or_else(|| provider.get("credit"))?;
    let amount = candidate
        .as_f64()
        .or_else(|| candidate.as_str()?.trim().parse::<f64>().ok())?;
    (amount.is_finite() && amount >= 0.0).then_some(amount)
}

fn mtime_ms(path: &Path) -> u64 {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(u64::MAX)
}

fn local_date(ts: i64) -> Option<String> {
    chrono::DateTime::from_timestamp_millis(ts)
        .map(|date| date.with_timezone(&Local).format("%Y-%m-%d").to_string())
}

/// 扫描本地 JSONL，输出「逐日 × 模型」的原始计数。
///
/// 只解析含 `"credit"` 的行（实测约占全部行的三分之一），其余行连 JSON
/// 都不解析；按文件 mtime 由旧到新处理并用与 `token_stats` 相同的指纹去重，
/// 避免复制/派生会话重放父历史时把同一笔积分算两次。
fn scan() -> Value {
    let mut roots = Vec::new();
    for variant in [WbVariant::Cn, WbVariant::Ai] {
        roots.extend(variant_source_roots(variant));
    }
    scan_from_roots(&roots)
}

fn scan_from_roots(roots: &[PathBuf]) -> Value {
    let mut paths: Vec<PathBuf> = Vec::new();
    for root in roots {
        files(root, &mut paths);
    }
    paths.sort_by_key(|path| mtime_ms(path));

    let mut days: HashMap<String, HashMap<String, Bucket>> = HashMap::new();
    let mut totals: HashMap<String, Bucket> = HashMap::new();
    let mut seen: HashSet<(i64, u64, u64, u64, u64, String, u64)> = HashSet::new();
    let mut records = 0u64;
    let mut undated_records = 0u64;
    let mut parse_errors = 0u64;
    let mut coverage_start: Option<i64> = None;
    let mut coverage_end: Option<i64> = None;

    for path in &paths {
        let Ok(file) = std::fs::File::open(path) else {
            parse_errors = parse_errors.saturating_add(1);
            continue;
        };
        for line in BufReader::new(file).lines() {
            let Ok(line) = line else {
                parse_errors = parse_errors.saturating_add(1);
                continue;
            };
            if !line.contains("\"credit\"") {
                continue;
            }
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                parse_errors = parse_errors.saturating_add(1);
                continue;
            };
            let Some(amount) = credit(&value) else { continue };
            let Some(usage) = credit_record_usage(&value) else {
                continue;
            };
            // 没有时间戳就无法落到官方账单的日期窗口，宁可不计也不要错配。
            let Some(ts) = timestamp(&value) else {
                undated_records = undated_records.saturating_add(1);
                continue;
            };
            let Some(date) = local_date(ts) else {
                undated_records = undated_records.saturating_add(1);
                continue;
            };
            let name = model(&value);
            let fingerprint = (
                ts,
                usage.input,
                usage.output,
                usage.read,
                usage.write,
                name.clone(),
                amount.to_bits(),
            );
            if !seen.insert(fingerprint) {
                continue;
            }

            records = records.saturating_add(1);
            coverage_start = Some(coverage_start.map_or(ts, |current| current.min(ts)));
            coverage_end = Some(coverage_end.map_or(ts, |current| current.max(ts)));
            days.entry(date)
                .or_default()
                .entry(name.clone())
                .or_default()
                .add(amount, usage);
            totals.entry(name).or_default().add(amount, usage);
        }
    }

    let mut day_values: Vec<Value> = days
        .into_iter()
        .map(|(date, models)| {
            json!({
                "date": date,
                "models": bucket_map(models),
            })
        })
        .collect();
    day_values.sort_by(|left, right| {
        left["date"]
            .as_str()
            .unwrap_or("")
            .cmp(right["date"].as_str().unwrap_or(""))
    });

    let mut model_names: Vec<String> = totals.keys().cloned().collect();
    model_names.sort();

    json!({
        "generatedAt": now_ms(),
        "filesScanned": paths.len(),
        "records": records,
        "undatedRecords": undated_records,
        "parseErrors": parse_errors,
        "coverageStartAt": coverage_start,
        "coverageEndAt": coverage_end,
        "models": model_names,
        // 全量（不受前端所选窗口影响）的模型汇总，便于「全部」范围直接取用。
        "totals": bucket_map(totals),
        "days": day_values,
    })
}

static BILLING_CACHE: OnceLock<Mutex<Option<(u64, Arc<Value>)>>> = OnceLock::new();
static BILLING_DISK_TRIED: OnceLock<()> = OnceLock::new();

fn billing_cache_file() -> PathBuf {
    store_dir().join(BILLING_CACHE_FILE)
}

/// 本地积分账本。数据指纹与 `token_stats` 共用：任何会话文件变化都会让缓存失效。
pub fn billing_statistics() -> Arc<Value> {
    let fingerprint = data_fingerprint();
    let cache = BILLING_CACHE.get_or_init(|| Mutex::new(None));
    if let Ok(guard) = cache.lock() {
        if let Some((saved_fp, value)) = guard.as_ref() {
            if *saved_fp == fingerprint {
                return Arc::clone(value);
            }
        }
    }
    if BILLING_DISK_TRIED.set(()).is_ok() {
        if let Some(text) = std::fs::read_to_string(billing_cache_file()).ok() {
            if let Ok(payload) = serde_json::from_str::<Value>(&text) {
                if payload.get("fingerprint").and_then(Value::as_u64) == Some(fingerprint) {
                    if let Some(value) = payload.get("value").cloned() {
                        let value = Arc::new(value);
                        if let Ok(mut guard) = cache.lock() {
                            *guard = Some((fingerprint, Arc::clone(&value)));
                        }
                        return value;
                    }
                }
            }
        }
    }

    let value = Arc::new(scan());
    if let Ok(text) = serde_json::to_string(&json!({
        "fingerprint": fingerprint,
        "value": &*value,
    })) {
        let _ = atomic_write(&billing_cache_file(), &text);
    }
    if let Ok(mut guard) = cache.lock() {
        *guard = Some((fingerprint, Arc::clone(&value)));
    }
    value
}

fn multiplier_file() -> PathBuf {
    store_dir().join(MULTIPLIER_FILE)
}

fn default_multipliers() -> HashMap<String, f64> {
    DEFAULT_MULTIPLIERS
        .iter()
        .map(|(name, rate)| ((*name).to_string(), *rate))
        .collect()
}

/// 读取倍率表。文件缺失/损坏时回落到官方默认表，并标记 `custom: false`。
pub fn multipliers() -> Value {
    let mut table = default_multipliers();
    let mut custom = false;
    let mut updated_at = 0i64;
    if let Ok(text) = std::fs::read_to_string(multiplier_file()) {
        if let Ok(payload) = serde_json::from_str::<Value>(&text) {
            if let Some(saved) = payload.get("multipliers").and_then(Value::as_object) {
                let mut parsed: HashMap<String, f64> = HashMap::new();
                for (name, rate) in saved {
                    let key = name.trim().to_string();
                    if key.is_empty() || key.len() > 120 {
                        continue;
                    }
                    let Some(value) = rate.as_f64().filter(|value| value.is_finite() && *value >= 0.0) else {
                        continue;
                    };
                    parsed.insert(key, value);
                }
                if !parsed.is_empty() {
                    table = parsed;
                    custom = true;
                    updated_at = payload.get("updatedAt").and_then(Value::as_i64).unwrap_or(0);
                }
            }
        }
    }
    json!({
        "multipliers": table,
        "defaults": default_multipliers(),
        "custom": custom,
        "updatedAt": updated_at,
    })
}

/// 保存用户编辑的倍率表。键与值都做白名单校验，避免把任意内容写进配置目录。
pub fn save_multipliers(input: HashMap<String, f64>) -> Result<Value, String> {
    if input.len() > MAX_MULTIPLIER_ENTRIES {
        return Err(format!("倍率表最多 {MAX_MULTIPLIER_ENTRIES} 条"));
    }
    let mut table = HashMap::new();
    for (name, rate) in input {
        let key = name.trim().to_string();
        if key.is_empty() || key.len() > 120 {
            return Err("模型名不能为空或超过 120 字符".to_string());
        }
        if !rate.is_finite() || !(0.0..=1000.0).contains(&rate) {
            return Err(format!("{key} 的倍率必须是 0–1000 之间的有限数字"));
        }
        table.insert(key, round4(rate));
    }
    let payload = json!({
        "multipliers": table,
        "updatedAt": now_ms(),
    });
    let text = serde_json::to_string_pretty(&payload).map_err(|error| error.to_string())?;
    atomic_write(&multiplier_file(), &text).map_err(|error| format!("保存倍率表失败: {error}"))?;
    Ok(multipliers())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credit_accepts_number_and_string_but_rejects_garbage() {
        assert_eq!(
            credit(&json!({ "providerData": { "rawUsage": { "credit": 0.18 } } })),
            Some(0.18)
        );
        assert_eq!(
            credit(&json!({ "providerData": { "rawUsage": { "credit": "1.25" } } })),
            Some(1.25)
        );
        assert_eq!(
            credit(&json!({ "providerData": { "rawUsage": { "credit": -3 } } })),
            None
        );
        assert_eq!(credit(&json!({ "providerData": {} })), None);
    }

    #[test]
    fn credit_usage_reuses_the_token_stats_decoder_and_falls_back_to_raw() {
        let standard = json!({
            "message": { "usage": {
                "input_tokens": 139449,
                "output_tokens": 682,
                "cache_read_input_tokens": 139335
            }},
            "providerData": { "rawUsage": { "credit": 0.18 } }
        });
        assert_eq!(
            credit_record_usage(&standard),
            Some(Usage {
                input: 139449,
                output: 682,
                read: 139335,
                write: 0,
            })
        );

        let raw_only = json!({
            "providerData": { "rawUsage": {
                "prompt_tokens": 20,
                "completion_tokens": 3,
                "cache_read_input_tokens": 12,
                "credit": 0.01
            }}
        });
        assert_eq!(
            credit_record_usage(&raw_only),
            Some(Usage {
                input: 20,
                output: 3,
                read: 12,
                write: 0,
            })
        );
    }

    #[test]
    fn bucket_accumulates_and_rounds_credit() {
        let mut bucket = Bucket::default();
        bucket.add(
            0.1,
            Usage {
                input: 100,
                output: 10,
                read: 60,
                write: 0,
            },
        );
        bucket.add(
            0.2,
            Usage {
                input: 50,
                output: 5,
                read: 20,
                write: 2,
            },
        );
        let value = bucket.value("fixture-model");
        assert_eq!(value["records"], 2);
        assert_eq!(value["credit"], 0.3);
        assert_eq!(value["input"], 150);
        assert_eq!(value["output"], 15);
        assert_eq!(value["cacheRead"], 80);
        assert_eq!(value["cacheWrite"], 2);
    }

    #[test]
    fn default_multipliers_are_official_values() {
        let table = default_multipliers();
        assert_eq!(table.get("hy4"), Some(&0.0));
        assert_eq!(table.get("deepseek-v4.1-flash"), Some(&0.11));
        assert_eq!(table.get("kimi-k3.1-pro"), Some(&0.97));
    }

    /// 装机后的人工排障探针：打印本机真实账本的规模与头部模型，不参与常规测试。
    /// 运行：`cargo test -p wb-switch-core probe_real_ledger -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn probe_real_ledger() {
        let value = scan();
        println!(
            "files={} records={} undated={} parseErrors={} days={}",
            value["filesScanned"],
            value["records"],
            value["undatedRecords"],
            value["parseErrors"],
            value["days"].as_array().map_or(0, Vec::len),
        );
        for row in value["totals"].as_array().into_iter().flatten().take(12) {
            let gross = row["input"].as_u64().unwrap_or(0)
                + row["output"].as_u64().unwrap_or(0)
                + row["cacheWrite"].as_u64().unwrap_or(0);
            let per_million = row["credit"].as_f64().unwrap_or(0.0) / gross.max(1) as f64 * 1e6;
            println!(
                "{:26} credit={:>10.2} gross={:>10} perM={:>8.2}",
                row["model"].as_str().unwrap_or("?"),
                row["credit"].as_f64().unwrap_or(0.0),
                gross,
                per_million,
            );
        }
    }

    #[test]
    fn scan_deduplicates_replayed_sessions_and_groups_by_local_day() {
        let now = now_ms();
        let root = std::env::temp_dir().join(format!(
            "wb-switch-credit-billing-{}-{now}",
            std::process::id()
        ));
        let project = root.join("fixture-project");
        std::fs::create_dir_all(&project).expect("create fixture dirs");
        let record = json!({
            "timestamp": now,
            "providerData": {
                "model": "fixture-model",
                "rawUsage": { "credit": 0.25 }
            },
            "message": { "usage": {
                "input_tokens": 100,
                "output_tokens": 20,
                "cache_read_input_tokens": 60
            }}
        });
        let newer = json!({
            "timestamp": now + 1_000,
            "providerData": {
                "model": "fixture-model",
                "rawUsage": { "credit": 0.5 }
            },
            "message": { "usage": { "input_tokens": 200, "output_tokens": 30 }}
        });
        std::fs::write(
            project.join("session-original.jsonl"),
            format!("{}\n", record),
        )
        .expect("write original fixture");
        std::fs::write(
            project.join("session-forked.jsonl"),
            format!("{}\n{}\n", record, newer),
        )
        .expect("write forked fixture");
        // 与 token_stats 一样：固定 mtime，保证原始会话先于副本被处理。
        let older = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
        std::fs::OpenOptions::new()
            .write(true)
            .open(project.join("session-original.jsonl"))
            .expect("open original")
            .set_modified(older)
            .expect("pin original mtime");

        let value = scan_from_roots(&[root.clone()]);
        assert_eq!(value["filesScanned"], 2);
        // 副本重放的那一笔只算一次。
        assert_eq!(value["records"], 2);
        assert_eq!(value["undatedRecords"], 0);
        let totals = value["totals"].as_array().expect("totals");
        assert_eq!(totals.len(), 1);
        assert_eq!(totals[0]["model"], "fixture-model");
        assert_eq!(totals[0]["credit"], 0.75);
        assert_eq!(totals[0]["input"], 300);
        assert_eq!(totals[0]["output"], 50);
        assert_eq!(totals[0]["cacheRead"], 60);

        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        let days = value["days"].as_array().expect("days");
        assert_eq!(days.len(), 1);
        assert_eq!(days[0]["date"], today);
        assert_eq!(days[0]["models"][0]["credit"], 0.75);
        std::fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn save_multipliers_rejects_invalid_values() {
        let mut bad = default_multipliers();
        bad.insert("evil".to_string(), f64::NAN);
        assert!(save_multipliers(bad).is_err());
        let mut negative = default_multipliers();
        negative.insert("evil".to_string(), -1.0);
        assert!(save_multipliers(negative).is_err());
        let mut too_long = default_multipliers();
        too_long.insert("x".repeat(121), 1.0);
        assert!(save_multipliers(too_long).is_err());
    }
}
