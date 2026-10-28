//! 表单收集：字段定义（schema）解析与校验、姓名登录、模板存取、提交。
//!
//! 分工：**模板在客户端编辑**，服务端只负责存、下发、收提交、导出。
//! 所以这里的职责边界是"不相信客户端"：客户端传来的 schema 要验，提交的数据要按
//! 对应版本的 schema 再验一遍，版本号还要能对上（快照冻结，见 `db::form_template_schema`）。

use std::collections::{BTreeMap, HashSet};

use axum::extract::{ConnectInfo, Query, State};
use axum::Form;
use std::net::SocketAddr;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::{db, html, AppState};

pub const FORM_TOKEN_TTL: i64 = 30 * 86400;
pub const EDIT_TOKEN_TTL: i64 = 7 * 86400;
const HEADER_FORM: &str = "x-bks-form";
const HEADER_EDIT: &str = "x-bks-edit";
const MAX_FIELDS: usize = 30;

/* ============================================================ schema */

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct Schema {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub fields: Vec<Field>,
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct Field {
    pub key: String,
    pub label: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub required: bool,
    #[serde(default, rename = "maxLength")]
    pub max_length: Option<usize>,
    #[serde(default)]
    pub min: Option<f64>,
    #[serde(default)]
    pub max: Option<f64>,
    #[serde(default)]
    pub decimals: Option<u32>,
    #[serde(default)]
    pub options: Option<Vec<String>>,
    #[serde(default)]
    pub placeholder: Option<String>,
}

pub const KINDS: [&str; 5] = ["text", "textarea", "number", "select", "date"];

/// 模板能不能存。返回人话错误，直接显示在客户端编辑器里。
pub fn validate_schema(schema: &Schema) -> Result<(), String> {
    if schema.fields.is_empty() {
        return Err("至少要有 1 个字段".into());
    }
    if schema.fields.len() > MAX_FIELDS {
        return Err(format!("字段最多 {MAX_FIELDS} 个"));
    }

    let mut seen = HashSet::new();
    for f in &schema.fields {
        if !valid_key(&f.key) {
            return Err(format!(
                "字段标识「{}」不合法：只能用小写字母、数字、下划线，1~32 位",
                f.key
            ));
        }
        if !seen.insert(f.key.as_str()) {
            return Err(format!("字段标识「{}」重复了", f.key));
        }
        let label = f.label.trim();
        if label.is_empty() {
            return Err(format!("字段「{}」的显示名称不能为空", f.key));
        }
        if label.chars().count() > 30 {
            return Err(format!("字段「{label}」的显示名称太长（最多 30 字）"));
        }
        if !KINDS.contains(&f.kind.as_str()) {
            return Err(format!("字段「{label}」的类型「{}」不支持", f.kind));
        }
        if f.kind == "select" {
            let empty = f
                .options
                .as_ref()
                .map(|o| o.iter().filter(|s| !s.trim().is_empty()).count())
                .unwrap_or(0);
            if empty < 2 {
                return Err(format!("下拉字段「{label}」至少要 2 个选项"));
            }
        }
        if f.kind == "number" {
            if let (Some(min), Some(max)) = (f.min, f.max) {
                if min > max {
                    return Err(format!("数字字段「{label}」的最小值大于最大值"));
                }
            }
        }
    }
    Ok(())
}

fn valid_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 32
        && key
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && !key.starts_with(|c: char| c.is_ascii_digit())
}

/// 按 schema 校验一份提交数据。返回「字段 key → 错误人话」，空表示全部通过。
pub fn validate_data(schema: &Schema, data: &Map<String, Value>) -> BTreeMap<String, String> {
    let mut errs = BTreeMap::new();

    for f in &schema.fields {
        let raw = data.get(&f.key);
        let text = match raw {
            None | Some(Value::Null) => String::new(),
            Some(Value::String(s)) => s.trim().to_string(),
            Some(other) => other.to_string(),
        };

        if text.is_empty() {
            if f.required {
                errs.insert(f.key.clone(), "必填".into());
            }
            continue;
        }

        match f.kind.as_str() {
            "text" | "textarea" => {
                let limit = f.max_length.unwrap_or(if f.kind == "textarea" { 500 } else { 200 });
                if text.chars().count() > limit {
                    errs.insert(f.key.clone(), format!("最多 {limit} 字"));
                }
            }
            "number" => match text.parse::<f64>() {
                Ok(v) => {
                    if let Some(min) = f.min {
                        if v < min {
                            errs.insert(f.key.clone(), format!("不能小于 {}", trim_num(min)));
                        }
                    }
                    if let Some(max) = f.max {
                        if v > max && !errs.contains_key(&f.key) {
                            errs.insert(f.key.clone(), format!("不能大于 {}", trim_num(max)));
                        }
                    }
                    if f.decimals == Some(0) && v.fract() != 0.0 && !errs.contains_key(&f.key) {
                        errs.insert(f.key.clone(), "必须是整数".into());
                    }
                }
                Err(_) => {
                    errs.insert(f.key.clone(), "请填数字".into());
                }
            },
            "select" => {
                let ok = f
                    .options
                    .as_ref()
                    .map(|opts| opts.iter().any(|o| o == &text))
                    .unwrap_or(false);
                if !ok {
                    errs.insert(f.key.clone(), "请从下拉框里选一个".into());
                }
            }
            "date" => {
                if !valid_date(&text) {
                    errs.insert(f.key.clone(), "日期格式应为 2026-10-28".into());
                }
            }
            _ => {}
        }
    }
    errs
}

fn trim_num(v: f64) -> String {
    if v.fract() == 0.0 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

fn valid_date(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != 3 || parts[0].len() != 4 || parts[1].len() != 2 || parts[2].len() != 2 {
        return false;
    }
    if !parts.iter().all(|p| p.chars().all(|c| c.is_ascii_digit())) {
        return false;
    }
    let (y, m, d) = (
        parts[0].parse::<i32>().unwrap_or(0),
        parts[1].parse::<u32>().unwrap_or(0),
        parts[2].parse::<u32>().unwrap_or(0),
    );
    (1970..=2100).contains(&y) && (1..=12).contains(&m) && (1..=31).contains(&d)
}

/* ============================================================ 鉴权小工具 */

fn header_token<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok()).map(str::trim)
}

/// 表单用户：校验 token → 再查库确认这个人还存在且没被停用。
/// （只在 token 里认 user_id 不够：停用一个客服后他的 token 不能还能提交。）
fn current_user(state: &AppState, headers: &HeaderMap) -> Option<db::FormUser> {
    let token = header_token(headers, HEADER_FORM)?;
    let user_id = state.auth.verify_form_token(token)?;
    let conn = state.conn.lock().unwrap();
    let user = db::form_user_find(&conn, user_id).ok().flatten()?;
    if user.enabled { Some(user) } else { None }
}

fn is_editor(state: &AppState, headers: &HeaderMap) -> bool {
    header_token(headers, HEADER_EDIT)
        .map(|t| state.auth.verify_edit_token(t))
        .unwrap_or(false)
}

fn ok(value: Value) -> Response {
    (StatusCode::OK, Json(value)).into_response()
}

fn bad(code: StatusCode, msg: impl Into<String>) -> Response {
    (code, Json(json!({ "error": msg.into() }))).into_response()
}

/* ============================================================ 接口 */

#[derive(Deserialize)]
pub struct LoginReq {
    #[serde(default)]
    name: String,
    /// 客户端机器标识（可选）。概览页按 IP + 它聚合，能区分同一 IP 下的多台机器
    #[serde(default)]
    client_id: String,
}

pub async fn login(
    State(state): State<AppState>,
    headers: HeaderMap,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(req): Json<LoginReq>,
) -> Response {
    let name = req.name.trim();
    if name.is_empty() {
        return bad(StatusCode::BAD_REQUEST, "请填写姓名");
    }
    if name.chars().count() > 20 {
        return bad(StatusCode::BAD_REQUEST, "姓名太长");
    }

    let user = {
        let conn = state.conn.lock().unwrap();
        db::form_user_by_name(&conn, name).ok().flatten()
    };
    let Some(user) = user else {
        // 不在名单里 / 已停用：同一句提示，不告诉对方"有这个人但停用了"
        return bad(StatusCode::FORBIDDEN, "姓名不在名单里，请联系管理员");
    };

    {
        let conn = state.conn.lock().unwrap();
        db::record_activity(
            &conn,
            "login",
            &user.name,
            &crate::peer_ip(&headers, Some(addr)),
            &req.client_id,
            "",
        );
    }

    let (token, exp) = state.auth.issue_form_token(user.id, FORM_TOKEN_TTL);
    ok(json!({
        "user_id": user.id,
        "name": user.name,
        "token": token,
        "expires_at": exp,
    }))
}

#[derive(Deserialize)]
pub struct EditLoginReq {
    #[serde(default)]
    token: String,
}

/// 客户端「模板管理」入口：输一次管理口令，换 7 天的 edit_token。
/// 口令本身（ADMIN_TOKEN）只在服务端比对，客户端不落盘。
pub async fn edit_login(State(state): State<AppState>, Json(req): Json<EditLoginReq>) -> Response {
    let expected = state.cfg.admin_token.trim();
    if expected.is_empty() {
        return bad(StatusCode::FORBIDDEN, "服务端没有配置管理口令，无法编辑模板");
    }
    let given = req.token.trim();
    if given.len() != expected.len() || !constant_time_eq(expected.as_bytes(), given.as_bytes()) {
        return bad(StatusCode::FORBIDDEN, "管理口令不对");
    }
    let (token, exp) = state.auth.issue_edit_token(EDIT_TOKEN_TTL);
    ok(json!({ "edit_token": token, "expires_at": exp }))
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

#[derive(Deserialize)]
pub struct TemplateQuery {
    /// 编辑器要看到停用的模板；填写页只要启用的
    #[serde(default)]
    all: Option<String>,
}

pub async fn templates(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<TemplateQuery>,
) -> Response {
    let want_all = matches!(q.all.as_deref(), Some("1") | Some("true"));
    if want_all && !is_editor(&state, &headers) {
        return bad(StatusCode::FORBIDDEN, "需要先输入管理口令");
    }

    let list = {
        let conn = state.conn.lock().unwrap();
        db::form_templates(&conn, want_all).unwrap_or_default()
    };

    let items: Vec<Value> = list
        .into_iter()
        .filter_map(|t| {
            let schema: Value = serde_json::from_str(&t.schema_json).ok()?;
            Some(json!({
                "id": t.id,
                "name": t.name,
                "version": t.version,
                "is_active": t.is_active,
                "sort_order": t.sort_order,
                "updated_at": t.updated_at,
                "schema": schema,
            }))
        })
        .collect();

    ok(json!({ "templates": items }))
}

#[derive(Deserialize)]
pub struct SaveReq {
    #[serde(default)]
    id: Option<i64>,
    #[serde(default)]
    name: String,
    schema: Schema,
    /// 新建时不传；更新时传客户端看到的版本（乐观锁）
    #[serde(default)]
    base_version: Option<i64>,
    #[serde(default)]
    sort_order: Option<i64>,
}

pub async fn template_save(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<SaveReq>,
) -> Response {
    if !is_editor(&state, &headers) {
        return bad(StatusCode::FORBIDDEN, "需要先输入管理口令");
    }

    let name = req.name.trim().to_string();
    if name.is_empty() {
        return bad(StatusCode::BAD_REQUEST, "模板名称不能为空");
    }
    if name.chars().count() > 20 {
        return bad(StatusCode::BAD_REQUEST, "模板名称最多 20 字");
    }
    if let Err(msg) = validate_schema(&req.schema) {
        return bad(StatusCode::UNPROCESSABLE_ENTITY, msg);
    }

    let schema_json = match serde_json::to_string(&req.schema) {
        Ok(s) => s,
        Err(e) => return bad(StatusCode::BAD_REQUEST, format!("字段定义无法序列化：{e}")),
    };

    let result = {
        let conn = state.conn.lock().unwrap();
        db::form_template_save(
            &conn,
            req.id,
            &name,
            &schema_json,
            req.base_version,
            req.sort_order.unwrap_or(0),
        )
    };

    match result {
        Ok(db::TemplateSave::Saved { id, version }) => ok(json!({ "id": id, "version": version })),
        Ok(db::TemplateSave::Conflict { current }) => bad(
            StatusCode::CONFLICT,
            format!("模板已被别处修改（现在是 v{current}），请刷新后重试"),
        ),
        Ok(db::TemplateSave::NotFound) => bad(StatusCode::NOT_FOUND, "没有这个模板"),
        Ok(db::TemplateSave::NameTaken) => bad(StatusCode::CONFLICT, "已经有同名模板了"),
        Err(e) => bad(StatusCode::INTERNAL_SERVER_ERROR, format!("保存失败：{e}")),
    }
}

#[derive(Deserialize)]
pub struct ToggleReq {
    id: i64,
    is_active: bool,
}

pub async fn template_toggle(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ToggleReq>,
) -> Response {
    if !is_editor(&state, &headers) {
        return bad(StatusCode::FORBIDDEN, "需要先输入管理口令");
    }
    let result = {
        let conn = state.conn.lock().unwrap();
        db::form_template_toggle(&conn, req.id, req.is_active)
    };
    match result {
        Ok(()) => ok(json!({ "ok": true })),
        Err(e) => bad(StatusCode::INTERNAL_SERVER_ERROR, format!("操作失败：{e}")),
    }
}

#[derive(Deserialize)]
pub struct SubmitReq {
    #[serde(default)]
    submission_id: String,
    template_id: i64,
    template_version: i64,
    #[serde(default)]
    data: Map<String, Value>,
    #[serde(default)]
    client_time: Option<i64>,
    #[serde(default)]
    client_id: String,
}

pub async fn submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(req): Json<SubmitReq>,
) -> Response {
    let Some(user) = current_user(&state, &headers) else {
        return bad(StatusCode::FORBIDDEN, "登录已失效，请重新填写姓名");
    };

    if !valid_submission_id(&req.submission_id) {
        return bad(StatusCode::BAD_REQUEST, "submission_id 不合法（8~64 位字母数字或 -）");
    }

    // 取提交时那一版的字段定义 —— 客户端离线几天也不会串列
    let schema_json = {
        let conn = state.conn.lock().unwrap();
        db::form_template_schema(&conn, req.template_id, req.template_version).ok().flatten()
    };
    let Some(schema_json) = schema_json else {
        return bad(
            StatusCode::UNPROCESSABLE_ENTITY,
            "表单版本已失效，请刷新后重填（已填内容仍在草稿里）",
        );
    };
    let Ok(schema) = serde_json::from_str::<Schema>(&schema_json) else {
        return bad(StatusCode::INTERNAL_SERVER_ERROR, "服务端的表单定义损坏，请联系管理员");
    };

    let errs = validate_data(&schema, &req.data);
    if !errs.is_empty() {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({ "error": "校验未通过", "fields": errs })),
        )
            .into_response();
    }

    // 客户端时间只用于报表排序，做一次合理性夹取（未来时间当现在）
    let now = db::now();
    let client_time = req.client_time.unwrap_or(now).clamp(1_500_000_000, now + 300);

    let data_json = Value::Object(req.data.clone()).to_string();
    let client_id: String = req.client_id.chars().take(64).collect();

    let created = {
        let conn = state.conn.lock().unwrap();
        let tpl_name = db::form_template_by_id(&conn, req.template_id)
            .ok()
            .flatten()
            .map(|t| t.name)
            .unwrap_or_default();
        db::record_activity(
            &conn,
            "submit",
            &user.name,
            &crate::peer_ip(&headers, Some(addr)),
            &req.client_id,
            &tpl_name,
        );
        db::form_submit_insert(
            &conn,
            &db::NewSubmission {
                submission_id: &req.submission_id,
                template_id: req.template_id,
                template_version: req.template_version,
                template_snapshot: &schema_json,
                user_id: user.id,
                user_name: &user.name,
                data_json: &data_json,
                client_time,
                client_id: &client_id,
            },
        )
    };

    match created {
        // duplicate 也回 200：客户端只有看到成功才会清队列，网络抖动重试才不会卡住
        Ok(true) => ok(json!({ "status": "created" })),
        Ok(false) => ok(json!({ "status": "duplicate" })),
        Err(e) => bad(StatusCode::INTERNAL_SERVER_ERROR, format!("保存失败：{e}")),
    }
}

fn valid_submission_id(s: &str) -> bool {
    (8..=64).contains(&s.len())
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

#[derive(Deserialize)]
pub struct MineQuery {
    #[serde(default)]
    limit: Option<i64>,
}

/// 客户端「我提交过的」：确认真的交上去了（离线补交后尤其需要）。
pub async fn mine(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<MineQuery>,
) -> Response {
    let Some(user) = current_user(&state, &headers) else {
        return bad(StatusCode::FORBIDDEN, "登录已失效，请重新填写姓名");
    };
    let filter = db::SubmissionFilter {
        user_id: Some(user.id),
        limit: q.limit.unwrap_or(20).clamp(1, 100),
        ..Default::default()
    };
    let rows = {
        let conn = state.conn.lock().unwrap();
        db::form_submissions(&conn, &filter).unwrap_or_default()
    };
    let items: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            json!({
                "submission_id": r.submission_id,
                "template_id": r.template_id,
                "template_version": r.template_version,
                "client_time": r.client_time,
                "created_at": r.created_at,
            })
        })
        .collect();
    ok(json!({ "items": items }))
}

/* ==================================================== 管理页（沿用 admin Cookie） */

/// `/admin/forms` 的查询与筛选参数。ok/err 是闪信，跟分发页共用一套。
#[derive(Deserialize)]
pub struct AdminQuery {
    #[serde(default)]
    pub template: Option<i64>,
    #[serde(default)]
    pub user: Option<i64>,
    #[serde(default)]
    pub from: Option<String>,
    #[serde(default)]
    pub to: Option<String>,
}

impl AdminQuery {
    /// 把 `YYYY-MM-DD` 解析成 UTC 秒；`to` 取当天 23:59:59，否则筛不进当天的数据。
    fn range(&self) -> (Option<i64>, Option<i64>) {
        (parse_date(self.from.as_deref(), false), parse_date(self.to.as_deref(), true))
    }
}

fn parse_date(s: Option<&str>, end_of_day: bool) -> Option<i64> {
    let s = s?.trim();
    if s.is_empty() {
        return None;
    }
    let (date, _) = s.split_once('T').unwrap_or((s, ""));
    let mut parts = date.split('-');
    let y: i32 = parts.next()?.parse().ok()?;
    let m: u8 = parts.next()?.parse().ok()?;
    let d: u8 = parts.next()?.parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    use time::Month;
    let month = Month::try_from(m).ok()?;
    let date = time::Date::from_calendar_date(y, month, d).ok()?;
    let dt = if end_of_day {
        date.with_hms(23, 59, 59).ok()?
    } else {
        date.with_hms(0, 0, 0).ok()?
    };
    Some(dt.assume_utc().unix_timestamp())
}

/// 三块内容，供 `admin::page` 拼进标签页。鉴权在调用方做过了。
pub fn panes(state: &AppState, q: &AdminQuery) -> (String, String, String) {

    let (from, to) = q.range();
    let filter = db::SubmissionFilter {
        template_id: q.template,
        user_id: q.user,
        from,
        to,
        limit: 50,
        offset: 0,
    };

    let (rows_html, total, tpl_rows, user_rows, tpl_opts, user_opts) = {
        let conn = state.conn.lock().unwrap();

        let rows = db::form_submissions(&conn, &filter).unwrap_or_default();
        let total = db::form_submissions_count(&conn, &filter).unwrap_or(0);

        // 模板名：一次取全，避免 N+1
        let templates = db::form_templates(&conn, true).unwrap_or_default();
        let name_of = |id: i64| -> String {
            templates.iter().find(|t| t.id == id).map(|t| t.name.clone()).unwrap_or_else(|| format!("#{id}"))
        };

        let rows_html: String = rows
            .iter()
            .map(|r| {
                let schema: Option<Schema> = serde_json::from_str(&r.template_snapshot).ok();
                let data: Map<String, Value> = serde_json::from_str::<Value>(&r.data_json)
                    .ok()
                    .and_then(|v| v.as_object().cloned())
                    .unwrap_or_default();
                let pairs: Vec<(String, String)> = match schema {
                    Some(s) => s
                        .fields
                        .iter()
                        .map(|f| {
                            let v = data
                                .get(&f.key)
                                .map(value_text)
                                .unwrap_or_default();
                            (f.label.clone(), v)
                        })
                        .collect(),
                    None => data
                        .iter()
                        .map(|(k, v)| (k.clone(), value_text(v)))
                        .collect(),
                };
                html::form_submission_row(
                    &crate::admin::short_time(r.client_time, state.cfg.tz_offset_min),
                    &crate::admin::short_time(r.created_at, state.cfg.tz_offset_min),
                    &name_of(r.template_id),
                    r.template_version,
                    &r.user_name,
                    &pairs,
                )
            })
            .collect::<Vec<_>>()
            .join("");
        let rows_html = if rows_html.is_empty() {
            r#"<tr><td colspan="4" class="muted">没有符合条件的记录</td></tr>"#.to_string()
        } else {
            rows_html
        };

        let tpl_rows: String = templates
            .iter()
            .map(|t| {
                let count = serde_json::from_str::<Schema>(&t.schema_json)
                    .map(|s| s.fields.len())
                    .unwrap_or(0);
                html::form_template_row(
                    t.id,
                    &t.name,
                    t.version,
                    count,
                    &crate::admin::short_time(t.updated_at, state.cfg.tz_offset_min),
                    t.is_active,
                )
            })
            .collect::<Vec<_>>()
            .join("");
        let tpl_rows = if tpl_rows.is_empty() {
            r#"<tr><td colspan="6" class="muted">还没有模板，去客户端「模板管理」里建一个</td></tr>"#.to_string()
        } else {
            tpl_rows
        };

        let users = db::form_users(&conn).unwrap_or_default();
        let user_rows: String = users
            .iter()
            .map(|u| html::form_user_row(u.id, &u.name, u.enabled))
            .collect::<Vec<_>>()
            .join("");
        let user_rows = if user_rows.is_empty() {
            r#"<tr><td colspan="3" class="muted">还没有人，加了名字客服才能登录</td></tr>"#.to_string()
        } else {
            user_rows
        };

        let tpl_opts = select_options(
            &templates
                .iter()
                .map(|t| (t.id.to_string(), t.name.clone()))
                .collect::<Vec<_>>(),
            q.template.map(|v| v.to_string()).as_deref(),
            "全部模板",
        );
        let user_opts = select_options(
            &users.iter().map(|u| (u.id.to_string(), u.name.clone())).collect::<Vec<_>>(),
            q.user.map(|v| v.to_string()).as_deref(),
            "全部人",
        );

        (rows_html, total, tpl_rows, user_rows, tpl_opts, user_opts)
    };

    // 导出链接带上当前筛选，所见即所得
    let mut export = String::from("/admin/forms/export.xlsx?");
    let mut parts: Vec<String> = Vec::new();
    if let Some(t) = q.template {
        parts.push(format!("template={t}"));
    }
    if let Some(u) = q.user {
        parts.push(format!("user={u}"));
    }
    if let Some(f) = q.from.as_deref().filter(|s| !s.trim().is_empty()) {
        parts.push(format!("from={f}"));
    }
    if let Some(t) = q.to.as_deref().filter(|s| !s.trim().is_empty()) {
        parts.push(format!("to={t}"));
    }
    export.push_str(&parts.join("&"));

    let register = format!(
        r##"<div class="card">
    <h2>提交记录<span class="muted">共 {} 条，最多显示最近 50 条</span></h2>
    <form method="get" action="/admin" class="row" style="margin-bottom:12px">
      <input type="hidden" name="tab" value="register">
      <select name="template">{tpl_opts}</select>
      <select name="user">{user_opts}</select>
      <label>从 <input type="date" name="from" value="{from}"></label>
      <label>到 <input type="date" name="to" value="{to}"></label>
      <button type="submit">筛选</button>
      <a class="primary" href="{export}">导出 xlsx</a>
    </form>
    <table>
      <tr><th>填写时间</th><th>模板</th><th>提交人</th><th>内容</th></tr>
      {rows_html}
    </table>
  </div>"##,
        total,
        from = crate::escape(q.from.as_deref().unwrap_or("")),
        to = crate::escape(q.to.as_deref().unwrap_or("")),
        export = crate::escape(&export),
    );

    let templates = format!(
        r##"<div class="card">
    <h2>模板<span class="muted">在客户端「模板管理」里编辑，这里只读；改坏了可以在这里停用</span></h2>
    <table>
      <tr><th>名称</th><th>版本</th><th>字段</th><th>更新时间</th><th>状态</th><th>操作</th></tr>
      {tpl_rows}
    </table>
  </div>"##
    );

    let users = format!(
        r##"<div class="card">
    <h2>用户（谁能填表）</h2>
    <form method="post" action="/admin/users/save" class="row" style="margin-bottom:12px">
      <input type="text" name="name" placeholder="中文姓名" required>
      <button type="submit">添加</button>
      <span class="muted">必须和客服本人说的完全一致；不在名单里的人登录会失败</span>
    </form>
    <table>
      <tr><th>姓名</th><th>状态</th><th>操作</th></tr>
      {user_rows}
    </table>
  </div>"##
    );

    (register, templates, users)
}

fn select_options(items: &[(String, String)], selected: Option<&str>, all_label: &str) -> String {
    let mut out = format!(
        r#"<option value="">{}</option>"#,
        crate::escape(all_label)
    );
    for (value, label) in items {
        let sel = if selected == Some(value.as_str()) { " selected" } else { "" };
        out.push_str(&format!(
            r#"<option value="{}"{}>{}</option>"#,
            crate::escape(value),
            sel,
            crate::escape(label)
        ));
    }
    out
}

fn value_text(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/* ------------------------------------------------------------ 用户 */

#[derive(Deserialize)]
pub struct UserSaveForm {
    #[serde(default)]
    id: Option<i64>,
    #[serde(default)]
    name: String,
}

pub async fn user_save(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<UserSaveForm>,
) -> Response {
    if !crate::is_admin(&headers, &state) {
        return redirect_err("users", "请先登录");
    }
    let name = form.name.trim().to_string();
    if name.is_empty() {
        return redirect_err("users", "姓名不能为空");
    }
    if name.chars().count() > 20 {
        return redirect_err("users", "姓名最多 20 个字");
    }

    let result = {
        let conn = state.conn.lock().unwrap();
        db::form_user_save(&conn, form.id, &name)
    };
    match result {
        Ok(_) => redirect_ok("users", &format!("已保存「{name}」")),
        Err(e) => {
            if e.to_string().contains("UNIQUE") {
                redirect_err("users", "已经有同名的人了")
            } else {
                redirect_err("users", &format!("保存失败：{e}"))
            }
        }
    }
}

#[derive(Deserialize)]
pub struct UserToggleForm {
    id: i64,
    #[serde(default)]
    enabled: Option<String>,
}

pub async fn user_toggle(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<UserToggleForm>,
) -> Response {
    if !crate::is_admin(&headers, &state) {
        return redirect_err("users", "请先登录");
    }
    let enabled = matches!(form.enabled.as_deref(), Some("1") | Some("true"));
    let result = {
        let conn = state.conn.lock().unwrap();
        db::form_user_toggle(&conn, form.id, enabled)
    };
    match result {
        Ok(()) => redirect_ok("users", if enabled { "已启用" } else { "已停用" }),
        Err(e) => redirect_err("users", &format!("操作失败：{e}")),
    }
}

#[derive(Deserialize)]
pub struct UserDeleteForm {
    id: i64,
}

/// 删人。历史提交**不动** —— `user_name` 是冗余存的，删了人记录还认得出是谁填的。
pub async fn user_delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<UserDeleteForm>,
) -> Response {
    if !crate::is_admin(&headers, &state) {
        return redirect_err("users", "请先登录");
    }
    let result = {
        let conn = state.conn.lock().unwrap();
        db::form_user_delete(&conn, form.id)
    };
    match result {
        Ok(true) => redirect_ok("users", "已删除；他填过的登记保留不变"),
        Ok(false) => redirect_err("users", "没有这个人"),
        Err(e) => redirect_err("users", &format!("删除失败：{e}")),
    }
}

/* ------------------------------------------------------------ 模板（应急开关） */

#[derive(Deserialize)]
pub struct TemplateToggleForm {
    id: i64,
    #[serde(default)]
    is_active: Option<String>,
}

/// 客户端编辑器改坏了模板时，在管理页把它停掉：客户端下次拉取就不再显示。
pub async fn admin_template_toggle(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<TemplateToggleForm>,
) -> Response {
    if !crate::is_admin(&headers, &state) {
        return redirect_err("templates", "请先登录");
    }
    let is_active = matches!(form.is_active.as_deref(), Some("1") | Some("true"));
    let result = {
        let conn = state.conn.lock().unwrap();
        db::form_template_toggle(&conn, form.id, is_active)
    };
    match result {
        Ok(()) => redirect_ok(
            "templates",
            if is_active { "模板已启用" } else { "模板已停用，客户端下次拉取就不再显示" },
        ),
        Err(e) => redirect_err("templates", &format!("操作失败：{e}")),
    }
}

/* ------------------------------------------------------------ 导出 xlsx */

pub async fn export(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<AdminQuery>,
) -> Response {
    if !crate::is_admin(&headers, &state) {
        return redirect_err("register", "请先登录");
    }
    if q.template.is_none() {
        // 跨模板并排导，列会稀疏得没法看 —— 一次只导一个模板
        return redirect_err("register", "导出请先选一个模板（一次只导一个）");
    }
    let (from, to) = q.range();
    let filter = db::SubmissionFilter {
        template_id: q.template,
        user_id: q.user,
        from,
        to,
        limit: 50_000,
        offset: 0,
    };

    let (rows, sheet_name) = {
        let conn = state.conn.lock().unwrap();
        let rows = db::form_submissions(&conn, &filter).unwrap_or_default();
        let name = q
            .template
            .and_then(|id| db::form_template_by_id(&conn, id).ok().flatten())
            .map(|t| t.name)
            .unwrap_or_else(|| "提交记录".to_string());
        (rows, name)
    };

    if rows.is_empty() {
        return redirect_err("register", "这个筛选下没有记录可导出");
    }

    let table = crate::xlsx::build_table(&rows, state.cfg.tz_offset_min);
    let bytes = match crate::xlsx::write_xlsx(&table, &sheet_name) {
        Ok(b) => b,
        Err(e) => return redirect_err("register", &format!("生成表格失败：{e}")),
    };

    let filename = format!(
        "{}-{}.xlsx",
        sheet_name,
        time::OffsetDateTime::now_utc()
            .to_offset(
                time::UtcOffset::from_whole_seconds(state.cfg.tz_offset_min * 60)
                    .unwrap_or(time::UtcOffset::UTC)
            )
            .date()
    );

    let mut response = axum::body::Body::from(bytes).into_response();
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static(
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        ),
    );
    let disposition = format!(
        "attachment; filename=\"{}\"; filename*=UTF-8''{}",
        filename.replace('"', ""),
        url_encode(&filename)
    );
    if let Ok(v) = axum::http::HeaderValue::from_str(&disposition) {
        response.headers_mut().insert(axum::http::header::CONTENT_DISPOSITION, v);
    }
    response
}

fn url_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.as_bytes() {
        match *b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/* ------------------------------------------------------------ 跳转 */

/// 管理页的操作都重定向回带标签的地址：加人回名单、停用模板回模板、
/// 导出出错回登记 —— 不然一操作就跳回概览，还得再点一次。
fn redirect_ok(tab: &str, msg: &str) -> Response {
    redirect_with(tab, "ok", msg)
}

fn redirect_err(tab: &str, msg: &str) -> Response {
    redirect_with(tab, "err", msg)
}

fn redirect_with(tab: &str, kind: &str, msg: &str) -> Response {
    axum::response::Redirect::to(&format!(
        "/admin?tab={tab}&{}={}",
        kind,
        url_encode(msg)
    ))
    .into_response()
}
