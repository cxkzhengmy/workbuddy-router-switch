//! axum 服务：对外暴露 OpenAI 兼容的 `/v1`，对内转发到 WorkBuddy 上游。
//!
//! 只实现 Chat Completions 与 Models 两个面——`/v1/responses`、`/v1/messages`
//! 的协议差异（事件模型、thinking 块）会成倍放大维护面，本项目用不到。
//!
//! 服务默认只监听回环地址：它是一个本地代理，没有鉴权之外的访问控制，绑
//! 0.0.0.0 等于把账号能力暴露给整个局域网。确实需要局域网访问时，由用户在
//! 「网关」页显式打开开关（见 [`super::network`]），绑定地址随之改变。

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Instant;

use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use futures_util::StreamExt;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, watch};
use tokio_stream::wrappers::ReceiverStream;

use super::account::{self, ActiveAccount};
use super::external;
use super::store::{self, RequestRecord};
use super::{catalog, upstream};

#[derive(Default)]
pub struct AppState {}

type SharedState = Arc<AppState>;

/// 记一行访问日志。
///
/// 「客户端连不上」是最难远程排查的一类问题：请求可能在鉴权、路由、
/// 预检任何一层就被挡掉，而这些分支都不会写台账。把 method / path /
/// 状态码 / 是否带头 / UA 落盘，一眼就能看出是没发请求、路径不对、
/// 还是 Key 不对。
async fn log_access(req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let has_auth = req.headers().contains_key(header::AUTHORIZATION)
        || req.headers().contains_key("x-api-key");
    let agent = req
        .headers()
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("-")
        .chars()
        .take(60)
        .collect::<String>();
    let origin = req
        .headers()
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("-")
        .chars()
        .take(40)
        .collect::<String>();
    let started = Instant::now();
    let response = next.run(req).await;
    let elapsed = started.elapsed().as_millis();
    append_access_log(&format!(
        "{method} {path} -> {} auth={} origin={origin} ua={agent} {elapsed}ms",
        response.status().as_u16(),
        if has_auth { "y" } else { "n" },
    ));
    response
}

/// 追加到 `~/.wb-switch/proxy-requests.log`，超过 2MB 时整体截断重写。
///
/// 不做轮转：这是排障日志，不是审计流水，留最近几千行足够。
fn append_access_log(line: &str) {
    use std::io::Write;
    let path = crate::modules::config::store_dir().join("proxy-requests.log");
    if let Ok(meta) = std::fs::metadata(&path) {
        if meta.len() > 2 * 1024 * 1024 {
            let _ = std::fs::write(&path, "");
        }
    }
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let stamp = chrono::Local::now().format("%m-%d %H:%M:%S");
        let _ = writeln!(file, "[{stamp}] {line}");
    }
}

/// 预检与 CORS。
///
/// 这里必须用中间件而不是 `fallback`：axum 在「路径存在但方法不允许」时
/// 直接回 405，不会落到 fallback——浏览器客户端在 POST 前发的 OPTIONS
/// 预检正好命中这种情况，于是被挡在门外，请求根本不会发出（台账里
/// 一条记录都没有，排查时非常误导）。
///
/// 顺带给所有响应补上 CORS 头：没有它，浏览器端连错误响应体都读不到，
/// 界面上只剩一句笼统的「请求失败」。
async fn cors(req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let is_preflight = req.method() == Method::OPTIONS;
    let mut response = if is_preflight {
        StatusCode::NO_CONTENT.into_response()
    } else {
        next.run(req).await
    };
    let headers = response.headers_mut();
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static(
            "authorization, content-type, x-api-key, anthropic-version, anthropic-beta",
        ),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, POST, OPTIONS"),
    );
    headers.insert(
        header::ACCESS_CONTROL_MAX_AGE,
        HeaderValue::from_static("86400"),
    );
    response
}

pub fn router() -> Router<SharedState> {
    Router::new()
        .route("/health", get(health))
        .route("/v1/models", get(models))
        .route("/v1/chat/completions", post(chat))
        // axum 默认只放 2MB 请求体，而带图片的 base64 或长上下文很容易超过，
        // 表现为莫名其妙的 413。放到 32MB：足够长对话，又不至于让一个坏请求
        // 把内存吃干。
        .layer(DefaultBodyLimit::max(32 * 1024 * 1024))
        .layer(axum::middleware::from_fn(log_access))
        // 放在 log_access 外层：预检也要进日志，否则「客户端没发请求」
        // 和「预检被拒」在日志里长得一样。
        .layer(axum::middleware::from_fn(cors))
        // 浏览器客户端会先发 OPTIONS 预检。
        .fallback(|method: Method| async move {
            if method == Method::OPTIONS {
                preflight()
            } else {
                status_page(StatusCode::NOT_FOUND, "not found")
            }
        })
}

/// 用已绑定的 listener 提供服务。
///
/// 绑定地址不在这里决定：`start()` 按网络策略先绑好再交给本函数，这样
/// 「端口被占用」能在 spawn 之前就以 `Err` 返回，界面不用等探测超时。
pub async fn serve_listener(
    listener: tokio::net::TcpListener,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), String> {
    let app = router().with_state(Arc::new(AppState::default()));
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = shutdown.changed().await;
        })
        .await
        .map_err(|e| format!("网关服务异常退出：{e}"))
}

// ---------------------------------------------------------------------------
// 基础端点
// ---------------------------------------------------------------------------

async fn health() -> Response {
    let acc = account::resolve();
    let body = match acc {
        Ok(a) => json!({
            "ok": true,
            "service": "wb-proxy-rs",
            // 只回掩码：/health 是无鉴权探活端点，原样返回手机号/邮箱
            // 等于把账号标识交给任何能访问本机端口的程序。
            "account": mask_label(&a.label),
            "realm": a.variant.as_str(),
            "keys": store::key_count(),
        }),
        Err(err) => json!({
            "ok": false,
            "service": "wb-proxy-rs",
            "error": err,
            "keys": store::key_count(),
        }),
    };
    json_response(StatusCode::OK, body)
}

async fn models(State(_): State<SharedState>, headers: HeaderMap) -> Response {
    let Some((_key_id, allowed)) = auth(&headers) else {
        return unauthorized();
    };
    // 按策略列出所有候选账号的模型并集：账号池混档位时，只列「当前账号」
    // 的模型会让客户端选不到实际能用的那一半。
    let identity = account::policy()
        .get("identity")
        .and_then(Value::as_str)
        .map(|raw| upstream::Identity::parse(Some(raw)))
        .unwrap_or_default();
    let (all, err) = catalog::list_for_policy(identity, false).await;
    if all.is_empty() {
        return status_page(
            StatusCode::SERVICE_UNAVAILABLE,
            err.as_deref().unwrap_or("模型目录为空，请检查账号状态"),
        );
    }
    let list = catalog::filter(&all, allowed.as_ref());
    let mut data: Vec<Value> = list
        .iter()
        .map(|m| {
            json!({
                "id": m.get("id").cloned().unwrap_or(Value::Null),
                "object": "model",
                "owned_by": "workbuddy",
                "name": m.get("name").cloned().unwrap_or(Value::Null),
            })
        })
        .collect();

    // 外部上游的模型一并列出（带 opencode/ 前缀）。Key 白名单同样约束它们——
    // 否则「限定了模型的 Key」会因为这个入口拿到未授权的模型。
    for model in external::models_for_listing() {
        let id = model.get("id").cloned().unwrap_or(Value::Null);
        if let Some(allow) = allowed.as_ref() {
            if !allow.is_empty() && !allow.iter().any(|a| Value::String(a.clone()) == id) {
                continue;
            }
        }
        data.push(model);
    }

    json_response(StatusCode::OK, json!({ "object": "list", "data": data }))
}

// ---------------------------------------------------------------------------
// Chat Completions
// ---------------------------------------------------------------------------

async fn chat(State(_): State<SharedState>, headers: HeaderMap, body: Bytes) -> Response {
    let Some((key_id, allowed)) = auth(&headers) else {
        return unauthorized();
    };
    let payload: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return status_page(StatusCode::BAD_REQUEST, &format!("请求体不是合法 JSON：{e}")),
    };
    if payload.get("messages").and_then(Value::as_array).is_none() {
        return status_page(StatusCode::BAD_REQUEST, "缺少 messages 字段");
    }
    let model = payload
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if model.is_empty() {
        return status_page(StatusCode::BAD_REQUEST, "缺少 model 字段");
    }
    // Key 的模型白名单：非空即视为「只允许这些」。放行未知模型名反而是隐患，
    // 用户配了白名单就是明确要限制。
    if let Some(list) = allowed.as_ref() {
        if !list.is_empty() && !list.iter().any(|m| m == &model) {
            return status_page(
                StatusCode::FORBIDDEN,
                &format!("当前 API Key 不允许使用模型 {model}"),
            );
        }
    }

    let client_stream = payload
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    // 外部上游（OpenCode 等）：带前缀的模型直接转发，不经过 WorkBuddy
    // 的账号链路——那边一套身份头、会话 id、流式聚合在这里都用不上。
    if let Some(external_model) = model.strip_prefix(external::PREFIX) {
        return external_chat(external_model, &payload, &key_id, client_stream).await;
    }

    let candidates = match account::resolve_candidates() {
        Ok(list) if !list.is_empty() => list,
        Ok(_) => return status_page(StatusCode::SERVICE_UNAVAILABLE, "账号池里没有可用账号"),
        Err(err) => return status_page(StatusCode::SERVICE_UNAVAILABLE, &err),
    };
    let policy = account::policy();
    let failover = policy.get("failover").and_then(Value::as_bool).unwrap_or(true);
    let identity = upstream::Identity::parse(policy.get("identity").and_then(Value::as_str));
    let mut upstream_body = upstream::build_body(&payload);
    // 上游只接受流式：实测 `stream:false` 直接 400
    // "Non-stream chat request is currently not supported"。
    // 所以一律按流式发，客户端要非流式时由网关自己聚合。
    if let Some(obj) = upstream_body.as_object_mut() {
        obj.insert("stream".into(), json!(true));
    }
    let started = Instant::now();

    let session = session_fingerprint(&upstream_body);
    let (resp, acc) =
        match open_with_failover(&upstream_body, &candidates, &key_id, failover, &session, identity)
            .await
        {
            Ok(pair) => pair,
            Err((code, message)) => {
                store::record(&RequestRecord {
                    key_id: Some(key_id.clone()),
                    account_uid: candidates.first().map(|a| a.uid.clone()),
                    account_label: candidates.first().map(|a| a.label.clone()),
                    model: model.clone(),
                    stream: client_stream,
                    status: code,
                    total_ms: Some(started.elapsed().as_millis() as i64),
                    error: Some(message.clone()),
                    ..Default::default()
                });
                return json_response(
                    StatusCode::from_u16(code).unwrap_or(StatusCode::BAD_GATEWAY),
                    json!({
                        "error": {
                            "message": message,
                            "type": "upstream_error",
                            "code": code,
                        }
                    }),
                );
            }
        };

    if client_stream {
        return stream_response(resp, key_id, acc, model, started).await;
    }
    aggregate_and_respond(resp, key_id, acc, model, started).await
}

/// 按候选顺序发起请求，失败时换下一个账号。
///
/// 只在「换号有意义」的失败上切换：401（凭据失效）、403 / 429（风控与限流）、
/// 5xx（上游抖动）、网络错误。参数类 4xx 直接返回——换号只会把同一个错误
/// 重放给另一个账号，白白制造风控样本。
async fn open_with_failover(
    body: &Value,
    candidates: &[ActiveAccount],
    key_id: &str,
    failover: bool,
    session: &str,
    identity: upstream::Identity,
) -> Result<(reqwest::Response, ActiveAccount), (u16, String)> {
    // 一次请求最多试 3 个账号：池子再大也不该扫完，否则一个坏请求会被
    // 放大成 N 倍失败记录，还平白给每个账号都记一笔。
    let limit = if failover { candidates.len().min(3) } else { 1 };
    let mut attempts = 0usize;
    let mut last: Option<(u16, String)> = None;

    for candidate in candidates {
        if attempts >= limit {
            break;
        }
        if account::is_cooling(&candidate.uid) {
            // 冷却中的账号这一轮跳过；如果全都在冷却，下面会带着具体账号名
            // 返回，比一个空泛的 503 好排查。
            last.get_or_insert_with(|| {
                (
                    503,
                    format!("账号「{}」刚失败过，正在冷却", candidate.label),
                )
            });
            continue;
        }
        attempts += 1;
        match try_account(candidate.clone(), body, key_id, session, identity).await {
            Ok(pair) => return Ok(pair),
            Err((code, message, retryable)) => {
                last = Some((code, message));
                if !retryable || !failover {
                    break;
                }
            }
        }
    }
    Err(last.unwrap_or((503, "没有可用账号".to_string())))
}

/// 用单个账号发一次请求；401 会先刷新再重试该账号。
///
/// 第三个返回值表示「换号有没有意义」：true 才会被 `open_with_failover`
/// 继续拿去试下一个账号。
async fn try_account(
    acc: ActiveAccount,
    body: &Value,
    key_id: &str,
    session: &str,
    identity: upstream::Identity,
) -> Result<(reqwest::Response, ActiveAccount), (u16, String, bool)> {
    let conv = conversation_id(&acc, key_id, session, identity);
    let resp = match upstream::open_chat(&acc, body, conv.as_deref(), identity).await {
        Ok(r) => r,
        Err(err) => {
            account::mark_cooldown(&acc.uid, 15);
            return Err((502, err, true));
        }
    };
    let status = resp.status();
    if status.is_success() {
        return Ok((resp, acc));
    }

    if status == reqwest::StatusCode::UNAUTHORIZED {
        // 先刷新再重试同一个账号。多数 401 只是 access token 过期，
        // 立刻换号等于把可恢复的问题扩大成「换个号也不行」。
        let fresh = match account::refresh(&acc).await {
            Ok(f) => f,
            Err(err) => {
                account::mark_cooldown(&acc.uid, 300);
                return Err((401, err, true));
            }
        };
        let conv2 = conversation_id(&fresh, key_id, session, identity);
        match upstream::open_chat(&fresh, body, conv2.as_deref(), identity).await {
            Ok(retry) if retry.status().is_success() => return Ok((retry, fresh)),
            Ok(retry) => {
                let code = retry.status().as_u16();
                let text = summarize_error(&retry.text().await.unwrap_or_default());
                account::mark_cooldown(&fresh.uid, 120);
                return Err((code, text, true));
            }
            Err(err) => {
                account::mark_cooldown(&fresh.uid, 60);
                return Err((502, err, true));
            }
        }
    }

    let code = status.as_u16();
    let text = summarize_error(&resp.text().await.unwrap_or_default());
    let retryable = status == reqwest::StatusCode::TOO_MANY_REQUESTS
        || status == reqwest::StatusCode::FORBIDDEN
        || status.is_server_error();
    if retryable {
        account::mark_cooldown(&acc.uid, 30);
    }
    Err((code, text, retryable))
}

/// 外部上游的一次对话：请求体原样转发，响应体原样透传。
///
/// 这里刻意不做协议转换——对端就是标准 OpenAI 接口。唯一要额外做的是
/// 记账：把上游返回的 `usage` 读出来写台账，否则外部模型的消耗在页面上
/// 完全看不见。
async fn external_chat(
    model: &str,
    payload: &Value,
    key_id: &str,
    client_stream: bool,
) -> Response {
    let started = Instant::now();
    let display_model = format!("{}{model}", external::PREFIX);

    let resp = match external::forward(payload, model).await {
        Ok(r) => r,
        Err((code, message)) => {
            store::record(&RequestRecord {
                key_id: Some(key_id.to_string()),
                account_label: Some("opencode".to_string()),
                model: display_model,
                stream: client_stream,
                status: code,
                total_ms: Some(started.elapsed().as_millis() as i64),
                error: Some(message.clone()),
                ..Default::default()
            });
            return json_response(
                StatusCode::from_u16(code).unwrap_or(StatusCode::BAD_GATEWAY),
                json!({ "error": { "message": message, "type": "upstream_error", "code": code } }),
            );
        }
    };

    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        let summary = summarize_error(&text);
        let code = status.as_u16();
        store::record(&RequestRecord {
            key_id: Some(key_id.to_string()),
            account_label: Some("opencode".to_string()),
            model: display_model,
            stream: client_stream,
            status: code,
            total_ms: Some(started.elapsed().as_millis() as i64),
            error: Some(summary.clone()),
            ..Default::default()
        });
        return json_response(
            StatusCode::from_u16(code).unwrap_or(StatusCode::BAD_GATEWAY),
            json!({ "error": { "message": summary, "type": "upstream_error", "code": code } }),
        );
    }

    if client_stream {
        return stream_external(resp, key_id.to_string(), display_model, started).await;
    }

    // 非流式：上游已经给了完整 JSON，原样交给客户端，只顺手记一笔用量。
    let text = resp.text().await.unwrap_or_default();
    let usage = serde_json::from_str::<Value>(&text)
        .ok()
        .map(|v| Usage::from_value(v.get("usage")))
        .unwrap_or_default();
    store::record(&RequestRecord {
        key_id: Some(key_id.to_string()),
        account_uid: None,
        account_label: Some("opencode".to_string()),
        model: display_model,
        stream: false,
        status: 200,
        prompt_tokens: usage.prompt,
        completion_tokens: usage.completion,
        cached_tokens: usage.cached,
        total_tokens: usage.total,
        credit: usage.credit,
        ttft_ms: None,
        total_ms: Some(started.elapsed().as_millis() as i64),
        error: None,
    });
    let mut response = Response::new(Body::from(text));
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    apply_cors(response.headers_mut());
    response
}

/// 外部上游的流式透传：字节原样转发，旁路解析 usage 记账。
fn stream_external(
    resp: reqwest::Response,
    key_id: String,
    model: String,
    started: Instant,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send>> {
    Box::pin(async move {
        let (tx, rx) = mpsc::channel::<Result<Bytes, Infallible>>(32);
        tokio::spawn(async move {
            let mut upstream_stream = resp.bytes_stream();
            let mut buffer = String::new();
            let mut first_token_ms: Option<i64> = None;
            let mut usage = Usage::default();
            let mut failed: Option<String> = None;

            while let Some(chunk) = upstream_stream.next().await {
                match chunk {
                    Ok(bytes) => {
                        feed_sse(&bytes, &mut buffer, &mut usage, &mut first_token_ms, started);
                        if tx.send(Ok(bytes)).await.is_err() {
                            failed = Some("客户端提前断开".to_string());
                            break;
                        }
                    }
                    Err(err) => {
                        failed = Some(format!("上游流中断：{err}"));
                        break;
                    }
                }
            }
            store::record(&RequestRecord {
                key_id: Some(key_id),
                account_uid: None,
                account_label: Some("opencode".to_string()),
                model,
                stream: true,
                status: if failed.is_some() { 499 } else { 200 },
                prompt_tokens: usage.prompt,
                completion_tokens: usage.completion,
                cached_tokens: usage.cached,
                total_tokens: usage.total,
                credit: usage.credit,
                ttft_ms: first_token_ms,
                total_ms: Some(started.elapsed().as_millis() as i64),
                error: failed,
            });
        });

        let mut response = Response::new(Body::from_stream(ReceiverStream::new(rx)));
        *response.status_mut() = StatusCode::OK;
        let headers = response.headers_mut();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream; charset=utf-8"),
        );
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
        apply_cors(headers);
        response
    })
}

fn stream_response(
    resp: reqwest::Response,
    key_id: String,
    acc: ActiveAccount,
    model: String,
    started: Instant,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send>> {
    Box::pin(async move {
        let (tx, rx) = mpsc::channel::<Result<Bytes, Infallible>>(32);
        tokio::spawn(async move {
            let mut upstream_stream = resp.bytes_stream();
            let mut buffer = String::new();
            let mut first_token_ms: Option<i64> = None;
            let mut usage = Usage::default();
            let mut failed: Option<String> = None;

            while let Some(chunk) = upstream_stream.next().await {
                match chunk {
                    Ok(bytes) => {
                        feed_sse(&bytes, &mut buffer, &mut usage, &mut first_token_ms, started);
                        if tx.send(Ok(bytes)).await.is_err() {
                            // 客户端断开：上游连接随之关闭，但这一笔仍要记账，
                            // 否则用户看到「扣了积分却没有请求记录」。
                            failed = Some("客户端提前断开".to_string());
                            break;
                        }
                    }
                    Err(err) => {
                        failed = Some(format!("上游流中断：{err}"));
                        break;
                    }
                }
            }
            store::record(&RequestRecord {
                key_id: Some(key_id),
                account_uid: Some(acc.uid.clone()),
                account_label: Some(acc.label.clone()),
                model,
                stream: true,
                status: if failed.is_some() { 499 } else { 200 },
                prompt_tokens: usage.prompt,
                completion_tokens: usage.completion,
                cached_tokens: usage.cached,
                total_tokens: usage.total,
                credit: usage.credit,
                ttft_ms: first_token_ms,
                total_ms: Some(started.elapsed().as_millis() as i64),
                error: failed,
            });
        });

        let mut response = Response::new(Body::from_stream(ReceiverStream::new(rx)));
        *response.status_mut() = StatusCode::OK;
        let headers = response.headers_mut();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream; charset=utf-8"),
        );
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
        apply_cors(headers);
        response
    })
}

/// 把上游流式响应折叠成一个非流式的 chat.completion。
///
/// 上游没有非流式接口，所以「客户端要 stream:false」只能由网关自己做：
/// 收全 SSE，拼接 content / reasoning_content / tool_calls，再一次性返回。
async fn aggregate_and_respond(
    resp: reqwest::Response,
    key_id: String,
    acc: ActiveAccount,
    model: String,
    started: Instant,
) -> Response {
    let mut stream = resp.bytes_stream();
    let mut buffer = String::new();
    let mut agg = Aggregate::new(&model);
    let mut failed: Option<String> = None;

    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(bytes) => agg.feed(&bytes, &mut buffer, started),
            Err(err) => {
                failed = Some(format!("上游流中断：{err}"));
                break;
            }
        }
    }

    let usage = agg.usage.clone();
    store::record(&RequestRecord {
        key_id: Some(key_id),
        account_uid: Some(acc.uid.clone()),
        account_label: Some(acc.label.clone()),
        model: agg.model.clone(),
        stream: false,
        status: if failed.is_some() { 502 } else { 200 },
        prompt_tokens: usage.prompt,
        completion_tokens: usage.completion,
        cached_tokens: usage.cached,
        total_tokens: usage.total,
        credit: usage.credit,
        ttft_ms: agg.first_token_ms,
        total_ms: Some(started.elapsed().as_millis() as i64),
        error: failed.clone(),
    });

    if let Some(err) = failed {
        return status_page(StatusCode::BAD_GATEWAY, &err);
    }
    json_response(StatusCode::OK, agg.into_json())
}

/// 流式分片的聚合状态。
#[derive(Default)]
struct Aggregate {
    id: String,
    model: String,
    content: String,
    reasoning: String,
    finish: String,
    calls: std::collections::BTreeMap<i64, ToolCall>,
    usage: Usage,
    first_token_ms: Option<i64>,
}

#[derive(Default, Clone)]
struct ToolCall {
    id: String,
    name: String,
    arguments: String,
}

impl Aggregate {
    fn new(model: &str) -> Self {
        Self {
            id: String::new(),
            model: model.to_string(),
            finish: "stop".to_string(),
            ..Default::default()
        }
    }

    fn feed(&mut self, bytes: &[u8], buffer: &mut String, started: Instant) {
        buffer.push_str(&String::from_utf8_lossy(bytes));
        while let Some(pos) = buffer.find('\n') {
            let line: String = buffer.drain(..=pos).collect();
            let line = line.trim();
            let Some(data) = line.strip_prefix("data:") else {
                continue;
            };
            let data = data.trim();
            if data.is_empty() || data == "[DONE]" {
                continue;
            }
            let Ok(chunk) = serde_json::from_str::<Value>(data) else {
                continue;
            };
            if let Some(id) = chunk.get("id").and_then(Value::as_str) {
                self.id = id.to_string();
            }
            if let Some(model) = chunk.get("model").and_then(Value::as_str) {
                self.model = model.to_string();
            }
            if let Some(u) = chunk.get("usage").filter(|v| v.is_object()) {
                // 上游可能在多个分片重复带 usage，取累计最大的那份。
                let parsed = Usage::from_value(Some(u));
                if parsed.total >= self.usage.total {
                    self.usage = parsed;
                }
            }
            let Some(choices) = chunk.get("choices").and_then(Value::as_array) else {
                continue;
            };
            for choice in choices {
                let Some(delta) = choice.get("delta") else {
                    continue;
                };
                if let Some(text) = delta.get("content").and_then(Value::as_str) {
                    if !text.is_empty() {
                        if self.first_token_ms.is_none() {
                            self.first_token_ms = Some(started.elapsed().as_millis() as i64);
                        }
                        self.content.push_str(text);
                    }
                }
                if let Some(text) = delta.get("reasoning_content").and_then(Value::as_str) {
                    if !text.is_empty() {
                        if self.first_token_ms.is_none() {
                            self.first_token_ms = Some(started.elapsed().as_millis() as i64);
                        }
                        self.reasoning.push_str(text);
                    }
                }
                if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                    if self.first_token_ms.is_none() {
                        self.first_token_ms = Some(started.elapsed().as_millis() as i64);
                    }
                    for (pos, call) in calls.iter().enumerate() {
                        let index = call
                            .get("index")
                            .and_then(Value::as_i64)
                            .unwrap_or(pos as i64);
                        let entry = self.calls.entry(index).or_default();
                        if let Some(id) = call.get("id").and_then(Value::as_str) {
                            entry.id = id.to_string();
                        }
                        if let Some(func) = call.get("function") {
                            if let Some(name) = func.get("name").and_then(Value::as_str) {
                                entry.name.push_str(name);
                            }
                            if let Some(args) = func.get("arguments").and_then(Value::as_str) {
                                entry.arguments.push_str(args);
                            }
                        }
                    }
                }
                if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                    self.finish = reason.to_string();
                }
            }
        }
        if buffer.len() > 1 << 20 {
            buffer.clear();
        }
    }

    fn into_json(self) -> Value {
        // 无函数名的 tool_call 是上游的空占位，泄漏给客户端会让它死等参数。
        let calls: Vec<ToolCall> = self
            .calls
            .into_values()
            .filter(|c| !c.name.is_empty())
            .collect();
        let mut message = json!({ "role": "assistant", "content": self.content });
        if !self.reasoning.is_empty() {
            message["reasoning_content"] = json!(self.reasoning);
        }
        let mut finish = self.finish;
        if !calls.is_empty() {
            let list: Vec<Value> = calls
                .iter()
                .map(|c| {
                    json!({
                        "id": if c.id.is_empty() {
                            format!("call_{}", uuid::Uuid::new_v4().simple())
                        } else {
                            c.id.clone()
                        },
                        "type": "function",
                        "function": { "name": c.name, "arguments": c.arguments },
                    })
                })
                .collect();
            message["tool_calls"] = Value::Array(list);
            if finish == "stop" {
                finish = "tool_calls".to_string();
            }
        }
        let id = if self.id.is_empty() {
            format!("chatcmpl-wb-{}", uuid::Uuid::new_v4().simple())
        } else {
            self.id
        };
        json!({
            "id": id,
            "object": "chat.completion",
            "created": chrono::Utc::now().timestamp(),
            "model": self.model,
            "choices": [{
                "index": 0,
                "message": message,
                "finish_reason": finish,
            }],
            "usage": {
                "prompt_tokens": self.usage.prompt,
                "completion_tokens": self.usage.completion,
                "total_tokens": self.usage.total,
                "credit": self.usage.credit,
            },
        })
    }
}

// ---------------------------------------------------------------------------
// SSE 解析
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone)]
struct Usage {
    prompt: i64,
    completion: i64,
    cached: i64,
    total: i64,
    credit: f64,
}

impl Usage {
    fn from_value(value: Option<&Value>) -> Self {
        let Some(usage) = value.filter(|v| v.is_object()) else {
            return Self::default();
        };
        let details = usage
            .get("completion_tokens_details")
            .or_else(|| usage.get("prompt_tokens_details"));
        Self {
            prompt: usage.get("prompt_tokens").and_then(Value::as_i64).unwrap_or(0),
            completion: usage
                .get("completion_tokens")
                .and_then(Value::as_i64)
                .unwrap_or(0),
            cached: usage
                .get("cache_read_input_tokens")
                .or_else(|| usage.get("cached_tokens"))
                .or_else(|| details.and_then(|d| d.get("cached_tokens")))
                .and_then(Value::as_i64)
                .unwrap_or(0),
            total: usage.get("total_tokens").and_then(Value::as_i64).unwrap_or(0),
            credit: usage.get("credit").and_then(Value::as_f64).unwrap_or(0.0),
        }
    }
}

/// 增量喂 SSE 字节，抽 usage 与首字时间。不修改转发内容。
fn feed_sse(
    bytes: &[u8],
    buffer: &mut String,
    usage: &mut Usage,
    first_token_ms: &mut Option<i64>,
    started: Instant,
) {
    buffer.push_str(&String::from_utf8_lossy(bytes));
    // 只处理完整的行，最后一段可能被切断，留在 buffer 里等下一块。
    while let Some(pos) = buffer.find('\n') {
        let line: String = buffer.drain(..=pos).collect();
        let line = line.trim();
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        let Ok(chunk) = serde_json::from_str::<Value>(data) else {
            continue;
        };
        if let Some(u) = chunk.get("usage").filter(|v| v.is_object()) {
            *usage = Usage::from_value(Some(u));
        }
        if first_token_ms.is_none() {
            let has_content = chunk
                .get("choices")
                .and_then(Value::as_array)
                .and_then(|c| c.first())
                .map(|c| {
                    let delta = c.get("delta");
                    delta
                        .and_then(|d| d.get("content"))
                        .map(|v| !v.is_null())
                        .unwrap_or(false)
                        || delta
                            .and_then(|d| d.get("tool_calls"))
                            .map(|v| !v.is_null())
                            .unwrap_or(false)
                })
                .unwrap_or(false);
            if has_content {
                *first_token_ms = Some(started.elapsed().as_millis() as i64);
            }
        }
    }
    // 防御：单行异常长时不要无限增长（上游不会这样，但坏数据不该拖垮进程）。
    if buffer.len() > 1 << 20 {
        buffer.clear();
    }
}

/// 会话指纹：取第一条 user 消息的内容。
///
/// 只按 Key 派生会让同一个 Key 下的所有对话共用一个 `X-Conversation-ID`，
/// 出站形态是「一个永不结束的超长会话」；按消息前缀派生后，同一对话的多轮
/// （客户端每轮重发完整历史）复用同一 id，不同对话各自新开——与官方客户端
/// 的行为一致。
fn session_fingerprint(body: &Value) -> String {
    let first_user = body
        .get("messages")
        .and_then(Value::as_array)
        .and_then(|list| {
            list.iter()
                .find(|msg| msg.get("role").and_then(Value::as_str) == Some("user"))
        })
        .map(|msg| match msg.get("content") {
            Some(Value::String(text)) => text.clone(),
            Some(other) => other.to_string(),
            None => String::new(),
        })
        .unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(first_user.as_bytes());
    let digest = hasher.finalize();
    digest[..8].iter().map(|b| format!("{b:02x}")).collect()
}

/// 按 (账号, Key, 会话) 派生稳定的会话 id。
///
/// 上游不拿这个 id 存取历史，但它会被风控用来判断「像不像人类客户端」。
/// 一次对话的每轮都换 id，等于把多轮拆成上千个一次性会话；按账号+Key 固定
/// 一个，多轮复用，既贴近官方语义又不会成为跨账号关联键。
fn conversation_id(
    acc: &ActiveAccount,
    key_id: &str,
    session: &str,
    identity: upstream::Identity,
) -> Option<String> {
    if key_id.is_empty() {
        return None;
    }
    let mut hasher = Sha256::new();
    hasher.update(acc.uid.as_bytes());
    hasher.update(b"|");
    hasher.update(key_id.as_bytes());
    hasher.update(b"|");
    hasher.update(session.as_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    // 按 RFC 4122 打上版本/变体位，形态与官方一致。
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let uuid = uuid::Uuid::from_bytes(bytes);
    // VSCode 扩展的 BaseAgent 用「uuidv4 去连字符」的 32 位形态，其余两种用
    // 带连字符的 UUID。形态不对不影响上游行为，但会让请求不像对应客户端。
    Some(match identity {
        upstream::Identity::Vscode => uuid.simple().to_string(),
        _ => uuid.to_string(),
    })
}

// ---------------------------------------------------------------------------
// 鉴权与响应工具
// ---------------------------------------------------------------------------

/// 取 Key：兼容 `Authorization: Bearer` 与 `x-api-key` 两种写法。
fn auth(headers: &HeaderMap) -> Option<(String, Option<Vec<String>>)> {
    let raw = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim())
        .filter(|v| !v.is_empty())
        .map(|v| {
            v.strip_prefix("Bearer ")
                .or_else(|| v.strip_prefix("bearer "))
                .unwrap_or(v)
                .trim()
                .to_string()
        })
        .or_else(|| {
            headers
                .get("x-api-key")
                .and_then(|v| v.to_str().ok())
                .map(|v| v.trim().to_string())
        });
    let secret = raw?;
    store::verify_key(&secret)
}

fn unauthorized() -> Response {
    let mut response = json_response(
        StatusCode::UNAUTHORIZED,
        json!({
            "error": {
                "message": "缺少或无效的 API Key。请在「网关」页创建一个 Key，并用 Authorization: Bearer <key> 调用。",
                "type": "invalid_request_error",
                "code": "invalid_api_key",
            }
        }),
    );
    response.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        HeaderValue::from_static("Bearer"),
    );
    response
}

fn json_response(status: StatusCode, body: Value) -> Response {
    let text = body.to_string();
    let mut response = Response::new(Body::from(text));
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    apply_cors(response.headers_mut());
    response
}

fn status_page(status: StatusCode, message: &str) -> Response {
    json_response(
        status,
        json!({ "error": { "message": message, "type": "proxy_error" } }),
    )
}

fn preflight() -> Response {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::NO_CONTENT;
    apply_cors(response.headers_mut());
    response
}

/// 只对 `/v1` 开 CORS：浏览器里的聊天客户端需要它，管理面不需要。
fn apply_cors(headers: &mut HeaderMap) {
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("authorization, content-type, x-api-key"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, POST, OPTIONS"),
    );
}

/// 账号标识脱敏：手机号留前 3 后 4，其余留前 2 后 2。
///
/// 不直接隐藏是因为诊断时仍要能区分「是哪个账号」；但完整值不该出现在
/// 一个无鉴权端点里。
fn mask_label(label: &str) -> String {
    let chars: Vec<char> = label.chars().collect();
    if chars.len() <= 4 {
        return "*".repeat(chars.len());
    }
    let keep_head = 3.min(chars.len() / 2);
    let keep_tail = 4.min(chars.len() / 2);
    let head: String = chars[..keep_head].iter().collect();
    let tail: String = chars[chars.len() - keep_tail..].iter().collect();
    format!("{head}***{tail}")
}

/// 把上游错误体压成一句人话。
fn summarize_error(text: &str) -> String {
    if let Ok(value) = serde_json::from_str::<Value>(text) {
        let candidates: [&[&str]; 4] = [
            &["error", "message"],
            &["error_msg"],
            &["message"],
            &["msg"],
        ];
        for path in candidates {
            let mut node = &value;
            let mut found = true;
            for key in path {
                match node.get(key) {
                    Some(next) => node = next,
                    None => {
                        found = false;
                        break;
                    }
                }
            }
            if found {
                if let Some(s) = node.as_str() {
                    return s.to_string();
                }
            }
        }
    }
    let trimmed = text.trim();
    if trimmed.is_empty() {
        "上游返回空错误体".to_string()
    } else {
        trimmed.chars().take(500).collect()
    }
}

/// 给宿主用的诊断信息（不含密钥）。
pub fn stats_snapshot() -> HashMap<String, Value> {
    let day = store::now_ms() - 86_400_000;
    let mut out = HashMap::new();
    out.insert("today".into(), store::summary(day));
    out.insert("models".into(), Value::Array(store::by_model(day)));
    out.insert("keys".into(), Value::Array(store::list_keys()));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_usage_from_sse() {
        let mut buffer = String::new();
        let mut usage = Usage::default();
        let mut ttft = None;
        let started = Instant::now();
        feed_sse(
            br#"data: {"choices":[{"delta":{"content":"hi"}}]}

"#,
            &mut buffer,
            &mut usage,
            &mut ttft,
            started,
        );
        assert!(ttft.is_some());
        feed_sse(
            br#"data: {"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15,"credit":0.2}}

"#,
            &mut buffer,
            &mut usage,
            &mut ttft,
            started,
        );
        assert_eq!(usage.total, 15);
        assert!((usage.credit - 0.2).abs() < f64::EPSILON);
    }

    #[test]
    fn handles_split_lines() {
        let mut buffer = String::new();
        let mut usage = Usage::default();
        let mut ttft = None;
        let started = Instant::now();
        feed_sse(br#"data: {"choices":[{"delta":{"cont"#, &mut buffer, &mut usage, &mut ttft, started);
        assert!(ttft.is_none());
        feed_sse(br#"ent":"x"}}]}"#, &mut buffer, &mut usage, &mut ttft, started);
        assert!(ttft.is_none());
        feed_sse(b"\n\n", &mut buffer, &mut usage, &mut ttft, started);
        assert!(ttft.is_some());
    }

    #[test]
    fn summarizes_error_bodies() {
        assert_eq!(
            summarize_error(r#"{"error":{"message":"bad model"}}"#),
            "bad model"
        );
        assert_eq!(summarize_error("plain text"), "plain text");
    }
}
