//! 贝可施客服小工具 · 更新分发服务
//!
//! 一个二进制 + 一个 SQLite 文件就是全部依赖：NAS 上不用装 Python、不用装运行时，
//! 镜像里只有这个 exe（约 5MB）和它写出来的数据库。
//!
//! 三件事：
//!   1. 对外发版本清单 `/latest.json?client=web`（客户端只认这个）
//!   2. 发程序包 `/packages/{filename}`
//!   3. 一个管理页：上传、发布、回滚、删除
//!   4. 表单收集（v0.2 起）：姓名登录、模板存取/下发、提交、导出 xlsx
//!      模板由**客户端**编辑，服务端只负责存、下发、收提交、导出。

mod admin;
mod auth;
mod db;
mod forms;
mod html;
mod xlsx;

#[cfg(test)]
mod tests;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::extract::{ConnectInfo, DefaultBodyLimit, Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{body::Body, Json, Router};
use rusqlite::Connection;
use serde::Deserialize;
use serde_json::json;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use tokio_util::io::ReaderStream;

pub struct Config {
    pub pkg_dir: PathBuf,
    pub admin_token: String,
    pub lan_base: String,
    pub public_base: String,
    pub channel: String,
    /// 管理页显示时间用的时区偏移（分钟）。scratch 镜像里没有 tzdata，
    /// `OffsetDateTime` 拿到的"本地时间"永远是 UTC，所以这里自己给一个偏移。
    pub tz_offset_min: i32,
}

#[derive(Clone)]
pub struct AppState {
    pub conn: Arc<Mutex<Connection>>,
    pub auth: Arc<auth::Auth>,
    pub cfg: Arc<Config>,
}

#[derive(Deserialize)]
struct ClientQuery {
    client: Option<String>,
}

/// 客户端 IP。Lucky 反代后面真实 IP 在 `X-Forwarded-For` 里；
/// 直连时才用 `ConnectInfo`（容器内看到的是反代的地址，记下来没意义）。
pub fn peer_ip(headers: &HeaderMap, addr: Option<SocketAddr>) -> String {
    if let Some(value) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
        if let Some(first) = value.split(',').next() {
            let first = first.trim();
            if !first.is_empty() {
                return first.chars().take(64).collect();
            }
        }
    }
    addr.map(|a| a.ip().to_string()).unwrap_or_default()
}

/* ------------------------------------------------------------ 对外接口 */

async fn health() -> &'static str {
    "ok"
}

/// 旧地址 `/admin/forms` 现在是「登记」标签，重定向过去，别让书签失效。
async fn redirect_forms() -> axum::response::Redirect {
    axum::response::Redirect::to("/admin?tab=register")
}

/// 版本清单。只服务新版客户端（`client=web`）。
///
/// 旧 Python 版已停发：不带 `client` 或带 `python` 一律 404。
/// **不能把新版的清单发给它** —— 旧客户端的更新逻辑里没有 client 校验，
/// 拿到清单就会把 `bkstools-*.exe` 当自己的新版下载下来替换掉自己。
async fn latest(
    State(state): State<AppState>,
    headers: HeaderMap,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Query(query): Query<ClientQuery>,
) -> Response {
    let client = query.client.unwrap_or_default().trim().to_lowercase();

    if client != db::CLIENT {
        let message = if client.is_empty() {
            "旧版客户端已停止发布，请换用新版".to_string()
        } else {
            format!("未知客户端：{client}")
        };
        return (StatusCode::NOT_FOUND, no_store(), Json(json!({ "error": message })))
            .into_response();
    }

    let release = {
        let conn = state.conn.lock().unwrap();
        db::bump_checks(&conn);
        // 谁在连：每次检查更新都记一笔，概览页据此显示"最近活动"
        db::record_activity(
            &conn,
            "check",
            "",
            &peer_ip(&headers, Some(addr)),
            "",
            "",
        );
        db::current(&conn).ok().flatten()
    };

    let Some(release) = release else {
        return (
            StatusCode::NOT_FOUND,
            no_store(),
            Json(json!({ "error": "还没有发布任何版本" })),
        )
            .into_response();
    };

    let path = format!("/packages/{}", release.filename);
    let mut asset = json!({
        "size": release.size,
        "sha256": release.sha256,
    });
    if !state.cfg.lan_base.is_empty() {
        asset["lan_url"] = json!(format!("{}{}", state.cfg.lan_base, path));
    }
    if !state.cfg.public_base.is_empty() {
        asset["public_url"] = json!(format!("{}{}", state.cfg.public_base, path));
    }
    if state.cfg.lan_base.is_empty() && state.cfg.public_base.is_empty() {
        asset["lan_url"] = json!(path);
    }

    let body = json!({
        "channel": state.cfg.channel,
        "client": release.client,
        "version": release.version,
        "published_at": iso(release.created_at),
        "notes": release.notes,
        "package": release.package,
        "windows_x64": asset,
    });

    (no_store(), Json(body)).into_response()
}

/// 程序包下载。文件名只做白名单字符校验，杜绝 `../` 之类跑出目录。
async fn package(State(state): State<AppState>, Path(name): Path<String>) -> Response {
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        return (StatusCode::BAD_REQUEST, "文件名不合法").into_response();
    }

    let file = state.cfg.pkg_dir.join(&name);
    let meta = match tokio::fs::metadata(&file).await {
        Ok(m) => m,
        Err(_) => return (StatusCode::NOT_FOUND, "没有这个文件").into_response(),
    };

    let handle = match tokio::fs::File::open(&file).await {
        Ok(f) => f,
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "读不到文件").into_response(),
    };

    {
        let conn = state.conn.lock().unwrap();
        if let Ok(Some(release)) = find_by_filename(&conn, &name) {
            db::bump_downloads(&conn, release.id);
        }
    }

    let stream = ReaderStream::new(handle);
    let headers = [
        (header::CONTENT_TYPE, "application/octet-stream".to_string()),
        (header::CONTENT_LENGTH, meta.len().to_string()),
        (
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{name}\""),
        ),
    ];

    (headers, Body::from_stream(stream)).into_response()
}

fn find_by_filename(conn: &Connection, name: &str) -> rusqlite::Result<Option<db::Release>> {
    for release in db::list_all(conn)? {
        if release.filename == name {
            return Ok(Some(release));
        }
    }
    Ok(None)
}

/* ---------------------------------------------------------------- 启动 */

/// 能连上自己的监听端口就算健康。用 TCP 连接而不是发 HTTP：
/// 只想知道"进程还在听"，不需要真的走一遍业务。
fn health_probe() -> i32 {
    let bind = env_or("BIND", "0.0.0.0:8080");
    let addr = bind.replacen("0.0.0.0", "127.0.0.1", 1);
    let timeout = std::time::Duration::from_secs(2);
    match std::net::TcpStream::connect_timeout(
        &addr.parse().unwrap_or_else(|_| "127.0.0.1:8080".parse().unwrap()),
        timeout,
    ) {
        Ok(_) => 0,
        Err(_) => 1,
    }
}

fn env_or(key: &str, fallback: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| fallback.to_string())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // scratch 镜像里没有 shell、没有 curl，健康检查只能靠二进制自己
    if std::env::args().any(|a| a == "--health") {
        std::process::exit(health_probe());
    }
    if std::env::args().any(|a| a == "--version") {
        println!("bks-updates {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    let data_dir = PathBuf::from(env_or("DATA_DIR", "/data"));
    let pkg_dir = data_dir.join("packages");
    std::fs::create_dir_all(&pkg_dir)?;
    clean_upload_leftovers(&pkg_dir);

    let conn = db::open(&data_dir.join("updates.db"))?;
    let auth = auth::Auth::load_or_create(&data_dir.join("secret.key"))?;

    let cfg = Config {
        pkg_dir,
        admin_token: std::env::var("ADMIN_TOKEN").unwrap_or_default(),
        lan_base: std::env::var("LAN_BASE")
            .unwrap_or_default()
            .trim_end_matches('/')
            .to_string(),
        public_base: std::env::var("PUBLIC_BASE")
            .unwrap_or_default()
            .trim_end_matches('/')
            .to_string(),
        channel: env_or("CHANNEL", "stable"),
        // 默认 +8 小时：这服务只在中国的 NAS 上跑；要改别的时区就设 TZ_OFFSET
        tz_offset_min: env_or("TZ_OFFSET", "480").parse().unwrap_or(480),
    };

    if cfg.admin_token.is_empty() {
        eprintln!("警告：没有设置 ADMIN_TOKEN，管理页将无法登录（清单和下载照常可用）");
    }

    let bind = env_or("BIND", "0.0.0.0:8080");
    let state = AppState {
        conn: Arc::new(Mutex::new(conn)),
        auth: Arc::new(auth),
        cfg: Arc::new(cfg),
    };

    let app = Router::new()
        .route("/", get(|| async { axum::response::Redirect::to("/admin") }))
        .route("/health", get(health))
        .route("/latest.json", get(latest))
        .route("/packages/{name}", get(package))
        .route("/admin", get(admin::page))
        .route("/admin/login", post(admin::login))
        .route("/admin/logout", post(admin::logout))
        .route("/admin/upload", post(admin::upload))
        .route("/admin/publish/{id}", post(admin::publish))
        .route("/admin/delete/{id}", post(admin::delete))
        .route("/admin/forms", get(redirect_forms))
        .route("/admin/users/delete", post(forms::user_delete))
        .route("/admin/forms/template/toggle", post(forms::admin_template_toggle))
        .route("/admin/forms/template/delete", post(forms::admin_template_delete))
        .route("/admin/forms/template/purge", post(forms::admin_template_purge))
        .route(
            "/admin/forms/template/{id}/edit",
            get(forms::template_edit),
        )
        .route(
            "/admin/forms/template/{id}/records",
            get(forms::template_records),
        )
        .route("/admin/forms/template/save", post(forms::template_save_web))
        .route("/admin/forms/submission/save", post(forms::submission_save))
        .route(
            "/admin/forms/submission/{id}/edit",
            get(forms::submission_edit),
        )
        .route("/admin/forms/export.xlsx", get(forms::export))
        .route("/admin/users/save", post(forms::user_save))
        .route("/admin/users/toggle", post(forms::user_toggle))
        .route("/admin/admins/save", post(forms::admin_user_save))
        .route("/admin/admins/toggle", post(forms::admin_user_toggle))
        .route("/admin/admins/token", post(forms::admin_user_token))
        .route("/admin/admins/delete", post(forms::admin_user_delete))
        .route("/form/login", post(forms::login))
        .route("/form/edit-login", post(forms::edit_login))
        .route("/form/templates", get(forms::templates))
        .route("/form/template/save", post(forms::template_save))
        .route("/form/template/toggle", post(forms::template_toggle))
        .route("/form/submit", post(forms::submit))
        .route("/form/mine", get(forms::mine))
        // 默认是 2MB：64MB 的安装包传到一半就被截断，浏览器看到的是"连接被重置"。
        // 放宽到略大于单包上限，超限的请求会被干净地拒掉（413）而不是悄悄截断。
        .layer(DefaultBodyLimit::max(
            (admin::MAX_UPLOAD_BYTES + 20 * 1024 * 1024) as usize,
        ))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&bind).await?;
    println!("监听 {bind}");
    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await?;
    Ok(())
}

/// 清掉上次上传中断留下的半个文件。
/// 正常路径会自己删，但进程被打断（重启容器、断电）时它们就留在 packages/ 里了。
fn clean_upload_leftovers(pkg_dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(pkg_dir) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(".upload-") && name.ends_with(".tmp")
        {
            let _ = std::fs::remove_file(entry.path());
            println!("清掉半个上传文件 {name}");
        }
    }
}

/* ---------------------------------------------------------------- 杂项 */

fn no_store() -> [(header::HeaderName, &'static str); 1] {
    [(header::CACHE_CONTROL, "no-store")]
}

pub fn iso(ts: i64) -> String {
    OffsetDateTime::from_unix_timestamp(ts)
        .ok()
        .and_then(|t| t.format(&Rfc3339).ok())
        .unwrap_or_default()
}

/// 管理页要用的口令校验。
pub fn is_admin(headers: &HeaderMap, state: &AppState) -> bool {
    admin_id(headers, state).is_some()
}

/// 当前登录的是谁（管理端人员 id）。`Some(0)` = 环境变量口令（超级管理员）。
pub fn admin_id(headers: &HeaderMap, state: &AppState) -> Option<i64> {
    let cookie = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok());
    state.auth.verify(cookie)
}

/// 查询参数里的整数。
///
/// **空串当成"没填"** —— 下拉框里"全部模板"这个选项的值就是空串，
/// 筛选表单每次提交都会带上 `template=&user=`。按 `Option<i64>` 直接解析，
/// serde 会报 `cannot parse integer from empty string`，整页直接 400。
pub fn opt_i64<'de, D>(d: D) -> Result<Option<i64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = Option::<String>::deserialize(d)?;
    Ok(raw.and_then(|s| s.trim().parse::<i64>().ok()))
}

pub fn flash(ok: Option<String>, err: Option<String>) -> String {
    let mut out = String::new();
    if let Some(msg) = ok.filter(|m| !m.is_empty()) {
        out.push_str(&format!(r#"<div class="flash">{}</div>"#, escape(&msg)));
    }
    if let Some(msg) = err.filter(|m| !m.is_empty()) {
        out.push_str(&format!(r#"<div class="err">{}</div>"#, escape(&msg)));
    }
    out
}

pub fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// 判断是安装包还是绿色版（跟旧版一致：扫头尾 4MB 有没有 Inno 标记）。
pub fn detect_package(path: &std::path::Path) -> String {
    use std::io::{Read, Seek, SeekFrom};

    const WINDOW: u64 = 4 * 1024 * 1024;
    let Ok(mut file) = std::fs::File::open(path) else {
        return "portable".into();
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);

    let head_len = len.min(WINDOW) as usize;
    let mut head = vec![0u8; head_len];
    if file.read_exact(&mut head).is_err() {
        return "portable".into();
    }

    let tail_len = len.min(WINDOW) as usize;
    let mut tail = vec![0u8; tail_len];
    let _ = file.seek(SeekFrom::Start(len.saturating_sub(WINDOW)));
    let _ = file.read_exact(&mut tail);

    let needle = b"Inno Setup";
    let found = contains(&head, needle) || contains(&tail, needle);

    if found { "installer".into() } else { "portable".into() }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}
