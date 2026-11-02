//! 模型目录：本机桌面端缓存 → 上游 `/v3/config` → 兜底。
//!
//! 为什么要三级：桌面客户端每次启动都会把产品配置写到
//! `~/.workbuddy-ai/cache/acc-product-config-v3.json`，那份是**最全**的
//! （21 个模型，含倍率与展示名）；`/v3/config` 需要有效账号才能取；
//! `/v2/enterprises/personal/models` 是 CLI 视角，条数更少。三者按此顺序退让，
//! 保证「没装桌面端也能列出模型」「断网也能用上次结果」。

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::account::ActiveAccount;
use super::upstream::{self, Identity};
use crate::modules::config::home_dir;
use crate::modules::variant::WbVariant;

const CACHE_TTL: Duration = Duration::from_secs(600);

struct Cache {
    at: Instant,
    models: Vec<Value>,
}

/// 按档位分开缓存。账号池可能同时有国内版与国际版，用单槽缓存会让两个
/// 档位互相覆盖——表现是「模型列表随机少一半」。
/// 缓存键是「档位 + 出站身份」：不同身份拿到的是不同的模型清单
/// （VSCode 插件与桌面端的可见模型集合并不相同）。
fn cache() -> &'static Mutex<std::collections::HashMap<String, Cache>> {
    static C: OnceLock<Mutex<std::collections::HashMap<String, Cache>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

/// 取模型目录（带 10 分钟内存缓存）。`force` 跳过缓存。
pub async fn list(acc: &ActiveAccount, identity: Identity, force: bool) -> Vec<Value> {
    let realm = format!("{}:{}", acc.variant.as_str(), identity.as_str());
    if !force {
        if let Ok(guard) = cache().lock() {
            if let Some(c) = guard.get(&realm) {
                if c.at.elapsed() < CACHE_TTL {
                    return c.models.clone();
                }
            }
        }
    }

    // 联网优先，本地快照只作离线兜底。
    //
    // 本地文件 `~/.workbuddy/cache/acc-product-config-v3.json` 是「用户上次
    // 打开桌面客户端时」写下的，不打客户端就永远停在那一刻（实测能差出 40
    // 多个模型）；而且档位回退会去读另一档位的目录——国内账号列表里冒出
    // gpt-* 就是这么来的。所以它不能当权威源。
    let ids = upstream::fetch_models(acc, identity).await;
    let mut models: Vec<Value> = ids.into_iter().map(|id| json!({ "id": id })).collect();
    if models.is_empty() {
        models = read_local_config(acc.variant);
    }

    if let Ok(mut guard) = cache().lock() {
        guard.insert(
            realm,
            Cache {
                at: Instant::now(),
                models: models.clone(),
            },
        );
    }
    models
}

/// 当前策略下所有候选账号的模型并集。
///
/// `/v1/models` 与页面上的模型清单都走这里：账号池混了国内版与国际版时，
/// 只按「当前账号」列会漏掉实际会用到的另一半。缺少的模型客户端根本选不到，
/// 比多列几个更难排查。
pub async fn list_for_policy(identity: Identity, force: bool) -> (Vec<Value>, Option<String>) {
    let candidates = match super::account::resolve_candidates() {
        Ok(list) => list,
        Err(err) => return (Vec::new(), Some(err)),
    };
    let mut seen_realm: Vec<&str> = Vec::new();
    let mut out: Vec<Value> = Vec::new();
    for acc in candidates.iter().take(3) {
        if seen_realm.contains(&acc.variant.as_str()) {
            continue;
        }
        seen_realm.push(acc.variant.as_str());
        for model in list(acc, identity, force).await {
            let id = model.get("id").cloned().unwrap_or(Value::Null);
            if !out.iter().any(|existing| existing.get("id") == Some(&id)) {
                out.push(model);
            }
        }
    }
    (out, None)
}

fn config_path(variant: WbVariant) -> std::path::PathBuf {
    let dir = match variant {
        WbVariant::Ai => ".workbuddy-ai",
        WbVariant::Cn => ".workbuddy",
    };
    home_dir()
        .join(dir)
        .join("cache")
        .join("acc-product-config-v3.json")
}

/// 读桌面端缓存的模型清单。文件不存在/损坏都返回空。
///
/// 档位目录只对「用哪个客户端登录过」有意义：只装了国际版客户端时，
/// 国内版路径 (`~/.workbuddy/cache/`) 根本不存在。模型目录本身两档位高度重合，
/// 所以当前档位没有就退到另一档位——总比空列表强，而且省一次联网。
fn read_local_config(variant: WbVariant) -> Vec<Value> {
    for candidate in [variant, other_variant(variant)] {
        let Ok(text) = std::fs::read_to_string(config_path(candidate)) else {
            continue;
        };
        let Ok(payload) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        let mut out: Vec<Value> = Vec::new();
        collect(&payload, &mut out);
        if !out.is_empty() {
            return out;
        }
    }
    Vec::new()
}

fn other_variant(variant: WbVariant) -> WbVariant {
    match variant {
        WbVariant::Ai => WbVariant::Cn,
        WbVariant::Cn => WbVariant::Ai,
    }
}

/// 递归找模型清单。
///
/// 缓存文件的结构在不同版本间漂移过：有时是 `agents.<name>.models` 的裸字符串
/// 数组，有时是 `models: [{id, name, ...}]` 的对象数组。两种都收，按 id 去重。
fn collect(node: &Value, out: &mut Vec<Value>) {
    match node {
        Value::Object(map) => {
            if let Some(Value::Array(list)) = map.get("models") {
                let mut handled = false;
                for item in list {
                    match item {
                        Value::String(id) => {
                            push_model(out, id, None);
                            handled = true;
                        }
                        Value::Object(obj) => {
                            if let Some(id) = obj.get("id").and_then(Value::as_str) {
                                let name = obj
                                    .get("name")
                                    .or_else(|| obj.get("displayName"))
                                    .or_else(|| obj.get("title"))
                                    .and_then(Value::as_str);
                                push_model(out, id, name);
                                handled = true;
                            }
                        }
                        _ => {}
                    }
                }
                if handled {
                    return;
                }
            }
            for value in map.values() {
                collect(value, out);
            }
        }
        Value::Array(list) => {
            for value in list {
                collect(value, out);
            }
        }
        _ => {}
    }
}

fn push_model(out: &mut Vec<Value>, id: &str, name: Option<&str>) {
    let id = id.trim();
    if id.is_empty() || out.iter().any(|m| m["id"] == id) {
        return;
    }
    let mut entry = json!({ "id": id });
    if let Some(name) = name.map(str::trim).filter(|n| !n.is_empty() && *n != id) {
        entry["name"] = json!(name);
    }
    out.push(entry);
}

/// 供 `/v1/models` 用：按 Key 的允许清单过滤。允许清单为空 = 不限制。
pub fn filter(models: &[Value], allowed: Option<&Vec<String>>) -> Vec<Value> {
    match allowed {
        None => models.to_vec(),
        Some(list) if list.is_empty() => models.to_vec(),
        Some(list) => models
            .iter()
            .filter(|m| {
                m.get("id")
                    .and_then(Value::as_str)
                    .map(|id| list.iter().any(|a| a == id))
                    .unwrap_or(false)
            })
            .cloned()
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collects_both_shapes() {
        let payload = json!({
            "data": {
                "agents": {
                    "cli": { "models": ["deepseek-v4.1-flash", "gpt-6-astra"] },
                    "other": { "models": [{ "id": "hy3", "name": "混元 3" }] }
                }
            }
        });
        let mut out = Vec::new();
        collect(&payload, &mut out);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0]["id"], "deepseek-v4.1-flash");
        assert_eq!(out[2]["name"], "混元 3");
    }

    #[test]
    fn filters_by_allowed_list() {
        let models = vec![json!({"id": "a"}), json!({"id": "b"})];
        let allowed = vec!["b".to_string()];
        assert_eq!(filter(&models, Some(&allowed)).len(), 1);
        assert_eq!(filter(&models, None).len(), 2);
    }
}
