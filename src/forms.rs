//! 表单收集：字段定义（schema）解析与校验、姓名登录、模板存取、提交。
//!
//! 分工：**模板在客户端编辑**，服务端只负责存、下发、收提交、导出。
//! 所以这里的职责边界是"不相信客户端"：客户端传来的 schema 要验，提交的数据要按
//! 对应版本的 schema 再验一遍，版本号还要能对上（快照冻结，见 `db::form_template_schema`）。

use std::collections::{BTreeMap, HashSet};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, Path, Query, State};
use axum::Form;
use std::net::SocketAddr;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::Json;
use argon2::password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
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
    #[serde(default, rename = "groupByCarrier")]
    pub group_by_carrier: bool,
}

/// 支持的字段类型。
///
/// `name` 是「姓名」：客户端填写页会自动填入当前登录人的姓名，
/// 服务端只当它是普通文本校验（默认最多 30 字）。
pub const KINDS: [&str; 7] = ["text", "textarea", "number", "select", "date", "name", "express"];

/// 字段类型的中文名（管理页的模板编辑器下拉框用）。
pub fn kind_label(kind: &str) -> &'static str {
    match kind {
        "text" => "单行文本",
        "textarea" => "多行文本",
        "number" => "数字",
        "select" => "下拉",
        "date" => "日期",
        "name" => "姓名",
        "express" => "物流快递",
        _ => "单行文本",
    }
}

/// 姓名字段默认最多几个字（客户端 form-render.js 里也是 30）。
pub const NAME_MAX: usize = 30;

/// 模板能不能存。返回人话错误，直接显示在客户端编辑器里。
pub fn validate_schema(schema: &Schema) -> Result<(), String> {
    if schema.fields.is_empty() {
        return Err("至少要有 1 个字段".into());
    }
    if schema.fields.len() > MAX_FIELDS {
        return Err(format!("字段最多 {MAX_FIELDS} 个"));
    }

    let mut seen = HashSet::new();
    let mut grouped_carrier_fields = 0usize;
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
        if f.group_by_carrier && f.kind != "express" {
            return Err(format!("只有物流快递字段「{label}」可以按快递公司分工作表"));
        }
        if f.group_by_carrier { grouped_carrier_fields += 1; }
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
    if grouped_carrier_fields > 1 { return Err("一个模板最多只能选择一个按快递分组的字段".into()); }
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
        if f.kind == "express" {
            let (number, carrier) = raw.and_then(Value::as_object).map(|v| (
                v.get("tracking_number").and_then(Value::as_str).unwrap_or("").trim(),
                v.get("carrier").and_then(Value::as_str).unwrap_or("").trim(),
            )).unwrap_or(("", ""));
            if f.required && (number.is_empty() || carrier.is_empty()) {
                errs.insert(f.key.clone(), "请填写物流单号并选择物流公司".into());
            }
            if number.chars().count() > 60 { errs.insert(f.key.clone(), "物流单号最多 60 字".into()); }
            if carrier.chars().count() > 30 { errs.insert(f.key.clone(), "物流公司最多 30 字".into()); }
            continue;
        }
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
            "name" => {
                let limit = f.max_length.unwrap_or(NAME_MAX);
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
    let (user_id, auth_version) = state.auth.verify_form_token(token)?;
    let conn = state.conn.get().unwrap();
    let user = db::form_user_find(&conn, user_id).ok().flatten()?;
    if user.enabled && user.auth_version == auth_version { Some(user) } else { None }
}

const PASSWORD_MAX_CHARS: usize = 32;
const LOGIN_WINDOW: Duration = Duration::from_secs(15 * 60);
const LOGIN_MAX_FAILURES: u32 = 5;
static LOGIN_FAILURES: OnceLock<Mutex<BTreeMap<String, (u32, Instant)>>> = OnceLock::new();

fn login_is_limited(ip: &str) -> bool {
    let now = Instant::now();
    let mut attempts = LOGIN_FAILURES.get_or_init(Default::default).lock().unwrap();
    if let Some((count, since)) = attempts.get(ip) {
        if now.duration_since(*since) < LOGIN_WINDOW && *count >= LOGIN_MAX_FAILURES {
            return true;
        }
    }
    attempts.retain(|_, (_, since)| now.duration_since(*since) < LOGIN_WINDOW);
    false
}

fn note_login_failure(ip: &str) {
    let now = Instant::now();
    let mut attempts = LOGIN_FAILURES.get_or_init(Default::default).lock().unwrap();
    let entry = attempts.entry(ip.to_string()).or_insert((0, now));
    if now.duration_since(entry.1) >= LOGIN_WINDOW { *entry = (0, now); }
    entry.0 = entry.0.saturating_add(1);
}

fn clear_login_failures(ip: &str) {
    if let Some(attempts) = LOGIN_FAILURES.get() {
        if let Ok(mut attempts) = attempts.lock() { attempts.remove(ip); }
    }
}

pub(crate) fn hash_password(password: &str) -> Result<String, String> {
    if password.is_empty() { return Ok(String::new()); }
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default().hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string()).map_err(|e| e.to_string())
}

pub(crate) fn password_matches(password: &str, encoded_hash: &str) -> bool {
    if encoded_hash.is_empty() { return password.is_empty(); }
    let Ok(hash) = PasswordHash::new(encoded_hash) else { return false };
    Argon2::default().verify_password(password.as_bytes(), &hash).is_ok()
}

fn validate_password(password: &str) -> Result<(), &'static str> {
    if password.chars().count() > PASSWORD_MAX_CHARS { return Err("密码最多 32 个字符") }
    if !password.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'@' || b == b'.') { return Err("密码只允许英文字母、数字、@ 和 .") }
    Ok(())
}

fn validate_new_password(password: &str, confirmation: &str) -> Result<(), &'static str> {
    validate_password(password)?;
    if password != confirmation { return Err("两次输入的密码不一致") }
    Ok(())
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
    #[serde(default)]
    password: String,
    /// 客户端机器标识（可选）。概览页按 IP + 它聚合，能区分同一 IP 下的多台机器
    #[serde(default)]
    client_id: String,
}

#[derive(Deserialize)]
pub struct HeartbeatReq { #[serde(default)] client_id: String }

pub async fn heartbeat(State(state): State<AppState>, headers: HeaderMap, ConnectInfo(addr): ConnectInfo<SocketAddr>, Json(req): Json<HeartbeatReq>) -> Response {
    let Some(user) = current_user(&state, &headers) else { return bad(StatusCode::FORBIDDEN, "登录已失效，请重新登录") };
    let client_id = req.client_id.trim();
    if client_id.is_empty() || client_id.len() > 64 { return bad(StatusCode::BAD_REQUEST, "设备标识无效") }
    let conn = state.conn.get().unwrap();
    db::touch_form_session(&conn, user.id, client_id, &crate::peer_ip(&headers, Some(addr)));
    ok(json!({"ok":true,"server_time":db::now()}))
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
    // 登录只比对哈希，**不校验字符集**：老账号可能设过中文密码或超过 32 位，
    // 按新规则去卡它，会让人明明密码对却登不进去（还提示"只允许字母数字"）。
    // 这里只挡一下超长的，免得白烧 argon2 的 CPU。
    if req.password.len() > 512 {
        return bad(StatusCode::BAD_REQUEST, "密码过长");
    }
    let ip = crate::peer_ip(&headers, Some(addr));
    if login_is_limited(&ip) {
        return bad(StatusCode::TOO_MANY_REQUESTS, "登录尝试过多，请 15 分钟后再试");
    }

    let user = {
        let conn = state.conn.get().unwrap();
        db::form_user_login_by_name(&conn, name).ok().flatten()
    };
    let Some(user) = user else {
        note_login_failure(&ip);
        return bad(StatusCode::UNAUTHORIZED, "账号或密码错误");
    };
    let password = req.password.clone();
    let encoded_hash = user.password_hash.clone();
    let valid = match tokio::task::spawn_blocking(move || password_matches(&password, &encoded_hash)).await {
        Ok(valid) => valid,
        Err(_) => return bad(StatusCode::INTERNAL_SERVER_ERROR, "登录暂时不可用，请稍后重试"),
    };
    if !valid {
        note_login_failure(&ip);
        return bad(StatusCode::UNAUTHORIZED, "账号或密码错误");
    }
    clear_login_failures(&ip);

    {
        let conn = state.conn.get().unwrap();
        db::record_activity(
            &conn,
            "login",
            &user.name,
            &crate::peer_ip(&headers, Some(addr)),
            &req.client_id,
            "",
        );
    }

    let (token, exp) = state.auth.issue_form_token(user.id, user.auth_version, FORM_TOKEN_TTL);
    ok(json!({
        "user_id": user.id,
        "name": user.name,
        "token": token,
        "expires_at": exp,
    }))
}

#[derive(Deserialize)]
pub struct ChangePasswordReq {
    #[serde(default)]
    current_password: String,
    #[serde(default)]
    new_password: String,
    #[serde(default)]
    new_password2: String,
}

pub async fn change_password(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ChangePasswordReq>,
) -> Response {
    let Some(user) = current_user(&state, &headers) else {
        return bad(StatusCode::FORBIDDEN, "登录已失效，请重新登录");
    };
    if let Err(msg) = validate_new_password(&req.new_password, &req.new_password2) {
        return bad(StatusCode::BAD_REQUEST, msg);
    }
    let login_record = {
        let conn = state.conn.get().unwrap();
        db::form_user_login_by_name(&conn, &user.name).ok().flatten()
    };
    let Some(login_record) = login_record else { return bad(StatusCode::FORBIDDEN, "登录已失效") };
    let old_password = req.current_password.clone();
    let encoded_hash = login_record.password_hash.clone();
    let new_password = req.new_password.clone();
    let result = tokio::task::spawn_blocking(move || {
        if !password_matches(&old_password, &encoded_hash) {
            return Err("当前密码不正确".to_string());
        }
        hash_password(&new_password)
    }).await;
    let hash = match result {
        Ok(Ok(hash)) => hash,
        Ok(Err(msg)) => return bad(StatusCode::UNAUTHORIZED, msg),
        Err(_) => return bad(StatusCode::INTERNAL_SERVER_ERROR, "密码修改失败，请重试"),
    };
    let changed = {
        let conn = state.conn.get().unwrap();
        db::form_user_set_password(&conn, user.id, &hash)
    };
    match changed {
        Ok(true) => {
            let auth_version = login_record.auth_version + 1;
            let (token, exp) = state.auth.issue_form_token(user.id, auth_version, FORM_TOKEN_TTL);
            ok(json!({ "user_id": user.id, "name": user.name, "token": token, "expires_at": exp }))
        }
        Ok(false) => bad(StatusCode::NOT_FOUND, "账号不存在"),
        Err(_) => bad(StatusCode::INTERNAL_SERVER_ERROR, "密码修改失败，请重试"),
    }
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
        let conn = state.conn.get().unwrap();
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
        let conn = state.conn.get().unwrap();
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
        let conn = state.conn.get().unwrap();
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
        let conn = state.conn.get().unwrap();
        // 模板被删了（软删除）就不该再收新登记
        if db::form_template_deleted(&conn, req.template_id).unwrap_or(false) {
            return bad(StatusCode::UNPROCESSABLE_ENTITY, "这个表单已经被删除了，不能再提交");
        }
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
        let conn = state.conn.get().unwrap();
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
                template_name: &tpl_name,
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
        let conn = state.conn.get().unwrap();
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
    #[serde(default, deserialize_with = "crate::opt_i64")]
    pub template: Option<i64>,
    #[serde(default, deserialize_with = "crate::opt_i64")]
    pub user: Option<i64>,
    #[serde(default)]
    pub from: Option<String>,
    #[serde(default)]
    pub to: Option<String>,
    /// 要预览/编辑的那条登记
    #[serde(default, deserialize_with = "crate::opt_i64")]
    pub rec: Option<i64>,
    /// 要编辑字段结构的那个模板（表单预览里的【编辑】按钮）
    #[serde(default, deserialize_with = "crate::opt_i64")]
    pub tpl_edit: Option<i64>,
    /// 加一个字段（没 JS 时「加一个字段」点链接过来）
    #[serde(default)]
    pub add: Option<String>,
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

/// 管理页里那四段内容。鉴权在调用方做过了。
pub struct AdminPanes {
    /// 表单登记 → 表单预览
    pub preview: String,
    /// 表单登记 → 模板管理
    pub templates: String,
    /// 登录管理 → 客户端（姓名白名单）
    pub clients: String,
    /// 登录管理 → 管理端（管理口令）
    pub admins: String,
}

/// 导出链接带上当前筛选，所见即所得。
fn export_url(q: &AdminQuery, template: Option<i64>) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(t) = template {
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
    if parts.is_empty() {
        "/admin/forms/export.xlsx".to_string()
    } else {
        format!("/admin/forms/export.xlsx?{}", parts.join("&"))
    }
}

/// 一条登记的摘要：取前几个字段拼成"标签：值"，太长截断。
/// 「表单预览」网格的列。
///
/// 列从**当前模板的字段定义**来（不是每条登记自带的快照）—— 这样同一模板的
/// 所有行才对得齐，改起来也像在改一张表。老登记里多出来、当前模板已经没有的
/// 字段，按出现次数附在后面并标「旧」：**只读**，改回去会跟别的行对不上。
pub fn grid_columns(
    cur: Option<&Schema>,
    parsed: &[(Option<Schema>, Map<String, Value>)],
) -> Vec<html::GridCol> {
    let mut cols: Vec<html::GridCol> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    if let Some(s) = cur {
        for f in &s.fields {
            seen.insert(f.key.clone());
            cols.push(html::GridCol {
                key: f.key.clone(),
                label: f.label.clone(),
                kind: f.kind.clone(),
                options: f.options.clone().unwrap_or_default(),
                legacy: false,
            });
        }
    }
    let mut extra: BTreeMap<String, usize> = BTreeMap::new();
    for (_, data) in parsed {
        for k in data.keys() {
            if !seen.contains(k) {
                *extra.entry(k.clone()).or_insert(0) += 1;
            }
        }
    }
    let mut extra: Vec<(String, usize)> = extra.into_iter().collect();
    extra.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    for (k, _) in extra {
        cols.push(html::GridCol {
            key: k.clone(),
            label: k,
            kind: "text".to_string(),
            options: Vec::new(),
            legacy: true,
        });
    }
    cols
}

/// 四段内容，供 `admin::page` 拼进标签页。
pub fn panes(state: &AppState, q: &AdminQuery) -> AdminPanes {
    let (from, to) = q.range();
    let filter = db::SubmissionFilter {
        template_id: q.template,
        user_id: q.user,
        from,
        to,
        limit: 200,
        offset: 0,
    };

    let (
        total,
        tpl_rows,
        user_rows,
        tpl_opts,
        user_opts,
        preview_groups,
        editor_html,
        tpl_editor_html,
        deleted_rows,
    ) = {
        let conn = state.conn.get().unwrap();

        let rows = db::form_submissions(&conn, &filter).unwrap_or_default();
        let total = db::form_submissions_count(&conn, &filter).unwrap_or(0);

        // 模板名：一次取全，避免 N+1。**含已删除的** —— 模板删了之后，
        // 历史登记上要还显示它原来的名字，而不是一个光秃秃的 #3。
        // 模板被「永久删除」之后表里就查不到了，这时用登记里冗余存的那份名字。
        let templates_all = db::form_templates_all(&conn).unwrap_or_default();
        let name_of_sub = |r: &db::FormSubmission| -> String {
            templates_all
                .iter()
                .find(|t| t.id == r.template_id)
                .map(|t| t.name.clone())
                .unwrap_or_else(|| {
                    let n = r.template_name.trim();
                    if n.is_empty() {
                        format!("已删除的模板 #{}", r.template_id)
                    } else {
                        n.to_string()
                    }
                })
        };

        /* ---- 表单预览：每个模板一张摘要卡 + 三个按钮 ----
           编辑（改表格结构）/ 上传记录（新窗口）/ 导出（xlsx）。
           列出**所有**没删掉的模板 —— 一条都没填过的也能编辑、导出；
           另外，已经删掉但当前筛选下还有登记的，也要列出来（不然那些记录
           在页面上就再也找不到了）。

           登记数据网格在「上传记录」页面中显示和编辑。 */
        let templates = db::form_templates(&conn, true).unwrap_or_default();
        let shown: Vec<&db::FormTemplate> = match q.template {
            Some(tid) => templates.iter().filter(|t| t.id == tid).collect(),
            None => templates.iter().collect(),
        };
        // (模板 id, 显示的名字, 是不是已删除的)
        let mut group_list: Vec<(i64, String, bool)> = shown
            .iter()
            .map(|t| (t.id, t.name.clone(), false))
            .collect();
        for t in templates_all.iter().filter(|t| t.deleted_at > 0) {
            let wanted =
                q.template == Some(t.id) || rows.iter().any(|r| r.template_id == t.id);
            if wanted {
                group_list.push((t.id, t.name.clone(), true));
            }
        }
        let preview_groups: String = group_list
            .iter()
            .map(|(tid, tpl_name, deleted)| {
                let subs: Vec<&db::FormSubmission> =
                    rows.iter().filter(|r| r.template_id == *tid).collect();
                // 已删除的模板不能编辑（行都软删了），只留「上传记录」和「导出」
                let edit_btn = if *deleted {
                    String::new()
                } else {
                    format!(
                        r#"<a class="ghost tpl-open" href="/admin?tab=register&amp;sub=preview&amp;tpl_edit={tid}" data-tpl="{tid}" data-title="编辑模板：{name}">编辑</a>"#,
                        tid = tid,
                        name = crate::escape(tpl_name),
                    )
                };
                let tag = if *deleted {
                    r#"<span class="tag off">已删除</span>"#
                } else {
                    ""
                };
                let hint = if subs.len() >= 200 {
                    r#"<span class="muted" style="margin-left:8px">仅显示最近 200 条</span>"#
                } else {
                    ""
                };
                format!(
                    r#"<div class="card gridcard">
  <h2>{name}{tag}<span class="muted">{n} 条</span>{hint}
    <span class="row" style="float:right">
      {edit_btn}
      <a class="ghost" href="/admin/forms/template/{tid}/records" target="_blank" rel="noopener">上传记录</a>
      <a class="ghost" href="/admin/forms/export.xlsx?template={tid}" title="导出这个模板的全部登记">导出</a>
    </span></h2>
  <p class="muted">填写记录已移到「上传记录」页面查看和编辑。</p>
</div>"#,
                    name = crate::escape(tpl_name),
                    tag = tag,
                    n = subs.len(),
                    hint = hint,
                    tid = tid,
                    edit_btn = edit_btn,
                )
            })
            .collect::<Vec<_>>()
            .join("");

        /* ---- 模板管理：能停用也能删（删是软删，历史登记不受影响）---- */
        let templates = db::form_templates(&conn, true).unwrap_or_default();
        let sub_counts = db::form_submissions_count_by_template(&conn).unwrap_or_default();
        let tpl_rows: String = templates
            .iter()
            .map(|t| {
                let count = serde_json::from_str::<Schema>(&t.schema_json)
                    .map(|s| s.fields.len())
                    .unwrap_or(0);
                let subs = sub_counts
                    .iter()
                    .find(|(tid, _)| *tid == t.id)
                    .map(|(_, n)| *n)
                    .unwrap_or(0);
                html::form_template_row(
                    t.id,
                    &t.name,
                    t.version,
                    count,
                    &crate::admin::short_time(t.updated_at, state.cfg.tz_offset_min),
                    t.is_active,
                    subs,
                )
            })
            .collect::<Vec<_>>()
            .join("");
        let tpl_rows = if tpl_rows.is_empty() {
            r#"<tr><td colspan="6" class="muted">还没有模板，去客户端「模板管理」里建一个</td></tr>"#.to_string()
        } else {
            tpl_rows
        };

        /* ---- 已删除的模板：单独列出来，让人看得到历史还在 ---- */
        let deleted_rows: String = templates_all
            .iter()
            .filter(|t| t.deleted_at > 0)
            .map(|t| {
                let subs = sub_counts
                    .iter()
                    .find(|(tid, _)| *tid == t.id)
                    .map(|(_, n)| *n)
                    .unwrap_or(0);
                html::deleted_template_row(
                    t.id,
                    if t.deleted_name.is_empty() { &t.name } else { &t.deleted_name },
                    &crate::admin::short_time(t.deleted_at, state.cfg.tz_offset_min),
                    subs,
                )
            })
            .collect::<Vec<_>>()
            .join("");

        /* ---- 客户端名单 ---- */
        let users = db::form_users(&conn).unwrap_or_default();
        let user_rows: String = users
            .iter()
            .map(|u| html::form_user_row(u.id, &u.name, u.enabled, u.has_password))
            .collect::<Vec<_>>()
            .join("");
        let user_rows = if user_rows.is_empty() {
            r#"<tr><td colspan="3" class="muted">还没有客户端账号。</td></tr>"#.to_string()
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

        /* ---- 要预览的那条：渲染成可编辑的表单 ---- */
        let editor_html = match q.rec {
            Some(id) => db::form_submission_by_id(&conn, id)
                .ok()
                .flatten()
                .map(|r| {
                    let schema: Option<Schema> = serde_json::from_str(&r.template_snapshot).ok();
                    let data: Map<String, Value> = serde_json::from_str::<Value>(&r.data_json)
                        .ok()
                        .and_then(|v| v.as_object().cloned())
                        .unwrap_or_default();
                    let fields: Vec<html::EditorField> = match &schema {
                        Some(s) => s
                            .fields
                            .iter()
                            .map(|f| html::EditorField {
                                key: f.key.clone(),
                                label: f.label.clone(),
                                kind: f.kind.clone(),
                                value: data.get(&f.key).map(value_text).unwrap_or_default(),
                                options: f.options.clone().unwrap_or_default(),
                            })
                            .collect(),
                        None => Vec::new(),
                    };
                    html::submission_editor(
                        r.id,
                        &name_of_sub(&r),
                        &r.user_name,
                        &crate::admin::short_time(r.client_time, state.cfg.tz_offset_min),
                        &fields,
                    )
                })
                .unwrap_or_default(),
            None => String::new(),
        };

        /* ---- 要编辑字段结构的那个模板：渲染成编辑器 ----
           没 JS 时这条链接是普通跳转，编辑器就内联在这儿（跟登记预览一个套路）。 */
        let tpl_editor_html = match q.tpl_edit {
            Some(id) => template_editor_html(&conn, id, q.add.is_some()),
            None => String::new(),
        };

        (
            total,
            tpl_rows,
            user_rows,
            tpl_opts,
            user_opts,
            preview_groups,
            editor_html,
            tpl_editor_html,
            deleted_rows,
        )
    };

    /* ------------------------------------------------------------ 表单预览 */

    // 工具条上的导出按钮：没选模板时不给链接（点下去只会跳回来报错），
    // 下面每个模板分组里各有一个「导出这个模板」，那个永远能用。
    let export_btn = if q.template.is_some() {
        format!(
            r#"<a class="primary" href="{url}" title="按当前筛选（模板/人/日期）导出">导出当前筛选</a>"#,
            url = crate::escape(&export_url(q, q.template))
        )
    } else {
        r#"<span class="muted">导出：在下面选一个模板，或直接点模板右上角的【导出】</span>"#.to_string()
    };

    let preview = format!(
        r##"<div class="card">
    <h2>表单预览<span class="muted">共 {total} 条 · 格子可以直接改，改完自动存；记录列表在【上传记录】里看</span></h2>
    <form method="get" action="/admin" class="row" style="margin-bottom:12px">
      <input type="hidden" name="tab" value="register">
      <input type="hidden" name="sub" value="preview">
      <select name="template">{tpl_opts}</select>
      <select name="user">{user_opts}</select>
      <label>从 <input type="date" name="from" value="{from}"></label>
      <label>到 <input type="date" name="to" value="{to}"></label>
      <button type="submit">筛选</button>
      {export_btn}
    </form>
    <div id="rec-editor-host">{editor}</div>
    <div id="tpl-editor-host">{tpl_editor}</div>
    {groups}
  </div>"##,
        total = total,
        from = crate::escape(q.from.as_deref().unwrap_or("")),
        to = crate::escape(q.to.as_deref().unwrap_or("")),
        editor = if editor_html.is_empty() {
            String::new()
        } else {
            format!(r#"<div id="rec-editor">{editor_html}</div>"#)
        },
        tpl_editor = if tpl_editor_html.is_empty() {
            String::new()
        } else {
            format!(r#"<div id="tpl-editor">{tpl_editor_html}</div>"#)
        },
        groups = if preview_groups.is_empty() {
            r#"<p class="muted">还没有模板。</p>"#.to_string()
        } else {
            preview_groups
        },
    );

    /* ------------------------------------------------------------ 模板管理 */

    let templates = format!(
        r##"<div class="card">
    <h2>模板<span class="muted">字段结构在「表单预览」里点【编辑】就能改；这里可以停用，也可以删除</span></h2>
    <table>
      <tr><th>名称</th><th>版本</th><th>字段</th><th>更新时间</th><th>状态</th><th>操作</th></tr>
      {tpl_rows}
    </table>
    <p class="muted" style="margin-top:12px">
      「停用」只是不让客户端再显示；「删除」是把模板移进下面的「已删除的模板」——
      这一步<b>不动填写记录，还能还原</b>。
      只有那里点「永久删除」才会连模板、字段版本和填写记录一起从库里删掉。
    </p>
  </div>
  <div class="card">
    <h2>已删除的模板<span class="muted">可还原；永久删除会同时删除模板、字段版本和全部填写记录</span></h2>
    {deleted}
    <p class="muted" style="margin-top:10px">
      「还原」会恢复模板名称和原启用状态；名称已被占用时需要先处理重名。
      「永久删除」不可恢复，会同时删除模板、字段版本和该模板的全部填写记录 ——
      所以要先在框里<b>一字不差地打出模板名</b>，服务端核对一致才会真的删。
    </p>
  </div>"##,
        deleted = if deleted_rows.is_empty() {
            r#"<p class="muted">还没有删过模板。</p>"#.to_string()
        } else {
            format!(
                r#"<table><tr><th>名称</th><th>删除时间</th><th>历史</th><th>操作</th></tr>{deleted_rows}</table>"#
            )
        },
    );

    /* ------------------------------------------------------------ 客户端 */

    let clients = format!(
        r##"<div class="card">
    <h2>客户端（谁能填表）</h2>
    <form method="post" action="/admin/users/save" class="row" style="margin-bottom:12px">
      <input type="text" name="name" placeholder="中文姓名" required>
      <input type="password" name="password" placeholder="初始密码（留空=免密登录）" maxlength="32">
      <input type="password" name="password2" placeholder="确认初始密码" maxlength="32">
      <button type="submit">添加</button>
      <span class="muted">姓名是登录账号；密码最多 32 位，仅字母、数字、@、.；留空表示免密登录</span>
    </form>
    <table>
      <tr><th>姓名</th><th>状态</th><th>操作</th></tr>
      {user_rows}
    </table>
  </div>"##
    );

    /* ------------------------------------------------------------ 管理端 */

    let admins = {
        let conn = state.conn.get().unwrap();
        let rows = db::admin_users(&conn).unwrap_or_default();
        let body: String = rows
            .iter()
            .map(|a| {
                html::admin_user_row(
                    a.id,
                    &a.name,
                    a.enabled,
                    &crate::admin::short_time(a.updated_at, state.cfg.tz_offset_min),
                )
            })
            .collect::<Vec<_>>()
            .join("");
        let body = if body.is_empty() {
            String::new()
        } else {
            format!("<tbody>{body}</tbody>")
        };
        let env_row = html::admin_env_row(!state.cfg.admin_token.trim().is_empty());
        format!(
            r##"<div class="card">
    <h2>管理端<span class="muted">谁可以登进这个管理页</span></h2>
    <form method="post" action="/admin/admins/save" class="row" style="margin-bottom:12px">
      <input type="text" name="name" placeholder="姓名" required>
      <input type="password" name="token" placeholder="登录口令" required>
      <input type="password" name="token2" placeholder="再输一次" required>
      <button type="submit">添加</button>
      <span class="muted">口令只存哈希，库里看不到原文；忘了就用环境变量口令进来改</span>
    </form>
    <table>
      <tr><th>姓名</th><th>状态</th><th>更新时间</th><th>操作</th></tr>
      {env_row}
      {body}
    </table>
  </div>"##
        )
    };

    AdminPanes {
        preview,
        templates,
        clients,
        admins,
    }
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
        Value::Object(obj) if obj.contains_key("tracking_number") && obj.contains_key("carrier") => {
            format!("{} | {}", obj.get("tracking_number").and_then(Value::as_str).unwrap_or(""), obj.get("carrier").and_then(Value::as_str).unwrap_or(""))
        }
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
    #[serde(default)]
    password: String,
    #[serde(default)]
    password2: String,
}

pub async fn user_save(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<UserSaveForm>,
) -> Response {
    if !crate::is_admin(&headers, &state) {
        return redirect_err("login", "clients", "请先登录");
    }
    let name = form.name.trim().to_string();
    if name.is_empty() {
        return redirect_err("login", "clients", "姓名不能为空");
    }
    if name.chars().count() > 20 {
        return redirect_err("login", "clients", "姓名最多 20 个字");
    }
    let password_hash = if form.id.is_none() {
        if let Err(msg) = validate_new_password(&form.password, &form.password2) {
            return redirect_err("login", "clients", msg);
        }
        match tokio::task::spawn_blocking({
            let password = form.password.clone();
            move || hash_password(&password)
        }).await {
            Ok(Ok(hash)) => Some(hash),
            _ => return redirect_err("login", "clients", "密码设置失败，请重试"),
        }
    } else {
        None
    };

    let result = {
        let conn = state.conn.get().unwrap();
        db::form_user_save(&conn, form.id, &name, password_hash.as_deref())
    };
    match result {
        Ok(_) => redirect_ok("login", "clients", &format!("已保存「{name}」")),
        Err(e) => {
            if e.to_string().contains("UNIQUE") {
                redirect_err("login", "clients", "已经有同名的人了")
            } else {
                redirect_err("login", "clients", &format!("保存失败：{e}"))
            }
        }
    }
}

#[derive(Deserialize)]
pub struct UserPasswordForm {
    id: i64,
    #[serde(default)]
    password: String,
    #[serde(default)]
    password2: String,
}

pub async fn user_password_reset(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<UserPasswordForm>,
) -> Response {
    if !crate::is_admin(&headers, &state) {
        return redirect_err("login", "clients", "请先登录");
    }
    if let Err(msg) = validate_new_password(&form.password, &form.password2) {
        return redirect_err("login", "clients", msg);
    }
    let password = form.password;
    let hash = match tokio::task::spawn_blocking(move || hash_password(&password)).await {
        Ok(Ok(hash)) => hash,
        _ => return redirect_err("login", "clients", "密码重置失败，请重试"),
    };
    let result = {
        let conn = state.conn.get().unwrap();
        db::form_user_set_password(&conn, form.id, &hash)
    };
    match result {
        Ok(true) => redirect_ok("login", "clients", "密码已重置，旧登录会话已失效"),
        Ok(false) => redirect_err("login", "clients", "没有这个账号"),
        Err(e) => redirect_err("login", "clients", &format!("密码重置失败：{e}")),
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
        return redirect_err("login", "clients", "请先登录");
    }
    let enabled = matches!(form.enabled.as_deref(), Some("1") | Some("true"));
    let result = {
        let conn = state.conn.get().unwrap();
        db::form_user_toggle(&conn, form.id, enabled)
    };
    match result {
        Ok(()) => redirect_ok("login", "clients", if enabled { "已启用" } else { "已停用" }),
        Err(e) => redirect_err("login", "clients", &format!("操作失败：{e}")),
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
        return redirect_err("login", "clients", "请先登录");
    }
    let result = {
        let conn = state.conn.get().unwrap();
        db::form_user_delete(&conn, form.id)
    };
    match result {
        Ok(true) => redirect_ok("login", "clients", "已删除；他填过的登记保留不变"),
        Ok(false) => redirect_err("login", "clients", "没有这个人"),
        Err(e) => redirect_err("login", "clients", &format!("删除失败：{e}")),
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
        return redirect_err("register", "templates", "请先登录");
    }
    let is_active = matches!(form.is_active.as_deref(), Some("1") | Some("true"));
    let result = {
        let conn = state.conn.get().unwrap();
        db::form_template_toggle(&conn, form.id, is_active)
    };
    match result {
        Ok(()) => redirect_ok(
            "register",
            "templates",
            if is_active { "模板已启用" } else { "模板已停用，客户端下次拉取就不再显示" },
        ),
        Err(e) => redirect_err("register", "templates", &format!("操作失败：{e}")),
    }
}

/* -------------------------------------------------- 删模板（软删除） */

#[derive(Deserialize)]
pub struct TemplateDeleteForm {
    id: i64,
}

/// 将模板移入回收站；填写记录暂时保留，直到永久删除。
pub async fn admin_template_delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<TemplateDeleteForm>,
) -> Response {
    if !crate::is_admin(&headers, &state) {
        return redirect_err("register", "templates", "请先登录");
    }
    let result = {
        let conn = state.conn.get().unwrap();
        db::form_template_delete(&conn, form.id)
    };
    match result {
        Ok(true) => redirect_ok("register", "templates", "模板已移入已删除列表；可还原或永久删除"),
        Ok(false) => redirect_err("register", "templates", "没有这个模板（或已经删过了）"),
        Err(e) => redirect_err("register", "templates", &format!("删除失败：{e}")),
    }
}

/// 永久删除的表单：多一个 `confirm_name` —— 必须一字不差打出模板原名。
///
/// 这一道是**服务端**的闸门，不是靠前端 confirm 糊弄：
/// 永久删除会连填写记录一起删，手滑点错就没了，值得让人多打几个字。
#[derive(Deserialize)]
pub struct TemplatePurgeForm {
    id: i64,
    #[serde(default)]
    confirm_name: String,
}

/// 模板的原名：优先 `deleted_name`，老数据没有就从「（已删除）」后缀还原。
pub fn template_original_name(t: &db::FormTemplate) -> String {
    let saved = t.deleted_name.trim();
    if !saved.is_empty() {
        return saved.to_string();
    }
    t.name
        .strip_suffix("（已删除）")
        .unwrap_or(&t.name)
        .to_string()
}

/// 永久删除回收站中的模板、字段版本和全部填写记录。
///
/// **必须带上正确的模板名**（`confirm_name`）才会执行 —— 服务端核对，
/// 光在浏览器里绕开 confirm 没用。
pub async fn admin_template_purge(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<TemplatePurgeForm>,
) -> Response {
    if !crate::is_admin(&headers, &state) {
        return redirect_err("register", "templates", "请先登录");
    }
    let typed = form.confirm_name.trim().to_string();
    let result = {
        let conn = state.conn.get().unwrap();
        match db::form_template_by_id(&conn, form.id).ok().flatten() {
            None => Err("没有这个模板".to_string()),
            Some(t) => {
                let original = template_original_name(&t);
                if typed != original {
                    Err(format!(
                        "永久删除要手打模板名「{original}」确认，输入框里填的不一样，已取消（什么都没删）"
                    ))
                } else {
                    db::form_template_purge(&conn, form.id)
                        .map_err(|e| format!("永久删除失败：{e}"))
                }
            }
        }
    };
    match result {
        Ok(db::PurgeResult::Purged { submissions }) => redirect_ok(
            "register",
            "templates",
            &format!("已永久删除模板及 {submissions} 条填写记录"),
        ),
        Ok(db::PurgeResult::NotFound) => {
            redirect_err("register", "templates", "没有这个模板")
        }
        Ok(db::PurgeResult::NotDeleted) => redirect_err(
            "register",
            "templates",
            "模板不在已删除列表中，不能永久删除",
        ),
        Err(msg) => redirect_err("register", "templates", &msg),
    }
}

pub async fn admin_template_restore(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<TemplateDeleteForm>,
) -> Response {
    if !crate::is_admin(&headers, &state) { return redirect_err("register", "templates", "请先登录"); }
    let result = { let conn = state.conn.get().unwrap(); db::form_template_restore(&conn, form.id) };
    match result {
        Ok(db::RestoreResult::Restored) => redirect_ok("register", "templates", "模板已还原"),
        Ok(db::RestoreResult::NotFound) => redirect_err("register", "templates", "没有这个模板"),
        Ok(db::RestoreResult::NotDeleted) => redirect_err("register", "templates", "这个模板不在已删除列表中"),
        Ok(db::RestoreResult::NameTaken) => redirect_err("register", "templates", "还原失败：原模板名称已被占用，请先处理同名模板"),
        Err(e) => redirect_err("register", "templates", &format!("还原失败：{e}")),
    }
}

/* -------------------------------------------- 模板的网页版编辑器（v0.5 起） */

/// 模板的字段编辑器 —— 改的是**表格结构**（有哪些列、每列什么类型）。
///
/// 既是弹出层的内容，也是没 JS 时内联在页面里的那份，所以两种情况都能用。
fn template_editor_html(conn: &rusqlite::Connection, id: i64, add_blank: bool) -> String {
    let Some(t) = db::form_template_by_id(conn, id).ok().flatten() else {
        return String::new();
    };
    // 删掉的模板不能编辑（行都软删了）
    if t.deleted_at > 0 {
        return String::new();
    }
    let Ok(schema) = serde_json::from_str::<Schema>(&t.schema_json) else {
        return String::new();
    };
    let mut fields: Vec<html::TplField> = schema
        .fields
        .iter()
        .map(|f| html::TplField {
            key: f.key.clone(),
            label: f.label.clone(),
            kind: f.kind.clone(),
            required: f.required,
            options: f.options.clone().unwrap_or_default(),
            group_by_carrier: f.group_by_carrier,
        })
        .collect();
    if add_blank {
        // 没 JS 时点「加一个字段」整页跳过来，多带一行空的
        fields.push(html::TplField {
            key: String::new(),
            label: String::new(),
            kind: "text".to_string(),
            required: false,
            options: Vec::new(),
            group_by_carrier: false,
        });
    }
    html::template_editor(t.id, &t.name, t.version, &fields)
}

/// 取模板的字段编辑器（HTML 片段）。「表单预览」点【编辑】时用。
pub async fn template_edit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Response {
    if !crate::is_admin(&headers, &state) {
        return (StatusCode::FORBIDDEN, "请先登录").into_response();
    }
    let conn = state.conn.get().unwrap();
    let out = template_editor_html(&conn, id, false);
    if out.is_empty() {
        return (StatusCode::NOT_FOUND, "没有这个模板（或已经删掉了）").into_response();
    }
    (StatusCode::OK, out).into_response()
}

/// 「上传记录」：新窗口打开的独立页面，**只显示最近 200 条**。
pub async fn template_records(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Response {
    if !crate::is_admin(&headers, &state) {
        return (StatusCode::FORBIDDEN, "请先登录").into_response();
    }
    let conn = state.conn.get().unwrap();

    // 模板名：删掉了就退回登记里冗余存的那份
    let name = db::form_templates_all(&conn)
        .unwrap_or_default()
        .into_iter()
        .find(|t| t.id == id)
        .map(|t| t.name)
        .unwrap_or_else(|| format!("已删除的模板 #{id}"));

    let filter = db::SubmissionFilter {
        template_id: Some(id),
        user_id: None,
        from: None,
        to: None,
        limit: RECORDS_WINDOW,
        offset: 0,
    };
    let rows = db::form_submissions(&conn, &filter).unwrap_or_default();
    let total = db::form_submissions_count(&conn, &filter).unwrap_or(0);

    let parsed: Vec<(Option<Schema>, Map<String, Value>)> = rows.iter().map(|r| (
        serde_json::from_str(&r.template_snapshot).ok(),
        serde_json::from_str::<Value>(&r.data_json).ok().and_then(|v| v.as_object().cloned()).unwrap_or_default(),
    )).collect();
    let current = db::form_template_by_id(&conn, id).ok().flatten().and_then(|t| serde_json::from_str::<Schema>(&t.schema_json).ok())
        .or_else(|| parsed.first().and_then(|(s, _)| s.clone()));
    let cols = grid_columns(current.as_ref(), &parsed);
    let grid_rows = rows.iter().enumerate().map(|(i, r)| {
        let data = &parsed[i].1;
        let cells = cols.iter().map(|c| html::grid_cell(c, &data.get(&c.key).map(value_text).unwrap_or_default(), !c.legacy)).collect::<String>();
        // 行尾「改」：没开 JS 时格子是纯文本，靠它整页打开登记编辑表单（有 JS 时基本用不到，
        // 因为格子本身就能改）。表头有这一列，行里也必须有，不然就是「写着改、点不到」。
        format!(r#"<tr data-sub="{id}"><td class="gnum">{n}</td><td class="ro">{user}</td><td class="ro">{time}</td>{cells}<td class="gst">已存</td><td class="gact"><a class="ghost" href="/admin?tab=register&amp;sub=preview&amp;rec={id}" title="打开这条登记的完整表单">改</a></td></tr>"#,
            id=r.id, n=i+1, user=crate::escape(&r.user_name), time=crate::escape(&crate::admin::short_time(r.client_time,state.cfg.tz_offset_min)), cells=cells)
    }).collect::<Vec<_>>().join("");
    let body = if rows.is_empty() { r#"<div class="card"><p class="muted">这个模板还没有人填过。</p></div>"#.to_string() } else {
        format!(r#"<div class="card gridcard"><div class="gridwrap"><table class="grid">{}{}</table></div>
<div class="gridfoot"><span class="gsave" data-role="saved" hidden></span><span class="gerr" data-role="err" hidden></span><span class="muted">编辑单元格后自动保存；旧字段只读</span></div></div>"#, html::grid_head(&cols), grid_rows)
    };

    let subtitle = if total > RECORDS_WINDOW {
        format!("共 {total} 条，这个窗口只显示最近 {RECORDS_WINDOW} 条（要看全部请导出 xlsx）；编辑单元格后自动保存")
    } else {
        format!("共 {total} 条")
    };

    let page = html::RECORDS_PAGE
        .replace("%%CSS%%", html::CSS)
        .replace("%%TITLE%%", &crate::escape(&format!("{name} · 上传记录")))
        .replace("%%SUBTITLE%%", &crate::escape(&subtitle))
        .replace("%%BODY%%", &body)
        .replace("%%SCRIPT%%", html::GRID_SCRIPT);
    Html(page).into_response()
}

/// 「上传记录」窗口最多显示多少条。
pub const RECORDS_WINDOW: i64 = 200;

/// 解析网页版模板编辑器提交的表单 → (模板名, 字段列表)。
///
/// 表单里每个字段一小组参数：`f0_key` / `f0_label` / `f0_type` / `f0_required` /
/// `f0_options` / `f0_pos` / `f0_del`。用 `f{行号}_{属性}` 是因为浏览器的表单
/// 不支持同名参数收数组（`serde_urlencoded` 只保留最后一个同名的）。
///
/// 规则：
/// - 勾了「删」的行跳过；标识空着的行也跳过（没 JS 时「加一个字段」留下的空行）
/// - 按「位置」排序，数字小的在前；位置一样按原来的行号
/// - 只有下拉字段才带选项
pub fn parse_template_form(form: &std::collections::HashMap<String, String>) -> (String, Vec<Field>) {
    let name = form
        .get("name")
        .map(|s| s.trim().to_string())
        .unwrap_or_default();

    // 把 f{i}_{属性} 收成「行号 → 属性表」
    let mut raw: BTreeMap<i64, BTreeMap<String, String>> = BTreeMap::new();
    for (k, v) in form {
        let Some(rest) = k.strip_prefix('f') else { continue };
        let Some((idx_s, attr)) = rest.split_once('_') else { continue };
        let Ok(idx) = idx_s.parse::<i64>() else { continue };
        raw.entry(idx).or_default().insert(attr.to_string(), v.clone());
    }

    // 按「位置」排序：数字小的在前；位置一样按原来的行号
    let mut items: Vec<(i64, i64, BTreeMap<String, String>)> = raw
        .into_iter()
        .map(|(idx, attrs)| {
            let pos = attrs
                .get("pos")
                .and_then(|s| s.trim().parse::<i64>().ok())
                .unwrap_or(idx + 1);
            (pos, idx, attrs)
        })
        .collect();
    items.sort_by_key(|(pos, idx, _)| (*pos, *idx));

    let mut fields: Vec<Field> = Vec::new();
    for (_, _, attrs) in &items {
        if attrs.get("del").map(|s| s == "1").unwrap_or(false) {
            continue;
        }
        let key = attrs.get("key").map(|s| s.trim().to_string()).unwrap_or_default();
        if key.is_empty() {
            continue;
        }
        let kind = attrs
            .get("type")
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "text".to_string());
        let options: Vec<String> = attrs
            .get("options")
            .map(|s| {
                s.split(['，', ',', '、'])
                    .map(|x| x.trim().to_string())
                    .filter(|x| !x.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        fields.push(Field {
            key,
            label: attrs.get("label").map(|s| s.trim().to_string()).unwrap_or_default(),
            kind: kind.clone(),
            required: attrs.get("required").map(|s| s == "1").unwrap_or(false),
            max_length: None,
            min: None,
            max: None,
            decimals: None,
            options: if kind == "select" { Some(options) } else { None },
            placeholder: None,
            group_by_carrier: attrs.get("group_by_carrier").map(|s| s == "1").unwrap_or(false),
        });
    }
    (name, fields)
}

/// 保存模板的字段结构（网页版编辑器提交的表单）。
pub async fn template_save_web(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<std::collections::HashMap<String, String>>,
) -> Response {
    if !crate::is_admin(&headers, &state) {
        return redirect_err("register", "preview", "请先登录");
    }

    let id: i64 = form
        .get("id")
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);
    if id <= 0 {
        return redirect_err("register", "preview", "没有这个模板");
    }
    let base_version: Option<i64> = form.get("base_version").and_then(|s| s.trim().parse().ok());
    let (name, fields) = parse_template_form(&form);
    if name.is_empty() {
        return redirect_err("register", "preview", "模板名称不能为空");
    }
    if name.chars().count() > 20 {
        return redirect_err("register", "preview", "模板名称最多 20 字");
    }

    let schema = Schema {
        // title 服务端和客户端都没在用，但保存时原样带着，别把人家的东西抹掉
        title: None,
        fields: fields.clone(),
    };
    if let Err(msg) = validate_schema(&schema) {
        return redirect_err("register", "preview", &msg);
    }
    let schema_json = match serde_json::to_string(&schema) {
        Ok(s) => s,
        Err(e) => return redirect_err("register", "preview", &format!("字段定义无法序列化：{e}")),
    };

    let result = {
        let conn = state.conn.get().unwrap();
        // 已经删掉的模板不让改
        if db::form_template_deleted(&conn, id).unwrap_or(false) {
            return redirect_err("register", "preview", "这个模板已经删除了，不能再改");
        }
        let current = db::form_template_by_id(&conn, id).ok().flatten();
        let Some(current) = current else {
            return redirect_err("register", "preview", "没有这个模板");
        };
        // 把原来的 title 带回去
        let mut schema_json = schema_json;
        if let Ok(old) = serde_json::from_str::<Schema>(&current.schema_json) {
            if let Some(t) = old.title {
                if let Ok(mut v) = serde_json::from_str::<Value>(&schema_json) {
                    if let Some(o) = v.as_object_mut() {
                        o.insert("title".to_string(), Value::String(t));
                    }
                    if let Ok(s) = serde_json::to_string(&v) {
                        schema_json = s;
                    }
                }
            }
        }
        db::form_template_save(
            &conn,
            Some(id),
            &name,
            &schema_json,
            base_version,
            current.sort_order,
        )
    };

    match result {
        Ok(db::TemplateSave::Saved { version, .. }) => redirect_ok(
            "register",
            "preview",
            &format!("已保存，模板升到 v{version}；已经填过的登记不受影响"),
        ),
        Ok(db::TemplateSave::Conflict { current }) => redirect_err(
            "register",
            "preview",
            &format!("模板已被别处修改（现在是 v{current}），请关掉这个窗口重新打开再改"),
        ),
        Ok(db::TemplateSave::NotFound) => redirect_err("register", "preview", "没有这个模板"),
        Ok(db::TemplateSave::NameTaken) => {
            redirect_err("register", "preview", "已经有同名模板了")
        }
        Err(e) => redirect_err("register", "preview", &format!("保存失败：{e}")),
    }
}

/* -------------------------------------------- 登记的预览与编辑 */

/// 取一条登记的编辑表单（HTML 片段）。「表单预览」点「预览」时用。
pub async fn submission_edit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Response {
    if !crate::is_admin(&headers, &state) {
        return (StatusCode::FORBIDDEN, "请先登录").into_response();
    }
    let conn = state.conn.get().unwrap();
    let Some(sub) = db::form_submission_by_id(&conn, id).ok().flatten() else {
        return (StatusCode::NOT_FOUND, "没有这条登记").into_response();
    };
    let schema: Option<Schema> = serde_json::from_str(&sub.template_snapshot).ok();
    let data: Map<String, Value> = serde_json::from_str::<Value>(&sub.data_json)
        .ok()
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();
    let fields: Vec<html::EditorField> = match &schema {
        Some(s) => s
            .fields
            .iter()
            .map(|f| html::EditorField {
                key: f.key.clone(),
                label: f.label.clone(),
                kind: f.kind.clone(),
                value: data.get(&f.key).map(value_text).unwrap_or_default(),
                options: f.options.clone().unwrap_or_default(),
            })
            .collect(),
        None => Vec::new(),
    };
    let templates = db::form_templates_all(&conn).unwrap_or_default();
    let name = templates
        .iter()
        .find(|t| t.id == sub.template_id)
        .map(|t| t.name.clone())
        .unwrap_or_else(|| format!("已删除的模板 #{}", sub.template_id));
    let html_out = html::submission_editor(
        sub.id,
        &name,
        &sub.user_name,
        &crate::admin::short_time(sub.client_time, state.cfg.tz_offset_min),
        &fields,
    );
    (StatusCode::OK, html_out).into_response()
}

#[derive(Deserialize)]
pub struct SubmissionSaveForm {
    id: i64,
    /// 每个字段一个 `f_<key>` 参数。这里统一收进来再按快照校验。
    #[serde(flatten)]
    fields: std::collections::HashMap<String, String>,
}

/// 改一条登记的内容。**只改 `data_json`**，字段定义（快照）不动 ——
/// 改了列就会跟同模板的其他行对不上。
pub async fn submission_save(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<SubmissionSaveForm>,
) -> Response {
    if !crate::is_admin(&headers, &state) {
        return redirect_err("register", "preview", "请先登录");
    }

    // `f_<key>` 还原成字段 key（网格那个 JSON 接口直接收 key，不走这层）
    let mut values: BTreeMap<String, String> = BTreeMap::new();
    for (k, v) in &form.fields {
        if let Some(key) = k.strip_prefix("f_") {
            values.insert(key.to_string(), v.clone());
        }
    }

    let conn = state.conn.get().unwrap();
    match apply_submission_edit(&conn, form.id, &values) {
        Ok(()) => redirect_ok("register", "preview", "已保存"),
        Err(msg) => redirect_err("register", "preview", &format!("没存上：{msg}")),
    }
}

#[derive(Deserialize)]
pub struct GridSaveReq {
    id: i64,
    /// key 就是字段 key（不是 `f_key`）。没提到的字段保持原样不动。
    #[serde(default)]
    data: BTreeMap<String, String>,
}

/// 网格里改完一个格子就自动存（fetch JSON，不整页跳）。
/// 校验规则和写入路径跟弹层那个 POST **完全一样**，只是换了种收发方式。
pub async fn grid_save(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<GridSaveReq>,
) -> Response {
    if !crate::is_admin(&headers, &state) {
        return (StatusCode::FORBIDDEN, "请先登录").into_response();
    }
    let conn = state.conn.get().unwrap();
    match apply_submission_edit(&conn, req.id, &req.data) {
        Ok(()) => Json(json!({
            "ok": true,
            "at": crate::admin::short_time(db::now(), state.cfg.tz_offset_min),
        }))
        .into_response(),
        Err(msg) => Json(json!({ "ok": false, "msg": msg })).into_response(),
    }
}

/// 改一条登记 —— 弹层 POST 和网格 JSON **共用**这一个函数。
///
/// 几条规矩：
/// - 只改 `data_json`，**快照不动**（改了列就跟同模板其他行对不上了）；
/// - 按**提交那一刻的快照**校验，校验不过就什么都不改；
/// - 以库里现有内容为底，只覆盖传进来的字段 —— 没提到的键（比如老版本
///   遗留的字段）原样保留，不会被清空。
pub(crate) fn apply_submission_edit(
    conn: &rusqlite::Connection,
    id: i64,
    values: &BTreeMap<String, String>,
) -> Result<(), String> {
    let Some(sub) = db::form_submission_by_id(conn, id).ok().flatten() else {
        return Err("没有这条登记".to_string());
    };

    let schema: Schema = match serde_json::from_str(&sub.template_snapshot) {
        Ok(s) => s,
        Err(_) => return Err("这条登记的字段定义坏了，改不了".to_string()),
    };

    let mut data: Map<String, Value> = serde_json::from_str::<Value>(&sub.data_json)
        .ok()
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();
    for f in &schema.fields {
        if let Some(v) = values.get(&f.key) {
            let v = v.trim();
            if v.is_empty() {
                // 清空 = 删掉这个键，跟客服在客户端填表时的语义一致
                data.remove(&f.key);
            } else if f.kind == "express" {
                let (tracking_number, carrier) = v.split_once(" | ").unwrap_or((v, ""));
                data.insert(f.key.clone(), json!({"tracking_number":tracking_number.trim(),"carrier":carrier.trim()}));
            } else {
                data.insert(f.key.clone(), Value::String(v.to_string()));
            }
        }
    }

    let errs = validate_data(&schema, &data);
    if !errs.is_empty() {
        // 报错带上字段名，不然客服不知道是哪一栏填错了
        let label_of = |key: &str| {
            schema
                .fields
                .iter()
                .find(|f| f.key == key)
                .map(|f| f.label.clone())
                .unwrap_or_else(|| key.to_string())
        };
        let msg = errs
            .iter()
            .map(|(k, v)| format!("{v}（{}）", label_of(k)))
            .collect::<Vec<_>>()
            .join("；");
        return Err(msg);
    }

    let data_json = Value::Object(data).to_string();
    db::form_submission_update(conn, id, &data_json).map_err(|e| format!("保存失败：{e}"))?;
    Ok(())
}

/* ------------------------------------------------- 管理端人员与口令 */

/// 口令存哈希。**不存明文** —— 库文件被人拷走也猜不出口令。
pub fn token_hash(token: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(token.trim().as_bytes());
    hex::encode(h.finalize())
}

/// 口令最少几个字。太短的口令跟没有口令差不多。
const MIN_TOKEN_LEN: usize = 6;

#[derive(Deserialize)]
pub struct AdminSaveForm {
    #[serde(default)]
    id: Option<i64>,
    #[serde(default)]
    name: String,
    #[serde(default)]
    token: String,
    #[serde(default)]
    token2: String,
}

/// 加一个管理端人员，或改已有人员的口令。
pub async fn admin_user_save(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<AdminSaveForm>,
) -> Response {
    if !crate::is_admin(&headers, &state) {
        return redirect_err("login", "admins", "请先登录");
    }
    let name = form.name.trim().to_string();
    if name.is_empty() {
        return redirect_err("login", "admins", "姓名不能为空");
    }
    if name.chars().count() > 20 {
        return redirect_err("login", "admins", "姓名最多 20 个字");
    }

    let token = form.token.trim();
    if token.len() < MIN_TOKEN_LEN {
        return redirect_err("login", "admins", &format!("口令至少 {MIN_TOKEN_LEN} 个字"));
    }
    if token != form.token2.trim() {
        return redirect_err("login", "admins", "两次输的口令不一样");
    }

    let hash = token_hash(token);
    let result = {
        let conn = state.conn.get().unwrap();
        match form.id {
            Some(id) => db::admin_user_update(&conn, id, &name, Some(&hash)),
            None => db::admin_user_create(&conn, &name, &hash).map(|_| true),
        }
    };
    match result {
        Ok(true) => redirect_ok("login", "admins", &format!("已保存「{name}」")),
        Ok(false) => redirect_err("login", "admins", "没有这个人"),
        Err(e) => {
            if e.to_string().contains("UNIQUE") {
                redirect_err("login", "admins", "已经有同名的人了")
            } else {
                redirect_err("login", "admins", &format!("保存失败：{e}"))
            }
        }
    }
}

#[derive(Deserialize)]
pub struct AdminToggleForm {
    id: i64,
    #[serde(default)]
    enabled: Option<String>,
}

pub async fn admin_user_toggle(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<AdminToggleForm>,
) -> Response {
    if !crate::is_admin(&headers, &state) {
        return redirect_err("login", "admins", "请先登录");
    }
    let enabled = matches!(form.enabled.as_deref(), Some("1") | Some("true"));
    let result = {
        let conn = state.conn.get().unwrap();
        db::admin_user_toggle(&conn, form.id, enabled)
    };
    match result {
        Ok(true) => redirect_ok("login", "admins", if enabled { "已启用" } else { "已停用" }),
        Ok(false) => redirect_err("login", "admins", "没有这个人"),
        Err(e) => redirect_err("login", "admins", &format!("操作失败：{e}")),
    }
}

#[derive(Deserialize)]
pub struct AdminDeleteForm {
    id: i64,
}

pub async fn admin_user_delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<AdminDeleteForm>,
) -> Response {
    if !crate::is_admin(&headers, &state) {
        return redirect_err("login", "admins", "请先登录");
    }
    let result = {
        let conn = state.conn.get().unwrap();
        db::admin_user_delete(&conn, form.id)
    };
    match result {
        Ok(true) => redirect_ok("login", "admins", "已删除；这个人之后登不进管理页了"),
        Ok(false) => redirect_err("login", "admins", "没有这个人"),
        Err(e) => redirect_err("login", "admins", &format!("删除失败：{e}")),
    }
}

/// 改口令（管理端那一行的「改口令」按钮走这里）。
#[derive(Deserialize)]
pub struct AdminTokenForm {
    id: i64,
    #[serde(default)]
    token: String,
}

pub async fn admin_user_token(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<AdminTokenForm>,
) -> Response {
    if !crate::is_admin(&headers, &state) {
        return redirect_err("login", "admins", "请先登录");
    }
    let token = form.token.trim();
    if token.len() < MIN_TOKEN_LEN {
        return redirect_err("login", "admins", &format!("口令至少 {MIN_TOKEN_LEN} 个字"));
    }
    let hash = token_hash(token);
    let result = {
        let conn = state.conn.get().unwrap();
        // 名字不改，只换口令
        let name = db::admin_users(&conn)
            .unwrap_or_default()
            .into_iter()
            .find(|a| a.id == form.id)
            .map(|a| a.name)
            .unwrap_or_default();
        if name.is_empty() {
            Ok(false)
        } else {
            db::admin_user_update(&conn, form.id, &name, Some(&hash))
        }
    };
    match result {
        Ok(true) => redirect_ok("login", "admins", "口令已改"),
        Ok(false) => redirect_err("login", "admins", "没有这个人"),
        Err(e) => redirect_err("login", "admins", &format!("改口令失败：{e}")),
    }
}

/* ------------------------------------------------------------ 导出 xlsx */
pub async fn export(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<AdminQuery>,
) -> Response {
    if !crate::is_admin(&headers, &state) {
        return redirect_err("register", "preview", "请先登录");
    }
    if q.template.is_none() {
        // 跨模板并排导，列会稀疏得没法看 —— 一次只导一个模板
        return redirect_err("register", "preview", "导出请先选一个模板（一次只导一个）");
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

    let pool = state.conn.clone();
    let template_id = q.template;
    let export_data = tokio::task::spawn_blocking(move || {
        let conn = pool.get().map_err(|e| e.to_string())?;
        let rows = db::form_submissions(&conn, &filter).unwrap_or_default();
        // sheet 名取模板名；模板被「永久删除」了就退回登记里冗余存的那份
        let name = template_id
            .and_then(|id| db::form_template_by_id(&conn, id).ok().flatten())
            .map(|t| t.name)
            .or_else(|| {
                rows.first()
                    .map(|r| r.template_name.trim().to_string())
                    .filter(|s| !s.is_empty())
            })
            .unwrap_or_else(|| "提交记录".to_string());
        let carrier_key = template_id.and_then(|id| db::form_template_by_id(&conn, id).ok().flatten())
            .and_then(|t| serde_json::from_str::<Schema>(&t.schema_json).ok())
            .and_then(|s| s.fields.into_iter().find(|f| f.kind == "express" && f.group_by_carrier).map(|f| f.key));
        Ok::<_, String>((rows, name, carrier_key))
    })
    .await;
    let (rows, sheet_name, carrier_key) = match export_data {
        Ok(Ok(data)) => data,
        Ok(Err(e)) => {
            eprintln!("xlsx query failed: {e}");
            return redirect_err("register", "preview", "读取导出数据失败，请稍后重试");
        }
        Err(e) => {
            eprintln!("xlsx query worker failed: {e}");
            return redirect_err("register", "preview", "读取导出数据失败，请稍后重试");
        }
    };

    if rows.is_empty() {
        return redirect_err("register", "preview", "这个筛选下没有记录可导出");
    }

    let tz_offset_min = state.cfg.tz_offset_min;
    let export_sheet_name = sheet_name.clone();
    let bytes = match tokio::task::spawn_blocking(move || {
        if let Some(key) = carrier_key {
            let mut groups: BTreeMap<String, Vec<db::FormSubmission>> = BTreeMap::new();
            for row in rows {
                let carrier = serde_json::from_str::<Value>(&row.data_json).ok()
                    .and_then(|v| v.get(&key).and_then(|x| x.get("carrier")).and_then(Value::as_str).map(str::to_string))
                    .filter(|s| !s.trim().is_empty()).unwrap_or_else(|| "未识别或未填写".to_string());
                groups.entry(carrier).or_default().push(row);
            }
            let tables: Vec<_> = groups.iter().map(|(_, rows)| crate::xlsx::build_table(rows, tz_offset_min)).collect();
            let names: Vec<String> = groups.keys().map(|carrier| carrier.clone()).collect();
            let sheets: Vec<_> = tables.iter().zip(names).map(|(table, carrier)| (table, carrier)).collect();
            crate::xlsx::write_xlsx_sheets(&sheets)
        } else {
            let table = crate::xlsx::build_table(&rows, tz_offset_min);
            crate::xlsx::write_xlsx(&table, &export_sheet_name)
        }
    })
    .await
    {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(e)) => {
            return redirect_err("register", "preview", &format!("生成表格失败：{e}"));
        }
        Err(e) => {
            eprintln!("xlsx worker failed: {e}");
            return redirect_err("register", "preview", "生成表格失败，请稍后重试");
        }
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

/// 管理页的操作都重定向回带标签的地址：加人回「登录管理 → 客户端」、
/// 停用模板回「表单登记 → 模板管理」、导出出错回「表单登记 → 表单预览」——
/// 不然一操作就跳回概览，还得再点两次。
fn redirect_ok(tab: &str, sub: &str, msg: &str) -> Response {
    redirect_with(tab, sub, "ok", msg)
}

fn redirect_err(tab: &str, sub: &str, msg: &str) -> Response {
    redirect_with(tab, sub, "err", msg)
}

fn redirect_with(tab: &str, sub: &str, kind: &str, msg: &str) -> Response {
    let mut url = format!("/admin?tab={tab}");
    if !sub.is_empty() {
        url.push_str(&format!("&sub={sub}"));
    }
    url.push_str(&format!("&{}={}", kind, url_encode(msg)));
    axum::response::Redirect::to(&url).into_response()
}
