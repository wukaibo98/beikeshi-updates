//! 管理页：上传、发布、回滚、删除。

use axum::extract::{Multipart, Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::Form;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::{db, flash, html, is_admin, AppState, Flash};

const MAX_UPLOAD_BYTES: u64 = 300 * 1024 * 1024;

#[derive(Deserialize)]
pub struct LoginForm {
    token: String,
}

/* ------------------------------------------------------------ 页面 */

pub async fn page(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(f): Query<Flash>,
) -> Response {
    if !is_admin(&headers, &state) {
        return Html(html::LOGIN.replace("%%ERROR%%", "")).into_response();
    }

    let (rows, subtitle, checks) = {
        let conn = state.conn.lock().unwrap();
        let releases = db::list_all(&conn).unwrap_or_default();
        let py = db::current(&conn, "python").ok().flatten();
        let web = db::current(&conn, "web").ok().flatten();
        let (count, last_at) = db::checks_info(&conn);

        let rows: String = releases
            .iter()
            .map(|r| {
                html::row(
                    r.id,
                    &r.version,
                    &r.notes,
                    &r.client,
                    r.size,
                    r.downloads,
                    &short_time(r.created_at),
                    r.is_current,
                )
            })
            .collect::<Vec<_>>()
            .join("");
        let rows = if rows.is_empty() {
            r#"<tr><td colspan="6" class="muted">暂无版本</td></tr>"#.to_string()
        } else {
            rows
        };

        let mut parts = Vec::new();
        if let Some(r) = py {
            parts.push(format!("Python 版 v{}", r.version));
        }
        if let Some(r) = web {
            parts.push(format!("网页版 v{}", r.version));
        }
        let subtitle = if parts.is_empty() {
            "还没有发布任何版本".to_string()
        } else {
            parts.join(" · ")
        };

        let checks = if last_at > 0 {
            format!("{count} 次 · 最近 {}", short_time(last_at))
        } else {
            format!("{count} 次")
        };

        (rows, subtitle, checks)
    };

    let manifest_hint = if state.cfg.lan_base.is_empty() {
        "/latest.json?client=web".to_string()
    } else {
        format!("{}/latest.json?client=web", state.cfg.lan_base)
    };

    let body = html::ADMIN
        .replace("%%SUBTITLE%%", &crate::escape(&subtitle))
        .replace("%%FLASH%%", &flash(f.ok, f.err))
        .replace("%%ROWS%%", &rows)
        .replace("%%CHECKS%%", &crate::escape(&checks))
        .replace(
            "%%LAN%%",
            &crate::escape(if state.cfg.lan_base.is_empty() {
                "（未配置）"
            } else {
                &state.cfg.lan_base
            }),
        )
        .replace(
            "%%PUBLIC%%",
            &crate::escape(if state.cfg.public_base.is_empty() {
                "（未配置）"
            } else {
                &state.cfg.public_base
            }),
        )
        .replace("%%MANIFEST%%", &crate::escape(&manifest_hint));

    Html(body).into_response()
}

/* ------------------------------------------------------------ 登录 */

pub async fn login(
    State(state): State<AppState>,
    Form(form): Form<LoginForm>,
) -> Response {
    let expected = state.cfg.admin_token.trim();
    let given = form.token.trim();

    if expected.is_empty() || !constant_time_eq(expected.as_bytes(), given.as_bytes()) {
        let mut response = Html(html::LOGIN.replace(
            "%%ERROR%%",
            r#"<div class="err">口令不对</div>"#,
        ))
        .into_response();
        *response.status_mut() = StatusCode::UNAUTHORIZED;
        return response;
    }

    let cookie = state.auth.issue_cookie();
    let mut response = Redirect::to("/admin?ok=%E7%99%BB%E5%BD%95%E6%88%90%E5%8A%9F").into_response();
    if let Ok(value) = header::HeaderValue::from_str(&cookie) {
        response.headers_mut().append(header::SET_COOKIE, value);
    }
    response
}

pub async fn logout(State(state): State<AppState>) -> Response {
    let mut response = Redirect::to("/admin").into_response();
    if let Ok(value) = header::HeaderValue::from_str(&state.auth.clear_cookie()) {
        response.headers_mut().append(header::SET_COOKIE, value);
    }
    response
}

/* ------------------------------------------------------------ 上传 */

pub async fn upload(
    State(state): State<AppState>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> Response {
    if !is_admin(&headers, &state) {
        return Redirect::to("/admin").into_response();
    }

    let mut version = String::new();
    let mut notes = String::new();
    let mut client = String::from("python");
    let mut saved: Option<(String, i64, String)> = None;
    let mut error: Option<String> = None;

    loop {
        let field = match multipart.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) => {
                error = Some(format!("读取上传内容失败：{e}"));
                break;
            }
        };

        match field.name().unwrap_or("") {
            "version" => version = field.text().await.unwrap_or_default().trim().to_string(),
            "notes" => notes = field.text().await.unwrap_or_default().trim().to_string(),
            "client" => {
                client = field.text().await.unwrap_or_default().trim().to_lowercase();
                if client.is_empty() {
                    client = "python".into();
                }
            }
            "file" => match save_upload(&state, field, &version, &client).await {
                Ok(info) => saved = Some(info),
                Err(msg) => {
                    error = Some(msg);
                    break;
                }
            },
            _ => {
                let _ = field.bytes().await;
            }
        }
    }

    if let Some(err) = error {
        return redirect_err(&err);
    }
    let Some((filename, size, sha)) = saved else {
        return redirect_err("没有收到文件");
    };

    if version.is_empty() || !valid_version(&version) {
        let _ = std::fs::remove_file(state.cfg.pkg_dir.join(&filename));
        return redirect_err("版本号格式不对，应形如 1.2.0");
    }

    let package = crate::detect_package(&state.cfg.pkg_dir.join(&filename));

    let result = {
        let conn = state.conn.lock().unwrap();
        db::insert_release(
            &conn, &version, &filename, size, &sha, &notes, &package, &client,
        )
    };

    if let Err(e) = result {
        let _ = std::fs::remove_file(state.cfg.pkg_dir.join(&filename));
        return redirect_err(&format!("写数据库失败：{e}"));
    }

    let kind = if package == "installer" { "安装包" } else { "程序" };
    redirect_ok(&format!(
        "已发布 {} v{}（{:.2} MB，{}）",
        db::label(&client),
        version,
        size as f64 / 1024.0 / 1024.0,
        kind
    ))
}

/// 流式落盘 + 边写边算 sha256。文件名按客户端和版本生成，不用原始文件名
/// （避免中文、空格和奇怪字符带来的麻烦）。
async fn save_upload(
    state: &AppState,
    mut field: axum::extract::multipart::Field<'_>,
    version: &str,
    client: &str,
) -> Result<(String, i64, String), String> {
    if version.is_empty() || !valid_version(version) {
        return Err("版本号格式不对，应形如 1.2.0".into());
    }

    let prefix = if client == "python" { "BeikeshiCS" } else { "bkstools" };
    let filename = format!("{prefix}-{version}.exe");
    let temp = state.cfg.pkg_dir.join(format!(".upload-{}.tmp", db::now()));
    let mut file = tokio::fs::File::create(&temp)
        .await
        .map_err(|e| format!("建不了临时文件：{e}"))?;

    let mut hasher = Sha256::new();
    let mut size: u64 = 0;

    while let Some(chunk) = field.chunk().await.map_err(|e| format!("接收数据失败：{e}"))? {
        size += chunk.len() as u64;
        if size > MAX_UPLOAD_BYTES {
            let _ = tokio::fs::remove_file(&temp).await;
            return Err("文件超过 300MB".into());
        }
        hasher.update(&chunk);
        tokio::io::AsyncWriteExt::write_all(&mut file, &chunk)
            .await
            .map_err(|e| format!("写文件失败：{e}"))?;
    }

    tokio::io::AsyncWriteExt::flush(&mut file)
        .await
        .map_err(|e| format!("写文件失败：{e}"))?;
    drop(file);

    if size == 0 {
        let _ = tokio::fs::remove_file(&temp).await;
        return Err("文件是空的".into());
    }

    let target = state.cfg.pkg_dir.join(&filename);
    tokio::fs::rename(&temp, &target)
        .await
        .map_err(|e| format!("保存文件失败：{e}"))?;

    Ok((filename, size as i64, hex::encode(hasher.finalize())))
}

/* ------------------------------------------------------ 发布 / 删除 */

pub async fn publish(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Response {
    if !is_admin(&headers, &state) {
        return Redirect::to("/admin").into_response();
    }

    let result = {
        let conn = state.conn.lock().unwrap();
        db::publish(&conn, id)
    };

    match result {
        Ok(Some(r)) => redirect_ok(&format!("已切回 {} v{}", db::label(&r.client), r.version)),
        Ok(None) => redirect_err("没有这个版本"),
        Err(e) => redirect_err(&format!("操作失败：{e}")),
    }
}

pub async fn delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Response {
    if !is_admin(&headers, &state) {
        return Redirect::to("/admin").into_response();
    }

    let result = {
        let conn = state.conn.lock().unwrap();
        db::delete(&conn, id)
    };

    match result {
        Ok(Some(r)) => {
            let _ = tokio::fs::remove_file(state.cfg.pkg_dir.join(&r.filename)).await;
            redirect_ok("已删除")
        }
        Ok(None) => redirect_err("没有这个版本"),
        Err(e) => redirect_err(&format!("操作失败：{e}")),
    }
}

/* ------------------------------------------------------------ 工具 */

fn valid_version(version: &str) -> bool {
    version.split('.').count() >= 2
        && version
            .split('.')
            .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// 跳转时把中文百分号编码：Location 头是 ASCII，直接塞中文浏览器会乱码。
fn redirect_ok(msg: &str) -> Response {
    Redirect::to(&format!("/admin?ok={}", encode(msg))).into_response()
}

fn redirect_err(msg: &str) -> Response {
    let mut response =
        Redirect::to(&format!("/admin?err={}", encode(msg))).into_response();
    *response.status_mut() = StatusCode::SEE_OTHER;
    response
}

fn encode(text: &str) -> String {
    let mut out = String::new();
    for b in text.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn short_time(ts: i64) -> String {
    match time::OffsetDateTime::from_unix_timestamp(ts) {
        Ok(t) => format!(
            "{:02}-{:02} {:02}:{:02}",
            t.month() as u8,
            t.day(),
            t.hour(),
            t.minute()
        ),
        Err(_) => "—".into(),
    }
}
