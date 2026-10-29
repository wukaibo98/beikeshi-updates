//! 管理页：上传、发布、回滚、删除。

use axum::extract::{Multipart, Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::Form;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::{db, html, is_admin, AppState};

/// 单个包的上限。body 限制要略大于它（见 main.rs 的 DefaultBodyLimit），
/// 否则超大文件会被拦在更外层、连"文件太大"这句提示都出不来。
pub(crate) const MAX_UPLOAD_BYTES: u64 = 300 * 1024 * 1024;

/// 小于这个数就当上传不完整。
/// 安装包不可能只有几十 KB，出现这种情况几乎一定是中途断了 ——
/// 宁可拒绝，也不要把一个残缺的包发布出去。
const MIN_PACKAGE_BYTES: u64 = 64 * 1024;

#[derive(Deserialize)]
pub struct LoginForm {
    token: String,
}

/* ------------------------------------------------------------ 页面 */

/// 五个标签页的统一入口：`概览 / 版本 / 登记 / 模板 / 名单`。
///
/// 所有内容**一次渲染完**，切标签由页面里 20 行 JS 切 `hidden`（不刷新），
/// URL 用 `replaceState` 同步，所以刷新和书签都能回到同一个标签。
/// 标签本身是 `<a href="?tab=">`，没有 JS 时点它照样整页跳转。
#[derive(Deserialize)]
pub struct PageQuery {
    #[serde(default)]
    pub tab: Option<String>,
    #[serde(default)]
    pub template: Option<i64>,
    #[serde(default)]
    pub user: Option<i64>,
    #[serde(default)]
    pub from: Option<String>,
    #[serde(default)]
    pub to: Option<String>,
    #[serde(default)]
    pub ok: Option<String>,
    #[serde(default)]
    pub err: Option<String>,
}

impl PageQuery {
    fn tab(&self) -> &str {
        let t = self.tab.as_deref().unwrap_or("overview").trim();
        if html::TABS.iter().any(|(k, _)| *k == t) {
            t
        } else {
            "overview"
        }
    }
}

pub async fn page(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<PageQuery>,
) -> Response {
    if !is_admin(&headers, &state) {
        return Html(html::LOGIN.replace("%%ERROR%%", "")).into_response();
    }

    let active = q.tab().to_string();
    let tz = state.cfg.tz_offset_min;

    let (overview, versions, counts, subtitle) = {
        let conn = state.conn.lock().unwrap();

        /* ---- 概览：谁在连服务器 ---- */
        let now = db::now();
        let acts = db::recent_activity(&conn, now - 7 * 86400, 50);
        let act_rows = if acts.is_empty() {
            r#"<tr><td colspan="4" class="muted">还没有客户端连过。客户端启动后会检查更新，那时这里就会出现。</td></tr>"#.to_string()
        } else {
            acts.iter().map(|a| html::activity_row(a, now, tz)).collect::<Vec<_>>().join("")
        };

        let stats = db::form_stats(&conn);
        let stat_cards = [
            html::stat("启用模板", stats.templates),
            html::stat("可填人数", stats.users),
            html::stat("登记总数", stats.submissions),
            html::stat("今日登记", stats.submissions_today),
        ]
        .join("");

        let current = db::current(&conn).ok().flatten();
        let (check_count, last_at) = db::checks_info(&conn);
        let checks = if last_at > 0 {
            format!("{check_count} 次 · 最近 {}", short_time(last_at, tz))
        } else {
            format!("{check_count} 次")
        };

        let lan = if state.cfg.lan_base.is_empty() {
            "（未配置）".to_string()
        } else {
            state.cfg.lan_base.clone()
        };
        let public = if state.cfg.public_base.is_empty() {
            "（未配置）".to_string()
        } else {
            state.cfg.public_base.clone()
        };
        let manifest = if state.cfg.lan_base.is_empty() {
            "/latest.json?client=web".to_string()
        } else {
            format!("{}/latest.json?client=web", state.cfg.lan_base)
        };

        let overview = format!(
            r##"<div class="card">
    <h2>谁在连服务器<span class="muted">15 分钟内有动作的标在线；没有心跳，只能记最后一次请求</span></h2>
    <table>
      <tr><th>谁 / 机器</th><th>IP</th><th>最后动作</th><th>时间</th></tr>
      {act_rows}
    </table>
  </div>
  <div class="card">
    <h2>概况</h2>
    <div class="grid">{stat_cards}</div>
    <table style="margin-top:14px">
      <tr><th>当前发布</th><td>{current_version}</td></tr>
      <tr><th>累计检查</th><td>{checks}</td></tr>
      <tr><th>内网地址</th><td><code>{lan}</code></td></tr>
      <tr><th>公网地址</th><td><code>{public}</code></td></tr>
      <tr><th>客户端清单</th><td><code>{manifest}</code></td></tr>
    </table>
  </div>"##,
            current_version = match &current {
                Some(r) => format!("v{} · {}", crate::escape(&r.version), crate::escape(&r.notes)),
                None => r#"<span class="muted">还没有发布任何版本</span>"#.to_string(),
            },
            lan = crate::escape(&lan),
            public = crate::escape(&public),
            manifest = crate::escape(&manifest),
        );

        /* ---- 版本 ---- */
        let releases = db::list_all(&conn).unwrap_or_default();
        let rows: String = releases
            .iter()
            .map(|r| {
                html::row(
                    r.id,
                    &r.version,
                    &r.notes,
                    r.client != db::CLIENT,
                    r.size,
                    r.downloads,
                    &short_time(r.created_at, tz),
                    r.is_current,
                )
            })
            .collect::<Vec<_>>()
            .join("");
        let rows = if rows.is_empty() {
            r#"<tr><td colspan="5" class="muted">暂无版本</td></tr>"#.to_string()
        } else {
            rows
        };
        let versions = format!(
            r##"<div class="card">
    <h2>发布新版本</h2>
    <form method="post" action="/admin/upload" enctype="multipart/form-data">
      <label>版本号（形如 1.2.0）</label>
      <input type="text" name="version" placeholder="1.2.0" style="width:240px" required>
      <label>更新说明（会显示在客户端，一句话）</label>
      <textarea name="notes" placeholder="修复赠品折合"></textarea>
      <label>程序文件（exe，上传后自动算 sha256）</label>
      <input type="file" name="file" accept=".exe" required>
      <div style="margin-top:14px"><button type="submit">上传并发布</button></div>
    </form>
  </div>
  <div class="card">
    <h2>版本列表</h2>
    <table>
      <tr><th>版本</th><th>大小</th><th>下载</th><th>发布时间</th><th>操作</th></tr>
      {rows}
    </table>
  </div>"##
        );

        let counts: [(&str, i64); 5] = [
            ("overview", 0),
            ("versions", releases.len() as i64),
            ("register", stats.submissions),
            ("templates", stats.templates),
            ("users", stats.users),
        ];

        let subtitle = format!(
            "{} · {} 个模板 · {} 人可填 · {} 条登记",
            match &current {
                Some(r) => format!("当前发布 v{}", r.version),
                None => "还没有发布任何版本".to_string(),
            },
            stats.templates,
            stats.users,
            stats.submissions
        );

        (overview, versions, counts, subtitle)
    };

    // 登记 / 模板 / 名单三段在 forms.rs（业务归业务，页面只是拼装）
    let fq = crate::forms::AdminQuery {
        template: q.template,
        user: q.user,
        from: q.from.clone(),
        to: q.to.clone(),
    };
    let (register, templates, users) = crate::forms::panes(&state, &fq);

    let hide = |pane: &str| if pane == active { "" } else { "hidden" };

    let body = html::PAGE
        .replace("%%SUBTITLE%%", &crate::escape(&subtitle))
        .replace("%%FLASH%%", &crate::flash(q.ok.clone(), q.err.clone()))
        .replace("%%TABS%%", &crate::html::tabs(&active, &counts))
        .replace("%%HIDE_OVERVIEW%%", hide("overview"))
        .replace("%%HIDE_VERSIONS%%", hide("versions"))
        .replace("%%HIDE_REGISTER%%", hide("register"))
        .replace("%%HIDE_TEMPLATES%%", hide("templates"))
        .replace("%%HIDE_USERS%%", hide("users"))
        .replace("%%PANE_OVERVIEW%%", &overview)
        .replace("%%PANE_VERSIONS%%", &versions)
        .replace("%%PANE_REGISTER%%", &register)
        .replace("%%PANE_TEMPLATES%%", &templates)
        .replace("%%PANE_USERS%%", &users);

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
            "file" => match save_upload(&state, field, &version).await {
                Ok(info) => saved = Some(info),
                Err(msg) => {
                    error = Some(msg);
                    break;
                }
            },
            _ => {
                // 老表单还会带 client 字段，直接丢掉：现在只有一条版本线
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
        db::insert_release(&conn, &version, &filename, size, &sha, &notes, &package)
    };

    if let Err(e) = result {
        let _ = std::fs::remove_file(state.cfg.pkg_dir.join(&filename));
        return redirect_err(&format!("写数据库失败：{e}"));
    }

    let kind = if package == "installer" { "安装包" } else { "程序" };
    redirect_ok(&format!(
        "已发布 v{}（{:.2} MB，{}）",
        version,
        size as f64 / 1024.0 / 1024.0,
        kind
    ))
}

/// 流式落盘 + 边写边算 sha256。文件名按版本生成，不用原始文件名
/// （避免中文、空格和奇怪字符带来的麻烦）。
async fn save_upload(
    state: &AppState,
    mut field: axum::extract::multipart::Field<'_>,
    version: &str,
) -> Result<(String, i64, String), String> {
    if version.is_empty() || !valid_version(version) {
        return Err("版本号格式不对，应形如 1.2.0".into());
    }

    let filename = format!("bkstools-{version}.exe");
    let temp = state.cfg.pkg_dir.join(format!(".upload-{}.tmp", db::now()));
    let mut file = tokio::fs::File::create(&temp)
        .await
        .map_err(|e| format!("建不了临时文件：{e}"))?;

    let mut hasher = Sha256::new();
    let mut size: u64 = 0;

    loop {
        let chunk = match field.chunk().await {
            Ok(Some(c)) => c,
            Ok(None) => break,
            Err(e) =>
            {
                // 中断了（网络断了、请求体被拦在更外层）：半个文件必须删掉，
                // 否则 packages/ 里会一直堆着 .upload-*.tmp
                let _ = tokio::fs::remove_file(&temp).await;
                return Err(format!("接收数据失败：{e}"));
            }
        };

        size += chunk.len() as u64;
        if size > MAX_UPLOAD_BYTES {
            let _ = tokio::fs::remove_file(&temp).await;
            return Err(format!("文件超过 {}MB", MAX_UPLOAD_BYTES / 1024 / 1024));
        }
        hasher.update(&chunk);
        if let Err(e) = tokio::io::AsyncWriteExt::write_all(&mut file, &chunk).await
        {
            let _ = tokio::fs::remove_file(&temp).await;
            return Err(format!("写文件失败：{e}"));
        }
    }

    if let Err(e) = tokio::io::AsyncWriteExt::flush(&mut file).await
    {
        let _ = tokio::fs::remove_file(&temp).await;
        return Err(format!("写文件失败：{e}"));
    }
    drop(file);

    if size < MIN_PACKAGE_BYTES {
        let _ = tokio::fs::remove_file(&temp).await;
        return Err(if size == 0 {
            "文件是空的".to_string()
        } else {
            format!("只有 {size} 字节，上传可能中途断了，请重传")
        });
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
        Ok(db::PublishResult::Published(r)) => redirect_ok(&format!("已切回 v{}", r.version)),
        Ok(db::PublishResult::Legacy(r)) => redirect_err(&format!(
            "v{} 是已停发的旧版客户端记录，不能切回",
            r.version
        )),
        Ok(db::PublishResult::NotFound) => redirect_err("没有这个版本"),
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

/// 页面上显示的时间。库里存的是 Unix 秒（UTC），这里按 `tz_offset_min` 换成当地时间 ——
/// scratch 镜像里没有 tzdata，`OffsetDateTime` 的"本地时间"永远是 UTC，
/// 直接显示会差 8 小时（"刚发布的版本"看着像早上 7 点发的）。
pub(crate) fn short_time(ts: i64, tz_offset_min: i32) -> String {
    let offset = time::UtcOffset::from_whole_seconds(tz_offset_min.saturating_mul(60))
        .unwrap_or(time::UtcOffset::UTC);
    match time::OffsetDateTime::from_unix_timestamp(ts) {
        Ok(t) => {
            let t = t.to_offset(offset);
            format!(
                "{:02}-{:02} {:02}:{:02}",
                t.month() as u8,
                t.day(),
                t.hour(),
                t.minute()
            )
        }
        Err(_) => "—".into(),
    }
}
