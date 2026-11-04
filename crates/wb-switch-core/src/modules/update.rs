//! 自动更新：检查公开 GitHub Releases 版本 + 更新源配置。
//!
//! 对照 server.py `load_github_config` / `save_github_config` /
//! `compare_versions` / `update_check`。下载安装走 tauri-plugin-updater（整包更新）。
//!
//! 版本检查不走 GitHub API（避免 60 次/小时/IP 限流）：
//! 1. 主端点：release 资产的 updater manifest（下载不计 API 配额）；
//! 2. 兜底端点：`/releases/latest` 的 302 `Location` 头解析 tag；
//! 3. 成功结果进程级缓存 6 小时，缓存命中不发网络请求。

use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use crate::modules::config::{
    atomic_write, http_request_raw, http_request_with_proxy, now_secs, store_dir,
};

/// 应用当前版本（来自 Cargo.toml package.version）。
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// 更新检查的目标仓库。
///
/// 本项目（workbuddy-router）有自己的发布页，更新的清单（`latest.json`）与
/// 安装包都发在这里。**不要指回上游 workbuddy-switch**：那是没有内嵌网关的
/// 原版，装上去等于把网关功能弄丢，而且签名密钥也对不上。
///
/// 用户仍可在设置里把 `github_config.json` 的 owner/repo 覆写成自己的仓库
/// （fork 后自建发布源），这里只是默认值。
pub const GITHUB_OWNER: &str = "cxkzhengmy";
pub const GITHUB_REPO: &str = "workbuddy-router-switch";

/// 成功结果缓存有效期（6 小时）。自动轮询（30 分钟）命中缓存，不发网络请求；
/// 设置页手动检查传 force=true 绕过缓存强制刷新。
const CACHE_TTL_SECS: i64 = 6 * 60 * 60;

/// 进程级内存缓存，只缓存 ok=true 的结果；失败不写缓存。
struct CachedCheck {
    checked_at: i64,
    value: Value,
}

static CACHE: Mutex<Option<CachedCheck>> = Mutex::new(None);

pub fn github_config_file() -> PathBuf {
    store_dir().join("github_config.json")
}

/// 读取更新源配置（兼容旧配置文件，但永不返回 token）。
pub fn load_github_config() -> Value {
    let mut owner = GITHUB_OWNER.to_string();
    let mut repo = GITHUB_REPO.to_string();
    let mut proxy = String::new();
    let f = github_config_file();
    let mut should_normalize = false;
    if f.exists() {
        if let Ok(text) = std::fs::read_to_string(&f) {
            if let Ok(v) = serde_json::from_str::<Value>(&text) {
                if let Some(value) = v.get("owner").and_then(|v| v.as_str()) {
                    if !value.trim().is_empty() {
                        owner = value.to_string();
                    }
                }
                if let Some(value) = v.get("repo").and_then(|v| v.as_str()) {
                    if !value.trim().is_empty() {
                        repo = value.to_string();
                    }
                }
                if let Some(value) = v.get("proxy").and_then(|v| v.as_str()) {
                    proxy = value.trim().to_string();
                }
                should_normalize = v.get("token").is_some();
            }
        }
    }
    // 老配置里可能写着上游仓库（`changexbc/*`，含早期的 wb-switch 与后来的
    // workbuddy-switch）。上游是**没有内嵌网关**的另一款产品，包也不是本项目
    // 签的：留在配置里会让检查结果指向别人的 release，而下载端点用的是
    // tauri.conf.json 里的本项目地址，两者一错位，用户只会看到「安装失败」。
    // 遇到就静默迁移到本项目仓库，并把配置写回去。
    if owner == "changexbc" {
        owner = GITHUB_OWNER.to_string();
        repo = GITHUB_REPO.to_string();
        should_normalize = true;
    }
    let normalized = json!({"owner": owner, "repo": repo, "proxy": proxy});
    if should_normalize {
        let _ = atomic_write(
            &f,
            &serde_json::to_string_pretty(&normalized).unwrap_or_default(),
        );
    }
    normalized
}

/// 保存更新源配置；公开仓库不需要也不保存 GitHub token。
pub fn save_github_config(cfg: &Value) -> std::io::Result<()> {
    let owner = cfg
        .get("owner")
        .and_then(|v| v.as_str())
        .filter(|v| !v.trim().is_empty())
        .unwrap_or(GITHUB_OWNER);
    let repo = cfg
        .get("repo")
        .and_then(|v| v.as_str())
        .filter(|v| !v.trim().is_empty())
        .unwrap_or(GITHUB_REPO);
    let proxy = cfg
        .get("proxy")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .unwrap_or("");
    let clean = json!({"owner": owner, "repo": repo, "proxy": proxy});
    std::fs::create_dir_all(store_dir())?;
    atomic_write(
        &github_config_file(),
        &serde_json::to_string_pretty(&clean).unwrap_or_default(),
    )
}

fn version_tuple(v: &str) -> Vec<i64> {
    v.trim_start_matches('v')
        .split('.')
        .filter_map(|x| x.parse::<i64>().ok())
        .collect()
}

/// 版本比较：a > b 返回 1，a < b 返回 -1，相等返回 0。
pub fn compare_versions(a: &str, b: &str) -> i64 {
    let ta = version_tuple(a);
    let tb = version_tuple(b);
    for i in 0..ta.len().max(tb.len()) {
        let x = ta.get(i).copied().unwrap_or(0);
        let y = tb.get(i).copied().unwrap_or(0);
        if x != y {
            return if x > y { 1 } else { -1 };
        }
    }
    0
}

/// updater manifest 候选 URL（按优先级）。
///
/// 1. 合并后的 `latest.json`（含各平台）；
/// 2. 当前系统的 `latest-<os>-<arch>.json`；
/// 3. 兼容旧 Windows 安装包：它们仍请求 `latest-macos-<arch>.json`。
pub fn updater_manifest_urls(owner: &str, repo: &str, os: &str, arch: &str) -> Vec<String> {
    let os_slug = match os {
        "macos" | "darwin" => "macos",
        other => other,
    };
    let mut urls = vec![format!(
        "https://github.com/{owner}/{repo}/releases/latest/download/latest.json"
    )];
    urls.push(format!(
        "https://github.com/{owner}/{repo}/releases/latest/download/latest-{os_slug}-{arch}.json"
    ));
    if os_slug != "macos" {
        urls.push(format!(
            "https://github.com/{owner}/{repo}/releases/latest/download/latest-macos-{arch}.json"
        ));
    }
    urls.dedup();
    urls
}

/// 主端点：拉取 updater manifest。成功返回解析后的 JSON（含 version / pub_date），
/// 失败返回可读错误信息。
async fn fetch_manifest_version(
    owner: &str,
    repo: &str,
    proxy: Option<&str>,
) -> Result<Value, String> {
    let mut headers = HashMap::new();
    headers.insert("Accept".to_string(), "application/json".to_string());
    headers.insert("User-Agent".to_string(), "wb-switch".to_string());
    let mut last_err = "更新清单解析失败".to_string();
    for url in updater_manifest_urls(owner, repo, std::env::consts::OS, std::env::consts::ARCH) {
        let resp = http_request_with_proxy(&url, "GET", None, Some(&headers), proxy).await;
        let version = resp.get("version").and_then(|v| v.as_str()).unwrap_or("");
        if !version.trim().is_empty() {
            return Ok(resp);
        }
        last_err = resp
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("更新清单解析失败")
            .to_string();
    }
    Err(last_err)
}

/// 兜底端点：请求 `/releases/latest`，读 302 `Location` 头（形如
/// `.../releases/tag/v0.1.13`）解析 tag。不跟随重定向，避免拉到 HTML 页面。
/// 成功返回 tag，失败返回可读错误 + code（状态码或 -1）。
async fn fetch_latest_tag(
    owner: &str,
    repo: &str,
    proxy: Option<&str>,
) -> Result<String, (String, i64)> {
    let url = format!("https://github.com/{owner}/{repo}/releases/latest");
    let mut headers = HashMap::new();
    headers.insert("Accept".to_string(), "text/html".to_string());
    headers.insert("User-Agent".to_string(), "wb-switch".to_string());
    let (status, resp_headers, body) =
        http_request_raw(&url, "GET", None, Some(&headers), proxy, false).await;

    if status == 0 {
        let msg = if body.trim().is_empty() {
            "网络请求失败".to_string()
        } else {
            body
        };
        return Err((msg, -1));
    }
    if status == 404 {
        // 无正式 release 或仓库不存在。
        return Err(("未找到可用的发布版本".to_string(), 404));
    }
    let location = resp_headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("location"))
        .map(|(_, v)| v.clone());
    let location = match location {
        Some(location) => location,
        None => {
            return Err((
                format!("无法获取发布页跳转地址（HTTP {status}）"),
                status as i64,
            ))
        }
    };
    let tag = location.rsplit('/').next().unwrap_or("").trim().to_string();
    if tag.is_empty() || !location.contains("/releases/tag/") {
        return Err(("无法解析发布版本标签".to_string(), -1));
    }
    Ok(tag)
}

/// 查询最新 Release，与本地版本对比。对照 server.py `update_check`。
///
/// `force=true` 绕过缓存强制刷新（设置页手动检查）；否则 6 小时内成功结果直接返回，
/// 不发网络请求。主端点 manifest 失败时自动走 302 兜底；两个端点都失败返回可读错误。
pub async fn update_check(proxy: Option<&str>, force: bool) -> Value {
    if !force {
        if let Some(cached) = CACHE.lock().unwrap().as_ref() {
            if now_secs() - cached.checked_at < CACHE_TTL_SECS {
                return cached.value.clone();
            }
        }
    }

    let cfg = load_github_config();
    let owner = cfg
        .get("owner")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let repo = cfg
        .get("repo")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let configured_proxy = cfg
        .get("proxy")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|v| !v.is_empty());
    let proxy = proxy.or(configured_proxy);
    let release_url = format!("https://github.com/{owner}/{repo}/releases/latest");
    let current = APP_VERSION.to_string();

    // 主端点：updater manifest（release 资产下载，不计 GitHub API 配额）。
    if let Ok(manifest) = fetch_manifest_version(&owner, &repo, proxy).await {
        let version = manifest
            .get("version")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        let latest = version.strip_prefix('v').unwrap_or(version).to_string();
        let tag = format!("v{latest}");
        let release_name = manifest
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or(&tag)
            .to_string();
        let published_at = manifest
            .get("pub_date")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let value = json!({
            "ok": true,
            "current": current,
            "latest": latest,
            "latestTag": tag,
            "hasUpdate": compare_versions(&latest, &current) > 0,
            "releaseName": release_name,
            "releaseUrl": release_url,
            "publishedAt": published_at,
            "checkedAt": now_secs(),
        });
        *CACHE.lock().unwrap() = Some(CachedCheck {
            checked_at: now_secs(),
            value: value.clone(),
        });
        return value;
    }

    // 兜底端点：`/releases/latest` 的 302 Location 头解析 tag（仅 manifest 失败时）。
    match fetch_latest_tag(&owner, &repo, proxy).await {
        Ok(tag) => {
            let latest = tag.strip_prefix('v').unwrap_or(&tag).to_string();
            let value = json!({
                "ok": true,
                "current": current,
                "latest": latest,
                "latestTag": tag,
                "hasUpdate": compare_versions(&latest, &current) > 0,
                "releaseName": tag.clone(),
                "releaseUrl": release_url,
                "checkedAt": now_secs(),
            });
            *CACHE.lock().unwrap() = Some(CachedCheck {
                checked_at: now_secs(),
                value: value.clone(),
            });
            value
        }
        Err((msg, code)) => json!({
            "ok": false,
            "error": msg,
            "message": format!("{msg}（code={code}）"),
            "releaseUrl": release_url,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `APP_VERSION` 与 `tauri.conf.json` 的 version 必须一致。
    ///
    /// 两者都会显示给用户（界面版本卡片、安装包文件名），不一致时会出现
    /// 「界面说 1.0.0、安装包叫 0.1.60」这种没法解释的现象。
    /// 改版本时最容易漏改其中一处，所以用测试锁住。
    #[test]
    fn cargo_and_tauri_versions_match() {
        let conf_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../src-tauri/tauri.conf.json");
        let text = std::fs::read_to_string(&conf_path)
            .unwrap_or_else(|e| panic!("读不到 {}：{e}", conf_path.display()));
        let v: Value = serde_json::from_str(&text).expect("tauri.conf.json 不是合法 JSON");
        let tauri_version = v.get("version").and_then(Value::as_str).expect("缺少 version 字段");
        assert_eq!(
            APP_VERSION, tauri_version,
            "Cargo.toml 的 version 与 tauri.conf.json 的 version 不一致"
        );
    }

    /// updater 必须指向**本项目**的发布源，且带一对能用的公钥。
    ///
    /// 三个字段任何一处漏改，症状都很隐蔽：
    /// - `active: false` → 手动检查能查出版本，但下载/安装永远不动；
    /// - endpoints 缺省 → 插件不知道去哪取清单；
    /// - pubkey 是上游的 → bundler 警告「签名与公钥不匹配」，下载回来的包
    ///   验签失败，用户只看到「安装失败」四个字。
    #[test]
    fn updater_points_at_own_repo_with_real_pubkey() {
        let conf_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../src-tauri/tauri.conf.json");
        let text = std::fs::read_to_string(&conf_path).unwrap();
        let v: Value = serde_json::from_str(&text).unwrap();
        let updater = &v["plugins"]["updater"];
        assert_eq!(
            updater.get("active").and_then(Value::as_bool),
            Some(true),
            "updater 未启用"
        );

        let expected = format!(
            "https://github.com/{GITHUB_OWNER}/{GITHUB_REPO}/releases/latest/download/latest.json"
        );
        let endpoints = updater
            .get("endpoints")
            .and_then(Value::as_array)
            .expect("endpoints 缺失：插件不知道该去哪取更新清单");
        assert!(
            endpoints
                .iter()
                .filter_map(Value::as_str)
                .any(|url| url == expected),
            "endpoints 必须包含本项目清单地址 {expected}，实际 {endpoints:?}"
        );

        // pubkey 是 bundler 的必填项，而且必须是 base64 的 minisign 公钥文本
        // （全 0 或纯 ASCII 会分别报 invalid utf-8 / Base64 conversion failed）。
        let pubkey = updater
            .get("pubkey")
            .and_then(Value::as_str)
            .expect("pubkey 是 bundler 必填项，删掉会在打包阶段失败");
        let decoded = base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            pubkey.as_bytes(),
        )
        .expect("pubkey 不是合法 base64");
        let public_text = String::from_utf8(decoded).expect("pubkey 必须是 minisign 公钥文本");
        assert!(
            public_text.starts_with("untrusted comment: minisign public key"),
            "pubkey 不是 minisign 公钥：{public_text}"
        );
    }

    /// 自动更新开关必须在**配置与代码两边**同时打开。
    ///
    /// `active: true` 只让 Tauri 的 updater 插件工作，而后台定时检查是自己写的
    /// 循环（`update_service::spawn_periodic_check`）。只改配置不改常量，表现是
    /// 「手动检查能查、后台永远不提醒」，很难查；只改常量不改配置则相反。
    #[test]
    fn background_update_switch_is_on_and_gated() {
        let src = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../src-tauri/src/update_service.rs"),
        )
        .expect("读不到 update_service.rs");
        assert!(
            src.contains("pub const UPDATER_ENABLED: bool = true;"),
            "UPDATER_ENABLED 必须为 true，否则后台检查与手动检查都会被短路"
        );
        // 循环入口必须仍受这个常量约束（将来要临时关掉时只改常量就够）。
        let body = src
            .split("pub fn spawn_periodic_check")
            .nth(1)
            .expect("找不到 spawn_periodic_check");
        let head: String = body.chars().take(700).collect();
        assert!(
            head.contains("UPDATER_ENABLED"),
            "spawn_periodic_check 里必须用 UPDATER_ENABLED 短路"
        );
    }

    /// 默认更新源必须锁在本项目仓库。
    ///
    /// 为什么值得锁：`load_github_config()` 以这两个常量作默认值。一旦被改回
    /// 上游，用户只会看到「发现新版本」，装上后才发现网关功能消失了。
    #[test]
    fn default_repo_points_at_own_release() {
        assert_eq!(
            (GITHUB_OWNER, GITHUB_REPO),
            ("cxkzhengmy", "workbuddy-router-switch"),
            "默认更新源必须是本项目仓库"
        );
        assert_ne!(GITHUB_OWNER, "changexbc", "不能指回上游仓库");
    }

    /// macOS 的 `os` 与 `os_slug` 相同，若不加判断会追加一份与主端点
    /// 重复的 `latest-macos-<arch>.json`，白跑一次请求。
    #[test]
    fn updater_manifest_urls_macos_skips_duplicate_fallback() {
        let urls = updater_manifest_urls(GITHUB_OWNER, GITHUB_REPO, "macos", "aarch64");
        let base = format!(
            "https://github.com/{GITHUB_OWNER}/{GITHUB_REPO}/releases/latest/download"
        );
        assert_eq!(
            urls,
            vec![
                format!("{base}/latest.json"),
                format!("{base}/latest-macos-aarch64.json"),
            ],
            "macOS 不该再追加一份重复的清单地址"
        );
    }

    /// Windows 仍要兜底问一次 `latest-macos-<arch>.json`：早期安装包里把
    /// 平台后缀写死成了 macos，删掉这个兜底会让那批用户的清单请求 404。
    #[test]
    fn updater_manifest_urls_windows_keeps_macos_compat() {
        let urls = updater_manifest_urls(GITHUB_OWNER, GITHUB_REPO, "windows", "x86_64");
        let base = format!(
            "https://github.com/{GITHUB_OWNER}/{GITHUB_REPO}/releases/latest/download"
        );
        assert_eq!(
            urls,
            vec![
                format!("{base}/latest.json"),
                format!("{base}/latest-windows-x86_64.json"),
                format!("{base}/latest-macos-x86_64.json"),
            ]
        );
    }

    #[test]
    fn compare_versions_orders_semver() {
        assert_eq!(compare_versions("0.1.18", "0.1.18"), 0);
        assert_eq!(compare_versions("0.1.19", "0.1.18"), 1);
        assert_eq!(compare_versions("v0.1.17", "0.1.18"), -1);
    }
}
