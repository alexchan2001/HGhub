//! HGhub 网页版：独立 axum HTTP 服务，复用桌面端 Rust 核心模块。
//!
//! - 业务接口：`POST /api/<命令名>`，JSON 请求/响应与 Tauri invoke 的参数和返回值一致。
//! - 媒体代理：`GET /media/img|hls|seg?u=...`，复用 `media::handle`，m3u8 改写前缀为 `/media`
//!  （同源），并支持 `Range` 请求（视频 seek 用）。
//! - 密码门（与上游账号登录 `api_login` 是两回事）：启动时读 `HG_WEB_PASSWORD`；
//!   未登录时 `/api` 与 `/media` 返回 401，页面请求重定向到 `/login`；
//!   登录成功发 HttpOnly cookie 会话。密码只存在内存里，不写日志、不写磁盘。
//! - 前端静态文件：`HG_WEB_DIST`（默认 `./dist`），SPA 回退到 `index.html`。
//! - 监听：`$PORT`（默认 8900）；数据目录：`HG_WEB_DATA`（默认 `./data`，
//!   存 session.json / line.json / 诊断日志，与桌面端同一套持久化逻辑）。
//! - 不动桌面端：本文件只新增一个 binary，不改 Tauri 的 `run()` 行为。

use axum::{
    body::Bytes,
    extract::{Path, RawQuery, State},
    http::{header, HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Redirect, Response},
    routing::{get, post},
    Json, Router,
};
use hghub_core::{
    api::{self, Api},
    media, net, playback_log,
};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    net::SocketAddr,
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const SESSION_COOKIE: &str = "hgweb_session";
const SESSION_TTL: Duration = Duration::from_secs(24 * 3600);
/// m3u8 改写前缀：同源相对路径，前端 `src/api.ts` 的 `LOCAL` 在网页模式下也是它。
const MEDIA_PREFIX: &str = "/media";

struct AppState {
    api: Arc<Api>,
    /// 网页访问密码（只放内存，绝不写日志/磁盘）。
    password: String,
    sessions: Mutex<HashMap<String, Instant>>,
    dist_dir: PathBuf,
}

type Shared = Arc<AppState>;

/* ───────────────────────── 会话 ───────────────────────── */

static TOKEN_COUNTER: AtomicU64 = AtomicU64::new(0);

fn new_token() -> String {
    let mut buf = [0u8; 32];
    let mut filled = false;
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        use std::io::Read;
        if f.read_exact(&mut buf).is_ok() {
            filled = true;
        }
    }
    if !filled {
        // 退路：时间 + 进程号 + 计数器（Linux 上基本走不到这里）。
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let n = TOKEN_COUNTER.fetch_add(1, Ordering::Relaxed);
        let seed = format!("{now}-{}-{n}", std::process::id());
        let mut h: u64 = 0xcbf29ce484222325;
        for b in seed.as_bytes() {
            h ^= *b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        buf[..8].copy_from_slice(&h.to_le_bytes());
        buf[8..16].copy_from_slice(&now.to_le_bytes()[..8]);
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// 固定时间比较，避免密码长度泄露时序信息。
fn password_ok(candidate: &str, expected: &str) -> bool {
    let a = candidate.as_bytes();
    let b = expected.as_bytes();
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for i in 0..a.len() {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

fn session_token(headers: &HeaderMap) -> Option<String> {
    let cookie = headers.get(header::COOKIE)?.to_str().ok()?;
    for part in cookie.split(';') {
        let part = part.trim();
        if let Some(tok) = part.strip_prefix(&format!("{SESSION_COOKIE}=")) {
            let tok = tok.trim();
            if !tok.is_empty() {
                return Some(tok.to_string());
            }
        }
    }
    None
}

fn is_authed(state: &Shared, headers: &HeaderMap) -> bool {
    let Some(tok) = session_token(headers) else {
        return false;
    };
    let mut sessions = match state.sessions.lock() {
        Ok(g) => g,
        Err(_) => return false,
    };
    match sessions.get(&tok) {
        Some(exp) if *exp > Instant::now() => true,
        _ => {
            sessions.remove(&tok);
            false
        }
    }
}

fn set_session_cookie(headers: &mut HeaderMap, token: &str) {
    let value = format!(
        "{SESSION_COOKIE}={token}; HttpOnly; Path=/; SameSite=Lax; Max-Age={}",
        SESSION_TTL.as_secs()
    );
    if let Ok(v) = value.parse() {
        headers.insert(header::SET_COOKIE, v);
    }
}

/* ───────────────────────── 登录页 ───────────────────────── */

fn login_html(error: Option<&str>) -> String {
    let err = error.map(|e| format!("<p class=\"err\">{e}</p>")).unwrap_or_default();
    format!(
        r#"<!doctype html>
<html lang="zh-CN">
<head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>登录 · HGhub</title>
<style>
body{{font-family:-apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif;background:#111;color:#eee;margin:0;display:flex;min-height:100vh;align-items:center;justify-content:center}}
main{{width:min(92vw,340px);background:#1c1c1e;border-radius:14px;padding:28px 24px;box-sizing:border-box}}
h1{{margin:0 0 4px;font-size:20px}}p{{color:#999;font-size:14px;margin:0 0 18px}}
input{{width:100%;box-sizing:border-box;padding:12px;border-radius:9px;border:1px solid #333;background:#000;color:#fff;font-size:16px;margin-bottom:12px}}
button{{width:100%;padding:12px;border:0;border-radius:9px;background:#e50914;color:#fff;font-size:16px}}
.err{{color:#ff6b6b!important}}
</style></head>
<body><main><h1>HGhub</h1><p>请输入访问密码</p>{err}
<form method="post" action="/login"><input type="password" name="password" placeholder="访问密码" required autofocus autocomplete="current-password"><button type="submit">登录</button></form>
</main></body></html>"#
    )
}

async fn login_page() -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        login_html(None),
    )
        .into_response()
}

async fn login_submit(
    State(state): State<Shared>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let password: Option<String> = if content_type.contains("application/json") {
        serde_json::from_slice::<Value>(&body)
            .ok()
            .and_then(|v| v.get("password").and_then(|p| p.as_str()).map(str::to_string))
    } else {
        url::form_urlencoded::parse(&body)
            .find(|(k, _)| k == "password")
            .map(|(_, v)| v.into_owned())
    };
    let wants_json = content_type.contains("application/json")
        || headers
            .get(header::ACCEPT)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.contains("application/json"));

    match password {
        Some(pw) if password_ok(&pw, &state.password) => {
            let token = new_token();
            if let Ok(mut sessions) = state.sessions.lock() {
                sessions.insert(token.clone(), Instant::now() + SESSION_TTL);
            }
            let mut resp_headers = HeaderMap::new();
            set_session_cookie(&mut resp_headers, &token);
            if wants_json {
                (StatusCode::OK, resp_headers, Json(json!({ "ok": true }))).into_response()
            } else {
                let mut resp = Redirect::to("/").into_response();
                resp.headers_mut().extend(resp_headers);
                resp
            }
        }
        _ => {
            if wants_json {
                (
                    StatusCode::UNAUTHORIZED,
                    Json(json!({ "error": "密码错误" })),
                )
                    .into_response()
            } else {
                (
                    StatusCode::UNAUTHORIZED,
                    [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
                    login_html(Some("密码错误，请重试")),
                )
                    .into_response()
            }
        }
    }
}

async fn healthz() -> Json<Value> {
    Json(json!({ "ok": true }))
}

/* ───────────────────────── /api 分发 ───────────────────────── */

fn arg_str(args: &Value, key: &str) -> Result<String, String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| format!("缺少参数 {key}"))
}

fn arg_str_opt(args: &Value, key: &str, default: &str) -> String {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| default.to_string())
}

fn arg_u32(args: &Value, key: &str, default: u32) -> Result<u32, String> {
    match args.get(key) {
        None => Ok(default),
        Some(v) => v
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .ok_or_else(|| format!("参数 {key} 不是有效的非负整数")),
    }
}

fn arg_i64(args: &Value, key: &str) -> Result<i64, String> {
    args.get(key)
        .and_then(|v| v.as_i64())
        .ok_or_else(|| format!("缺少参数 {key}"))
}

fn arg_bool(args: &Value, key: &str) -> Result<bool, String> {
    args.get(key)
        .and_then(|v| v.as_bool())
        .ok_or_else(|| format!("缺少参数 {key}"))
}

fn arg_str_list(args: &Value, key: &str) -> Result<Vec<String>, String> {
    args.get(key)
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .ok_or_else(|| format!("缺少参数 {key}"))
}

/// 可选整数：同时接受 snake_case（Tauri 参数名）与 camelCase（前端实际传参）。
fn arg_opt_i64(args: &Value, snake: &str, camel: &str) -> Option<i64> {
    args.get(snake)
        .or_else(|| args.get(camel))
        .and_then(|v| v.as_i64())
}

async fn api_dispatch(
    State(state): State<Shared>,
    Path(cmd): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !is_authed(&state, &headers) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "未登录" })),
        )
            .into_response();
    }
    let args: Value = if body.is_empty() {
        Value::Null
    } else {
        match serde_json::from_slice::<Value>(&body) {
            Ok(v) => v,
            Err(_) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": "请求体不是合法 JSON" })),
                )
                    .into_response()
            }
        }
    };
    const KNOWN_COMMANDS: &[&str] = &[
        "app_boot", "net_diag", "line_state", "line_set", "line_mirrors", "line_probe",
        "check_update", "security_set", "cache_clear", "api_login", "api_register",
        "api_logout", "api_me", "api_feed", "api_category", "api_rank", "api_search",
        "api_posts", "api_post", "api_video", "api_play", "api_comments", "api_comment_send",
        "api_like", "api_collect", "api_follow", "api_post_like", "api_post_collect",
        "api_post_comments", "playback_log_status", "playback_log_set", "playback_log_write",
        "playback_log_read",
    ];
    if !KNOWN_COMMANDS.contains(&cmd.as_str()) {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("未知命令 {cmd}") })),
        )
            .into_response();
    }
    let api = &state.api;
    let result: Result<Value, String> = async {
        match cmd.as_str() {
        "app_boot" => api.app_boot().await,
        "net_diag" => api::net_diag().await,
        "line_state" => api::line_state().await,
        "line_set" => {
            let url = arg_str(&args, "url")?;
            api.line_set(url).await.map(Value::String)
        }
        "line_mirrors" => api::line_mirrors().await,
        "line_probe" => {
            let urls = arg_str_list(&args, "urls")?;
            api::line_probe(urls).await
        }
        "check_update" => api::check_update().await,
        "security_set" => {
            let strict = arg_bool(&args, "strict")?;
            api::security_set(strict).await.map(Value::Bool)
        }
        "cache_clear" => api::cache_clear().await.map(|_| Value::Null),
        "api_login" => {
            let username = arg_str(&args, "username")?;
            let password = arg_str(&args, "password")?;
            api.login(username, password).await
        }
        "api_register" => {
            let username = arg_str(&args, "username")?;
            let password = arg_str(&args, "password")?;
            let email = arg_str_opt(&args, "email", "");
            api.register(username, password, email).await
        }
        "api_logout" => api.logout().await,
        "api_me" => api.me().await,
        "api_feed" => {
            let page = arg_u32(&args, "page", 1)?;
            let size = arg_u32(&args, "size", 20)?;
            api.feed(page, size).await
        }
        "api_category" => {
            let cate = arg_str(&args, "cate")?;
            let page = arg_u32(&args, "page", 1)?;
            let size = arg_u32(&args, "size", 20)?;
            api.category(cate, page, size).await
        }
        "api_rank" => {
            let kind = arg_str(&args, "kind")?;
            api.rank(kind).await
        }
        "api_search" => {
            let q = arg_str(&args, "q")?;
            let page = arg_u32(&args, "page", 1)?;
            let size = arg_u32(&args, "size", 20)?;
            api.search(q, page, size).await
        }
        "api_posts" => {
            let page = arg_u32(&args, "page", 1)?;
            let size = arg_u32(&args, "size", 20)?;
            api.posts(page, size).await
        }
        "api_post" => {
            let id = arg_i64(&args, "id")?;
            api.post_detail(id).await
        }
        "api_video" => {
            let id = arg_i64(&args, "id")?;
            api.video(id).await
        }
        "api_play" => {
            let id = arg_i64(&args, "id")?;
            let ep = arg_u32(&args, "ep", 1)?;
            api.play(id, ep).await
        }
        "api_comments" => {
            let id = arg_i64(&args, "id")?;
            let ep = arg_u32(&args, "ep", 1)?;
            let page = arg_u32(&args, "page", 1)?;
            let size = arg_u32(&args, "size", 20)?;
            api.comments(id, ep, page, size).await
        }
        "api_comment_send" => {
            let id = arg_i64(&args, "id")?;
            let ep = arg_u32(&args, "ep", 1)?;
            let text = arg_str(&args, "text")?;
            let reply_id = arg_opt_i64(&args, "reply_id", "replyId");
            api.comment_send(id, ep, text, reply_id).await
        }
        "api_like" => {
            let id = arg_i64(&args, "id")?;
            let ep = arg_u32(&args, "ep", 1)?;
            let on = arg_bool(&args, "on")?;
            api.like(id, ep, on).await
        }
        "api_collect" => {
            let id = arg_i64(&args, "id")?;
            let on = arg_bool(&args, "on")?;
            api.collect(id, on).await
        }
        "api_follow" => {
            let uid = arg_i64(&args, "uid")?;
            let on = arg_bool(&args, "on")?;
            api.follow(uid, on).await
        }
        "api_post_like" => {
            let id = arg_i64(&args, "id")?;
            let on = arg_bool(&args, "on")?;
            api.post_like(id, on).await
        }
        "api_post_collect" => {
            let id = arg_i64(&args, "id")?;
            let on = arg_bool(&args, "on")?;
            api.post_collect(id, on).await
        }
        "api_post_comments" => {
            let id = arg_i64(&args, "id")?;
            let page = arg_u32(&args, "page", 1)?;
            let size = arg_u32(&args, "size", 20)?;
            api.post_comments(id, page, size).await
        }
        // 播放诊断日志（前端 playbackDiagnostics.ts 用；与桌面端同名命令一致）。
        "playback_log_status" => playback_log::playback_log_status()
            .and_then(|s| serde_json::to_value(s).map_err(|e| e.to_string())),
        "playback_log_set" => {
            let enabled = arg_bool(&args, "enabled")?;
            playback_log::playback_log_set(enabled)
                .and_then(|s| serde_json::to_value(s).map_err(|e| e.to_string()))
        }
        "playback_log_write" => {
            let entries = args
                .get("entries")
                .cloned()
                .and_then(|v| serde_json::from_value::<Vec<playback_log::Entry>>(v).ok())
                .ok_or_else(|| "缺少参数 entries".to_string())?;
            playback_log::playback_log_write(entries).map(|_| Value::Null)
        }
        "playback_log_read" => playback_log::playback_log_read().map(Value::String),
        _ => Err(format!("未知命令 {cmd}")),
        }
    }
    .await;
    match result {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, Json(json!({ "error": e }))).into_response(),
    }
}

/* ───────────────────────── /media 代理 ───────────────────────── */

/// 解析 `Range: bytes=start-end` / `bytes=start-` / `bytes=-suffix`。
fn parse_range(header: &str, len: u64) -> Option<(u64, u64)> {
    let h = header.trim().strip_prefix("bytes=")?;
    let (start_s, end_s) = h.split_once('-')?;
    if start_s.is_empty() {
        let suffix: u64 = end_s.parse().ok()?;
        if suffix == 0 || len == 0 {
            return None;
        }
        let start = len.saturating_sub(suffix);
        Some((start, len - 1))
    } else {
        let start: u64 = start_s.parse().ok()?;
        if start >= len {
            return None;
        }
        let end: u64 = if end_s.is_empty() {
            len - 1
        } else {
            end_s.parse().ok()?
        };
        if end < start {
            return None;
        }
        Some((start, end.min(len - 1)))
    }
}

async fn media_handler(
    State(state): State<Shared>,
    Path(route): Path<String>,
    RawQuery(raw_query): RawQuery,
    headers: HeaderMap,
) -> Response {
    if !is_authed(&state, &headers) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "未登录" })),
        )
            .into_response();
    }
    if !matches!(route.as_str(), "img" | "hls" | "seg") {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    let query = raw_query.unwrap_or_default();
    let path = format!("/{route}");
    let resp = media::handle(&path, &query, MEDIA_PREFIX).await;
    let status = resp.status();
    let ctype = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();
    let body = resp.into_body();
    let len = body.len() as u64;

    // 播放列表（m3u8）不做 Range；分片/封面支持 Range（视频 seek 用）。
    let is_playlist = ctype.contains("mpegurl");
    if let Some(range) = headers.get(header::RANGE).and_then(|v| v.to_str().ok()) {
        if status.is_success() && !is_playlist {
            match parse_range(range, len) {
                Some((start, end)) => {
                    let slice = body[start as usize..=end as usize].to_vec();
                    let content_range = format!("bytes {start}-{end}/{len}");
                    return (
                        StatusCode::PARTIAL_CONTENT,
                        [
                            (header::CONTENT_TYPE, ctype),
                            (header::CONTENT_RANGE, content_range),
                            (header::CONTENT_LENGTH, slice.len().to_string()),
                            (header::ACCEPT_RANGES, "bytes".to_string()),
                        ],
                        slice,
                    )
                        .into_response();
                }
                None => {
                    return (
                        StatusCode::RANGE_NOT_SATISFIABLE,
                        [
                            (header::CONTENT_RANGE, format!("bytes */{len}")),
                            (header::CONTENT_LENGTH, "0".to_string()),
                        ],
                        Vec::<u8>::new(),
                    )
                        .into_response();
                }
            }
        }
    }
    (
        status,
        [
            (header::CONTENT_TYPE, ctype),
            (header::CONTENT_LENGTH, len.to_string()),
            (header::ACCEPT_RANGES, "bytes".to_string()),
        ],
        body,
    )
        .into_response()
}

/* ───────────────────────── 前端静态文件 ───────────────────────── */

fn content_type_for(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("").to_ascii_lowercase().as_str() {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "map" => "application/json",
        _ => "application/octet-stream",
    }
}

async fn spa_handler(State(state): State<Shared>, uri: Uri, headers: HeaderMap) -> Response {
    if !is_authed(&state, &headers) {
        return Redirect::to("/login").into_response();
    }
    let raw = uri.path().trim_start_matches('/');
    // 只允许常规路径段，拒绝目录穿越。
    let safe = !raw.split('/').any(|seg| seg == ".." || seg.contains('\\'));
    let mut file_path = state.dist_dir.join(raw);
    if !safe || raw.is_empty() || !file_path.is_file() {
        file_path = state.dist_dir.join("index.html");
    }
    // 防止符号链接逃出 dist 目录。
    let dist = &state.dist_dir;
    let inside = file_path
        .canonicalize()
        .map(|p| p.starts_with(dist))
        .unwrap_or(false);
    if !inside {
        file_path = dist.join("index.html");
    }
    match tokio::fs::read(&file_path).await {
        Ok(bytes) => {
            let ctype = content_type_for(file_path.to_string_lossy().as_ref());
            (
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, ctype.to_string()),
                    (header::CONTENT_LENGTH, bytes.len().to_string()),
                    (header::CACHE_CONTROL, "no-cache".to_string()),
                ],
                bytes,
            )
                .into_response()
        }
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "前端尚未构建（缺少 dist/index.html），请先执行 pnpm build",
        )
            .into_response(),
    }
}

fn clean_dist_dir(p: PathBuf) -> PathBuf {
    // 统一用绝对路径，避免工作目录变化后静态文件找不到。
    match p.canonicalize() {
        Ok(abs) => abs,
        Err(_) => p,
    }
}

/* ───────────────────────── 入口 ───────────────────────── */

#[tokio::main]
async fn main() {
    let password = std::env::var("HG_WEB_PASSWORD").unwrap_or_default();
    if password.trim().is_empty() {
        eprintln!("HG_WEB_PASSWORD 未设置：请设置网页访问密码后再启动（不设密码不启动）。");
        std::process::exit(2);
    }
    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8900);
    let data_dir = std::env::var("HG_WEB_DATA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("./data"));
    let dist_dir = std::env::var("HG_WEB_DIST")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("./dist"));
    if let Err(e) = std::fs::create_dir_all(&data_dir) {
        eprintln!("无法创建数据目录 {}: {e}", data_dir.display());
        std::process::exit(2);
    }
    // 与桌面端同一套持久化：线路选择 / 诊断日志 / 上游登录会话。
    net::bind_base(&data_dir);
    playback_log::bind(&data_dir);
    let api = Arc::new(Api::new());
    api.session.bind(data_dir.clone());
    let dist_dir = clean_dist_dir(dist_dir);
    if !dist_dir.join("index.html").is_file() {
        eprintln!(
            "提醒：{} 下没有 index.html，前端页面将返回 503，请先执行 pnpm build。",
            dist_dir.display()
        );
    }

    let state: Shared = Arc::new(AppState {
        api,
        password,
        sessions: Mutex::new(HashMap::new()),
        dist_dir,
    });
    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/login", get(login_page).post(login_submit))
        .route("/api/{cmd}", post(api_dispatch))
        .route("/media/{route}", get(media_handler))
        .fallback(get(spa_handler))
        .with_state(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("无法监听 {addr}: {e}");
            std::process::exit(2);
        }
    };
    eprintln!("hghub-web listening on http://{addr} (data: {})", data_dir.display());
    if let Err(e) = axum::serve(listener, app).await {
        eprintln!("服务异常退出: {e}");
        std::process::exit(1);
    }
}
