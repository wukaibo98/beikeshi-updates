//! 商品库：商品（SPU）+ 规格（SKU）+ 图片。
//!
//! 几个关键决定：
//!   - 图片**按内容哈希命名**：同一张图重复上传只存一份，商品行里只存哈希；
//!   - 上传时就统一成 800×800（等比缩放后白底补边，不裁掉内容）+ 160 缩略图，
//!     客户端列表只拉缩略图，点开才拉大图；
//!   - 增量同步靠 `rev`（见 db 模块注释），客户端带 `since` 来问；
//!   - 商品库**必须登录**才能看（里面有售价），未登录返回 401。

use std::collections::HashMap;
use std::path::PathBuf;

use axum::extract::{Multipart, Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::Form;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{db, is_admin, AppState};

/* ============================================================ 图片 */

/// 统一尺寸：长边缩到 800 后贴在 800×800 白底上居中
pub const IMAGE_SIZE: u32 = 800;
pub const THUMB_SIZE: u32 = 160;
pub const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;

fn encode_jpeg(img: &image::RgbImage, quality: u8) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality);
    enc.encode_image(img).map_err(|e| format!("图片编码失败：{e}"))?;
    Ok(out)
}

/// 任意图片 → 800×800 白底 JPEG。
///
/// 为什么是「补边」而不是「裁切」：瓶装产品是竖长条，居中裁成正方形会把瓶盖和瓶底切掉。
/// 补边保证任何比例都不丢内容，代价是四周可能留白 —— 商品图上完全可接受。
pub fn normalize_image(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let img = image::load_from_memory(bytes)
        .map_err(|e| format!("无法识别这张图片（支持 jpg / png / webp）：{e}"))?;
    let (w, h) = (img.width(), img.height());
    if w == 0 || h == 0 {
        return Err("图片尺寸为 0".into());
    }
    let scale = IMAGE_SIZE as f32 / w.max(h) as f32;
    let nw = ((w as f32 * scale).round() as u32).clamp(1, IMAGE_SIZE);
    let nh = ((h as f32 * scale).round() as u32).clamp(1, IMAGE_SIZE);

    let rgb = img.to_rgb8();
    let resized = image::imageops::resize(&rgb, nw, nh, image::imageops::FilterType::Lanczos3);
    let mut canvas =
        image::RgbImage::from_pixel(IMAGE_SIZE, IMAGE_SIZE, image::Rgb([255, 255, 255]));
    image::imageops::overlay(
        &mut canvas,
        &resized,
        ((IMAGE_SIZE - nw) / 2) as i64,
        ((IMAGE_SIZE - nh) / 2) as i64,
    );
    encode_jpeg(&canvas, 82)
}

/// 从 800 图再缩一张 160 缩略图（列表用它，约 8KB）
pub fn make_thumb(jpg: &[u8]) -> Result<Vec<u8>, String> {
    let img = image::load_from_memory(jpg)
        .map_err(|e| format!("缩略图生成失败：{e}"))?
        .to_rgb8();
    let small = image::imageops::resize(
        &img,
        THUMB_SIZE,
        THUMB_SIZE,
        image::imageops::FilterType::Lanczos3,
    );
    encode_jpeg(&small, 80)
}

fn sha16(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    hex::encode(&digest[..8])
}

fn image_dir(state: &AppState) -> PathBuf {
    state.cfg.data_dir.join("products").join("img")
}

pub fn store_image(state: &AppState, bytes: &[u8]) -> Result<String, String> {
    store_image_in(&image_dir(state), bytes)
}

/// 存图：算哈希 → 已存在就不重复写 → 返回哈希（商品行里存它）
pub fn store_image_in(dir: &PathBuf, bytes: &[u8]) -> Result<String, String> {
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err("图片太大（上限 10MB）".into());
    }
    let jpg = normalize_image(bytes)?;
    let hash = sha16(&jpg);
    let dir = dir.clone();
    std::fs::create_dir_all(&dir).map_err(|e| format!("建目录失败：{e}"))?;
    let big = dir.join(format!("{hash}.jpg"));
    if !big.exists() {
        std::fs::write(&big, &jpg).map_err(|e| format!("写图片失败：{e}"))?;
    }
    let small = dir.join(format!("{hash}_t.jpg"));
    if !small.exists() {
        let thumb = make_thumb(&jpg)?;
        std::fs::write(&small, &thumb).map_err(|e| format!("写缩略图失败：{e}"))?;
    }
    Ok(hash)
}

/// `/product-image/{hash}?s=thumb`。内容寻址，所以可以长缓存。
pub async fn image_file(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Query(q): Query<ImageQuery>,
) -> Response {
    // 哈希只允许 16 位十六进制，挡掉 ../ 之类
    let hash = name.trim_end_matches(".jpg");
    if hash.len() != 16 || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let thumb = matches!(q.s.as_deref(), Some("thumb"));
    let file = image_dir(&state).join(format!("{hash}{}.jpg", if thumb { "_t" } else { "" }));
    let Ok(bytes) = std::fs::read(&file) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    (
        [
            (header::CONTENT_TYPE, "image/jpeg"),
            (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
        ],
        bytes,
    )
        .into_response()
}

#[derive(Deserialize)]
pub struct ImageQuery {
    #[serde(default)]
    pub s: Option<String>,
}

/* ============================================================ 客户端接口 */

fn product_json(p: &db::ProductRow) -> Value {
    let forms: Value = serde_json::from_str(&p.forms_json).unwrap_or_else(|_| json!([]));
    let funcs: Value = serde_json::from_str(&p.funcs_json).unwrap_or_else(|_| json!([]));
    json!({
        "id": p.id,
        "name": p.name,
        "category": p.category,
        "class": p.class_name,
        "forms": forms,
        "functions": funcs,
        "usage": p.usage_text,
        "image": p.image,
        "hits": p.hit_count,
        "last_hit_at": p.last_hit_at,
        "enabled": p.enabled,
    })
}

fn sku_json(s: &db::SkuRow) -> Value {
    json!({
        "id": s.id,
        "product_id": s.product_id,
        "spec_value": s.spec_value,
        "spec_unit": s.spec_unit,
        "spec_pack": s.spec_pack,
        "barcode": s.barcode69,
        "price_cents": s.price_cents,
        "promo": s.promo,
        "sort": s.sort_order,
        "enabled": s.enabled,
    })
}

#[derive(Deserialize)]
pub struct SyncQuery {
    #[serde(default, deserialize_with = "crate::opt_i64")]
    pub since: Option<i64>,
}

/// `GET /products?since=<rev>` —— 只回比 since 新的行；`since` 省略 = 全量。
pub async fn api_list(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<SyncQuery>,
) -> Response {
    // 未登录不给看：商品库里有售价和促销
    if crate::forms::current_user(&state, &headers).is_none() {
        return (
            StatusCode::UNAUTHORIZED,
            axum::Json(json!({ "error": "请先登录后再查看商品库" })),
        )
            .into_response();
    }
    let since = q.since.unwrap_or(0).max(0);
    let conn = state.conn.get().unwrap();
    let sync = match db::product_sync(&conn, since) {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(json!({ "error": format!("读取失败：{e}") })),
            )
                .into_response()
        }
    };
    let dict = db::product_dict(&conn).unwrap_or_default();

    axum::Json(json!({
        "ok": true,
        "rev": sync.rev,
        "since": since,
        "changed": !sync.products.is_empty() || !sync.skus.is_empty() || !sync.deleted.is_empty(),
        "products": sync.products.iter().map(product_json).collect::<Vec<_>>(),
        "skus": sync.skus.iter().map(sku_json).collect::<Vec<_>>(),
        "deleted": sync.deleted.iter().map(|(k, id)| json!({ "kind": k, "id": id })).collect::<Vec<_>>(),
        "dict": dict.iter().map(|d| json!({ "kind": d.kind, "value": d.value })).collect::<Vec<_>>(),
    }))
    .into_response()
}

#[derive(Deserialize)]
pub struct HitBody {
    #[serde(default)]
    pub ids: Vec<i64>,
}

/// `POST /products/hit` —— 客户端点开商品详情时报一次，用于「热度排序」
pub async fn api_hit(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<HitBody>,
) -> Response {
    if crate::forms::current_user(&state, &headers).is_none() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let ids: Vec<i64> = body.ids.into_iter().take(50).collect();
    if !ids.is_empty() {
        let conn = state.conn.get().unwrap();
        let _ = db::product_hit(&conn, &ids);
    }
    axum::Json(json!({ "ok": true })).into_response()
}

/* ============================================================ 管理端动作 */

fn back(sub: &str, ok: Option<&str>, err: Option<&str>) -> Response {
    let mut url = format!("/admin?tab=products&sub={sub}");
    if let Some(m) = ok {
        url.push_str(&format!("&ok={}", crate::admin::encode(m)));
    }
    if let Some(m) = err {
        url.push_str(&format!("&err={}", crate::admin::encode(m)));
    }
    Redirect::to(&url).into_response()
}

/// 存完/存错都停在编辑页：id=0（还没建出来）就回到「新增」那张空表单
fn back_edit(id: i64, ok: Option<&str>, err: Option<&str>) -> Response {
    let mut url = if id > 0 {
        format!("/admin?tab=products&sub=list&edit={id}")
    } else {
        "/admin?tab=products&sub=list&new=1".to_string()
    };
    if let Some(m) = ok {
        url.push_str(&format!("&ok={}", crate::admin::encode(m)));
    }
    if let Some(m) = err {
        url.push_str(&format!("&err={}", crate::admin::encode(m)));
    }
    Redirect::to(&url).into_response()
}

fn require_admin(state: &AppState, headers: &HeaderMap) -> Option<Response> {
    if is_admin(headers, state) {
        None
    } else {
        Some(back("list", None, Some("登录已过期，请重新登录")))
    }
}

/// 多选勾选框：表单里 `mk-form:液体=1`，服务端按码表顺序还原成数组。
fn picked(map: &HashMap<String, String>, kind: &str) -> Vec<String> {
    let prefix = format!("mk-{kind}:");
    let mut out: Vec<String> = map
        .iter()
        .filter(|(k, v)| k.starts_with(&prefix) && v.as_str() == "1")
        .map(|(k, _)| k[prefix.len()..].to_string())
        .collect();
    out.sort();
    out.dedup();
    out
}

/// 表单里的一条规格（还没落库）。`price` 留着原始文本，转换失败要报给人看。
#[derive(Default, Clone)]
pub struct SkuInput {
    pub id: Option<i64>,
    pub value: f64,
    pub unit: String,
    pub pack: String,
    pub barcode: String,
    pub price: String,
    pub promo: String,
    /// 勾了「删」：保存时把这条删掉（新增行勾了也没意义，直接忽略）
    pub remove: bool,
}

impl SkuInput {
    fn is_blank(&self) -> bool {
        self.barcode.is_empty()
            && self.price.trim().is_empty()
            && self.promo.trim().is_empty()
            && self.value == 0.0
    }
}

/// 从表单里把 `sku_<序号>_<字段>` 抽出来，按序号排好。
pub fn parse_sku_rows(form: &HashMap<String, String>) -> Vec<SkuInput> {
    let mut rows: std::collections::BTreeMap<usize, SkuInput> = Default::default();
    for (key, raw) in form {
        let Some(rest) = key.strip_prefix("sku_") else { continue };
        let Some((idx, field)) = rest.split_once('_') else { continue };
        let Ok(idx) = idx.parse::<usize>() else { continue };
        let row = rows.entry(idx).or_default();
        let v = raw.trim();
        match field {
            "id" => row.id = v.parse::<i64>().ok().filter(|x| *x > 0),
            "value" => row.value = v.parse::<f64>().unwrap_or(0.0),
            "unit" => row.unit = v.to_string(),
            "pack" => row.pack = v.to_string(),
            "barcode" => row.barcode = v.to_string(),
            "price" => row.price = v.to_string(),
            "promo" => row.promo = v.to_string(),
            "del" => row.remove = v == "1" || v == "on",
            _ => {}
        }
    }
    rows.into_values().collect()
}

/// 商品 + 规格一次性保存。任何一条不合格就整体不动（先校验、后事务）。
pub fn save_product_with_skus(
    conn: &rusqlite::Connection,
    id: Option<i64>,
    p: &db::NewProduct<'_>,
    rows: &[SkuInput],
) -> Result<i64, String> {
    use std::collections::HashSet;

    // 1) 先校验：空行跳过，其余必须有 69 码、价格能转、69 码不重复
    let mut seen: HashSet<&str> = HashSet::new();
    let mut clean: Vec<&SkuInput> = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        if row.remove || row.is_blank() {
            continue;
        }
        if row.barcode.is_empty() {
            return Err(format!("第 {} 条规格没填 69 码", i + 1));
        }
        if !seen.insert(row.barcode.as_str()) {
            return Err(format!("69 码 {} 在这个商品里填了两次", row.barcode));
        }
        if price_to_cents(&row.price).is_none() {
            return Err(format!("69 码 {} 的售价格式不对（最多两位小数）", row.barcode));
        }
        if db::sku_barcode_taken(conn, &row.barcode, row.id.unwrap_or(0))
            .map_err(|e| format!("校验失败：{e}"))?
        {
            return Err(format!("69 码 {} 已被别的规格占用", row.barcode));
        }
        clean.push(row);
    }

    // 2) 再落库：商品 + 规格在同一个事务里，中途失败不会留下半个商品
    let tx = conn.unchecked_transaction().map_err(|e| format!("开事务失败：{e}"))?;
    let pid = db::product_save(&tx, id, p).map_err(|e| format!("保存商品失败：{e}"))?;

    for (sort, row) in clean.iter().enumerate() {
        let price_cents = price_to_cents(&row.price).unwrap_or(0);
        let new = db::NewSku {
            product_id: pid,
            spec_value: row.value,
            spec_unit: &row.unit,
            spec_pack: &row.pack,
            barcode69: &row.barcode,
            price_cents,
            promo: &row.promo,
            sort_order: sort as i64,
        };
        db::sku_save(&tx, row.id, &new).map_err(|e| format!("保存规格失败：{e}"))?;
    }

    // 3) 勾了删的，删掉（必须是这个商品自己的规格）
    for row in rows.iter().filter(|r| r.remove) {
        if let Some(sku_id) = row.id {
            db::sku_delete(&tx, sku_id, pid).map_err(|e| format!("删除规格失败：{e}"))?;
        }
    }

    tx.commit().map_err(|e| format!("提交失败：{e}"))?;
    Ok(pid)
}

pub async fn save(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    if let Some(r) = require_admin(&state, &headers) {
        return r;
    }
    let name = form.get("name").map(|s| s.trim()).unwrap_or("").to_string();
    if name.is_empty() {
        return back("list", None, Some("商品名称不能为空"));
    }
    // 新增时表单里塞的是 0，不能当成真 id 去 UPDATE
    let id = form
        .get("id")
        .and_then(|s| s.trim().parse::<i64>().ok())
        .filter(|v| *v > 0);
    let forms = picked(&form, "form");
    let funcs = picked(&form, "function");
    let usage = form.get("usage").map(|s| s.replace("\r\n", "\n")).unwrap_or_default();
    let category = form.get("category").cloned().unwrap_or_default();
    let class_name = form.get("class_name").cloned().unwrap_or_default();
    let image = form.get("image").cloned().unwrap_or_default();
    let forms_json = serde_json::to_string(&forms).unwrap_or_else(|_| "[]".into());
    let funcs_json = serde_json::to_string(&funcs).unwrap_or_else(|_| "[]".into());

    let new = db::NewProduct {
        name: &name,
        category: &category,
        class_name: &class_name,
        forms_json: &forms_json,
        funcs_json: &funcs_json,
        usage_text: &usage,
        image: &image,
    };
    let rows = parse_sku_rows(&form);

    let conn = state.conn.get().unwrap();
    match save_product_with_skus(&conn, id, &new, &rows) {
        // 存完停在编辑页：还能接着改规格
        Ok(pid) => {
            let kept = rows.iter().filter(|r| !r.remove && !r.is_blank()).count();
            back_edit(pid, Some(&format!("已保存「{name}」，{kept} 条规格")), None)
        }
        Err(msg) => back_edit(id.unwrap_or(0), None, Some(&msg)),
    }
}

pub async fn toggle(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    if let Some(r) = require_admin(&state, &headers) {
        return r;
    }
    let Some(id) = form.get("id").and_then(|s| s.trim().parse::<i64>().ok()) else {
        return back("list", None, Some("缺少商品 id"));
    };
    let enabled = form.get("enabled").map(|s| s.as_str() == "1").unwrap_or(false);
    let conn = state.conn.get().unwrap();
    match db::product_toggle(&conn, id, enabled) {
        Ok(_) => back("list", Some(if enabled { "已启用" } else { "已停用" }), None),
        Err(e) => back("list", None, Some(&format!("操作失败：{e}"))),
    }
}

pub async fn delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    if let Some(r) = require_admin(&state, &headers) {
        return r;
    }
    let Some(id) = form.get("id").and_then(|s| s.trim().parse::<i64>().ok()) else {
        return back("list", None, Some("缺少商品 id"));
    };
    let conn = state.conn.get().unwrap();
    match db::product_delete(&conn, id) {
        Ok(_) => back("list", Some("已删除（规格一并删除）"), None),
        Err(e) => back("list", None, Some(&format!("删除失败：{e}"))),
    }
}

/// 「元」字符串 → 「分」。允许 39 / 39.9 / 39.90，最多两位小数。
pub fn price_to_cents(raw: &str) -> Option<i64> {
    let s = raw.trim().replace(',', "");
    if s.is_empty() {
        return Some(0);
    }
    let (int_part, frac_part) = match s.split_once('.') {
        Some((a, b)) => (a, b),
        None => (s.as_str(), ""),
    };
    if int_part.len() > 7 || frac_part.len() > 2 {
        return None;
    }
    if !int_part.chars().all(|c| c.is_ascii_digit())
        || !frac_part.chars().all(|c| c.is_ascii_digit())
    {
        return None;
    }
    let yuan: i64 = if int_part.is_empty() { 0 } else { int_part.parse().ok()? };
    let cents: i64 = match frac_part.len() {
        0 => 0,
        1 => frac_part.parse::<i64>().ok()? * 10,
        _ => frac_part.parse().ok()?,
    };
    Some(yuan * 100 + cents)
}

/// 传图：multipart（product_id + file）。存完只改 `image` 一列。
pub async fn image_upload(
    State(state): State<AppState>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> Response {
    if let Some(r) = require_admin(&state, &headers) {
        return r;
    }
    let mut product_id: Option<i64> = None;
    let mut bytes: Option<Vec<u8>> = None;

    while let Ok(Some(field)) = multipart.next_field().await {
        match field.name().unwrap_or("") {
            "product_id" => {
                product_id = field.text().await.ok().and_then(|t| t.trim().parse().ok());
            }
            "file" => {
                if let Ok(data) = field.bytes().await {
                    bytes = Some(data.to_vec());
                }
            }
            _ => {}
        }
    }

    let Some(pid) = product_id else {
        return back("list", None, Some("缺少商品 id"));
    };
    let Some(bytes) = bytes else {
        return back("list", None, Some("没有收到图片文件"));
    };
    if bytes.is_empty() {
        return back("list", None, Some("图片是空文件"));
    }

    // 图片处理是 CPU 活，别占着运行时线程
    let stored = {
        let state2 = state.clone();
        match tokio::task::spawn_blocking(move || store_image(&state2, &bytes)).await {
            Ok(v) => v,
            Err(e) => Err(format!("处理中断：{e}")),
        }
    };
    match stored {
        Ok(hash) => {
            let conn = state.conn.get().unwrap();
            match db::product_set_image(&conn, pid, &hash) {
                Ok(_) => back("list", Some("图片已更新"), None),
                Err(e) => back("list", None, Some(&format!("保存失败：{e}"))),
            }
        }
        Err(msg) => back("list", None, Some(&msg)),
    }
}

pub async fn dict_add(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    if let Some(r) = require_admin(&state, &headers) {
        return r;
    }
    let kind = form.get("kind").cloned().unwrap_or_default();
    let value = form.get("value").map(|s| s.trim().to_string()).unwrap_or_default();
    if !matches!(kind.as_str(), "form" | "function" | "unit" | "pack") {
        return back("dict", None, Some("这类字段不支持修改"));
    }
    if value.is_empty() {
        return back("dict", None, Some("填一个名称"));
    }
    let conn = state.conn.get().unwrap();
    match db::dict_add(&conn, &kind, &value) {
        Ok(_) => back("dict", Some(&format!("已添加「{value}」")), None),
        Err(e) => back("dict", None, Some(&format!("添加失败：{e}"))),
    }
}

pub async fn dict_delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    if let Some(r) = require_admin(&state, &headers) {
        return r;
    }
    let Some(id) = form.get("id").and_then(|s| s.trim().parse::<i64>().ok()) else {
        return back("dict", None, Some("缺少 id"));
    };
    let conn = state.conn.get().unwrap();
    match db::dict_delete(&conn, id) {
        Ok(_) => back("dict", Some("已删除（已用这个值的商品不受影响）"), None),
        Err(e) => back("dict", None, Some(&format!("删除失败：{e}"))),
    }
}

#[derive(Deserialize)]
pub struct MoveBody {
    pub kind: String,
    pub ids: Vec<i64>,
}

/// 拖拽排序：前端把整组新顺序发过来
pub async fn dict_move(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<MoveBody>,
) -> Response {
    if !is_admin(&headers, &state) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let conn = state.conn.get().unwrap();
    match db::dict_reorder(&conn, &body.kind, &body.ids) {
        Ok(_) => axum::Json(json!({ "ok": true })).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

/* ============================================================ 管理页面 */

#[derive(Default)]
pub struct ProductQuery {
    /// 正在编辑的商品 id
    pub edit: Option<i64>,
    /// 点「新增商品」过来（渲染一张空表单）
    pub new: bool,
}

pub struct ProductPanes {
    pub list: String,
    pub import: String,
    pub dict: String,
}

fn pill(text: &str) -> String {
    if text.is_empty() {
        String::new()
    } else {
        format!(r#"<span class="tag">{}</span>"#, crate::escape(text))
    }
}

fn spec_text(s: &db::SkuRow) -> String {
    let mut out = String::new();
    if s.spec_value > 0.0 {
        let v = if (s.spec_value - s.spec_value.round()).abs() < 1e-9 {
            format!("{}", s.spec_value.round() as i64)
        } else {
            format!("{}", s.spec_value)
        };
        out.push_str(&v);
        out.push_str(&s.spec_unit);
    }
    if !s.spec_pack.is_empty() {
        out.push('/');
        out.push_str(&s.spec_pack);
    }
    if out.is_empty() {
        out.push('—');
    }
    out
}

fn yuan(cents: i64) -> String {
    if cents % 100 == 0 {
        format!("{}", cents / 100)
    } else {
        format!("{}.{:02}", cents / 100, cents % 100)
    }
}

fn options(values: &[String], current: &str) -> String {
    values
        .iter()
        .map(|v| {
            let sel = if v == current { " selected" } else { "" };
            format!(r#"<option value="{v}"{sel}>{v}</option>"#, v = crate::escape(v))
        })
        .collect()
}

fn dict_values(dict: &[db::DictRow], kind: &str) -> Vec<String> {
    dict.iter()
        .filter(|d| d.kind == kind && d.enabled)
        .map(|d| d.value.clone())
        .collect()
}

/// 商品编辑卡片：商品字段 + 图片 + 规格子表。全是普通表单，没有 JS 也能用。
fn edit_card(state: &AppState, dict: &[db::DictRow], product: Option<&db::ProductRow>) -> String {
    let pid = product.map(|p| p.id).unwrap_or(0);
    let name = product.map(|p| p.name.clone()).unwrap_or_default();
    let category = product.map(|p| p.category.clone()).unwrap_or_default();
    let class_name = product.map(|p| p.class_name.clone()).unwrap_or_default();
    let usage = product.map(|p| p.usage_text.clone()).unwrap_or_default();
    let image = product.map(|p| p.image.clone()).unwrap_or_default();
    let forms: Vec<String> = product
        .map(|p| serde_json::from_str(&p.forms_json).unwrap_or_default())
        .unwrap_or_default();
    let funcs: Vec<String> = product
        .map(|p| serde_json::from_str(&p.funcs_json).unwrap_or_default())
        .unwrap_or_default();

    let checks = |kind: &str, chosen: &[String], prefix: &str| -> String {
        dict.iter()
            .filter(|d| d.kind == kind && d.enabled)
            .map(|d| {
                let on = if chosen.contains(&d.value) { " checked" } else { "" };
                format!(
                    r#"<label style="margin-right:10px;white-space:nowrap"><input type="checkbox" style="width:auto" name="{prefix}{v}" value="1"{on}> {v}</label>"#,
                    prefix = prefix,
                    v = crate::escape(&d.value),
                    on = on
                )
            })
            .collect()
    };

    let cats = dict_values(dict, "category");
    let classes = dict_values(dict, "class");

    let image_block = if image.is_empty() {
        r#"<span class="muted">还没有图片</span>"#.to_string()
    } else {
        format!(
            r#"<img src="/product-image/{h}?s=thumb" width="72" height="72" alt="" style="border-radius:8px;border:1px solid #E2E8D8;background:#fff">"#,
            h = crate::escape(&image)
        )
    };

    // 规格行：已有的 + 一条空白（不点任何按钮，直接填完最下面的「保存」就一起存）；
    // 行多的时候点「＋ 加一行」在浏览器里克隆，不需要往返一次服务器。
    let conn = state.conn.get().unwrap();
    let existing = if pid > 0 { db::skus_of(&conn, pid).unwrap_or_default() } else { Vec::new() };
    let blank_index = existing.len();
    let units_all = dict_values(dict, "unit");
    let packs_all = dict_values(dict, "pack");

    let sku_row_html = |i: usize, s: Option<&db::SkuRow>| -> String {
        format!(
            r##"<div class="row" style="margin-bottom:8px;align-items:center">
  <input type="hidden" name="sku_{i}_id" value="{id}">
  <input name="sku_{i}_value" value="{value}" style="width:64px" placeholder="数值">
  <select name="sku_{i}_unit" style="width:76px">{units}</select>
  <select name="sku_{i}_pack" style="width:76px">{packs}</select>
  <input name="sku_{i}_barcode" value="{barcode}" style="width:132px" placeholder="69 码">
  <input name="sku_{i}_price" value="{price}" style="width:68px" placeholder="售价">
  <input name="sku_{i}_promo" value="{promo}" style="width:120px" placeholder="促销（可空）">
  <label class="muted" style="display:flex;align-items:center;gap:4px;white-space:nowrap">
    <input type="checkbox" name="sku_{i}_del" value="1" style="width:auto"> 删
  </label>
</div>"##,
            i = i,
            id = s.map(|x| x.id.to_string()).unwrap_or_default(),
            value = s
                .map(|x| if x.spec_value > 0.0 { format!("{}", x.spec_value) } else { String::new() })
                .unwrap_or_default(),
            units = options(&units_all, s.map(|x| x.spec_unit.as_str()).unwrap_or("")),
            packs = options(&packs_all, s.map(|x| x.spec_pack.as_str()).unwrap_or("")),
            barcode = crate::escape(s.map(|x| x.barcode69.as_str()).unwrap_or("")),
            price = s.map(|x| yuan(x.price_cents)).unwrap_or_default(),
            promo = crate::escape(s.map(|x| x.promo.as_str()).unwrap_or("")),
        )
    };

    let mut sku_rows = existing
        .iter()
        .enumerate()
        .map(|(i, s)| sku_row_html(i, Some(s)))
        .collect::<String>();
    // 末尾永远留一条空白行：多填一条，存完再来一条
    let blank_html = sku_row_html(blank_index, None);
    sku_rows.push_str(&format!(r#"<div id="sku-blank">{blank_html}</div>"#));
    // 「＋ 加一行」用的模板：把 __i__ 换成新序号
    let tpl = sku_row_html(999_999, None).replace("_999999_", "___i___");

    format!(
        r##"<div class="card">
  <h2>{title}<span class="muted">{sub}</span></h2>
  <form method="post" action="/admin/products/save" id="product-form">
    <input type="hidden" name="id" value="{pid}">
    <input type="hidden" name="image" value="{image}">
    <table>
      <tr><th style="width:90px">商品名称</th><td><input name="name" value="{name}" required></td></tr>
      <tr><th>类目 / 类别</th><td class="row">
        <select name="category" style="width:120px">{cats}</select>
        <select name="class_name" style="width:140px">{classes}</select>
      </td></tr>
      <tr><th>剂型（多选）</th><td>{form_checks}</td></tr>
      <tr><th>功能（多选）</th><td>{func_checks}</td></tr>
      <tr><th>使用方法</th><td><textarea name="usage" rows="6" placeholder="客服话术，可长文">{usage}</textarea></td></tr>
      <tr><th>主图</th><td class="row">{image_block}<span class="muted">上传后自动补成 800×800（白底）</span></td></tr>

      <tr><th style="vertical-align:top;padding-top:12px">规格</th><td style="padding-top:8px">
        <div class="muted" style="margin-bottom:6px">
          一个 69 码一条规格；<b>填完直接点最下面的「保存商品」就一起存</b>，不用单独保存
        </div>
        <div class="muted" style="margin-bottom:6px">数值 · 单位 · 包装 · 69 码 · 售价(元) · 促销</div>
        <div id="sku-rows">{sku_rows}</div>
        <button class="ghost" type="button" id="sku-add">＋ 加一行</button>
      </td></tr>
    </table>

    <div style="border-top:1px solid #EFF3E7;margin:14px 0"></div>

    <div class="row">
      <button type="submit">保存商品</button>
      <a class="ghost" href="/admin?tab=products&amp;sub=list">关闭</a>
    </div>
  </form>

  <form class="row" method="post" action="/admin/products/image" enctype="multipart/form-data" style="margin-top:12px">
    <input type="hidden" name="product_id" value="{pid}">
    <input type="file" name="file" accept="image/*" required style="max-width:280px">
    <button class="ghost" type="submit">上传 / 替换图片</button>
    <span class="muted">传完会立刻生效，不用再点保存</span>
  </form>

  <template id="sku-tpl">{tpl}</template>
</div>
<script>
(function () {{
  var add = document.getElementById('sku-add');
  var wrap = document.getElementById('sku-rows');
  var tpl = document.getElementById('sku-tpl');
  var box = document.getElementById('sku-blank');
  var next = Number({blank_index}) + 1;
  if (!add || !wrap || !tpl) return;
  add.addEventListener('click', function () {{
    var holder = document.createElement('div');
    holder.innerHTML = tpl.innerHTML.replace(/__i__/g, String(next++));
    var row = holder.firstElementChild;
    if (box && box.parentNode === wrap) wrap.insertBefore(row, box);
    else wrap.appendChild(row);
  }});
}})();
</script>"##,
        title = if pid == 0 { "新增商品" } else { "编辑商品" },
        sub = if pid == 0 {
            "先填商品信息并保存，再回来加规格".to_string()
        } else {
            format!(" #{pid}")
        },
        pid = pid,
        name = crate::escape(&name),
        usage = crate::escape(&usage),
        image = crate::escape(&image),
        cats = options(&cats, &category),
        classes = options(&classes, &class_name),
        form_checks = checks("form", &forms, "mk-form:"),
        func_checks = checks("function", &funcs, "mk-function:"),
        image_block = image_block,
        sku_rows = sku_rows,
        tpl = tpl,
        blank_index = blank_index,
    )
}

pub fn panes(state: &AppState, q: &ProductQuery) -> ProductPanes {
    let conn = state.conn.get().unwrap();
    let dict = db::product_dict(&conn).unwrap_or_default();

    /* ---- 商品表格 ---- */
    let products = db::products(&conn, false).unwrap_or_default();
    let rows: String = products
        .iter()
        .map(|p| {
            let skus = db::skus_of(&conn, p.id).unwrap_or_default();
            let spec_lines = if skus.is_empty() {
                r#"<span class="muted">没有规格</span>"#.to_string()
            } else {
                skus.iter()
                    .map(|s| {
                        let promo = if s.promo.is_empty() {
                            String::new()
                        } else {
                            format!(r#" <span style="color:#BA7517">{}</span>"#, crate::escape(&s.promo))
                        };
                        format!(
                            r#"<div class="muted" style="font-size:12px">{} · {} · <b>¥{}</b>{}</div>"#,
                            spec_text(s),
                            crate::escape(&s.barcode69),
                            yuan(s.price_cents),
                            promo
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("")
            };
            let forms: Vec<String> = serde_json::from_str(&p.forms_json).unwrap_or_default();
            let funcs: Vec<String> = serde_json::from_str(&p.funcs_json).unwrap_or_default();
            let tags = std::iter::once(pill(&p.category))
                .chain(std::iter::once(pill(&p.class_name)))
                .chain(forms.iter().map(|v| pill(v)))
                .chain(funcs.iter().map(|v| pill(v)))
                .collect::<String>();
            let thumb = if p.image.is_empty() {
                r#"<div style="width:48px;height:48px;border-radius:8px;background:#F1EFE8"></div>"#.to_string()
            } else {
                format!(
                    r#"<img src="/product-image/{}?s=thumb" width="48" height="48" alt="" style="border-radius:8px;border:1px solid #E2E8D8">"#,
                    crate::escape(&p.image)
                )
            };
            let toggle_label = if p.enabled { "停用" } else { "启用" };
            format!(
                r##"<tr>
  <td>{thumb}</td>
  <td><b>{name}</b>{off}<div style="margin-top:4px">{tags}</div></td>
  <td>{specs}</td>
  <td style="white-space:nowrap">
    <a class="ghost" href="/admin?tab=products&amp;sub=list&amp;edit={id}">编辑</a>
    <form method="post" action="/admin/products/toggle" style="display:inline">
      <input type="hidden" name="id" value="{id}">
      <input type="hidden" name="enabled" value="{next_enabled}">
      <button class="ghost" type="submit">{toggle_label}</button>
    </form>
    <form method="post" action="/admin/products/delete" style="display:inline" onsubmit="return confirm('删除商品会连规格一起删，确定？')">
      <input type="hidden" name="id" value="{id}">
      <button class="ghost" type="submit">删除</button>
    </form>
  </td>
</tr>"##,
                thumb = thumb,
                name = crate::escape(&p.name),
                off = if p.enabled { "" } else { r#" <span class="tag off">已停用</span>"# },
                tags = tags,
                specs = spec_lines,
                id = p.id,
                next_enabled = if p.enabled { "0" } else { "1" },
                toggle_label = toggle_label,
            )
        })
        .collect::<Vec<_>>()
        .join("");
    let rows = if rows.is_empty() {
        r#"<tr><td colspan="4" class="muted">还没有商品。点右上角「新增商品」，或用「批量导入」。</td></tr>"#.to_string()
    } else {
        rows
    };

    let editing = if let Some(id) = q.edit {
        db::product_by_id(&conn, id)
            .ok()
            .flatten()
            .map(|p| edit_card(state, &dict, Some(&p)))
            .unwrap_or_default()
    } else if q.new {
        edit_card(state, &dict, None)
    } else {
        String::new()
    };

    let list = format!(
        r##"{editing}
<div class="card">
  <h2>商品<span class="muted">共 {n} 个商品 · 一个 69 码一条规格</span>
    <span class="row" style="float:right">
      <a class="ghost" href="/admin/products/export.xlsx" title="导出成 Excel（和导入模板同一套列）">导出</a>
      <a class="ghost" href="/admin?tab=products&amp;sub=list&amp;new=1">新增商品</a>
    </span>
  </h2>
  <table>
    <tr><th style="width:60px">图</th><th>商品</th><th>规格 · 69 码 · 售价</th><th style="width:180px">操作</th></tr>
    {rows}
  </table>
</div>"##,
        editing = editing,
        n = products.len(),
        rows = rows,
    );

    /* ---- 批量导入 ---- */
    let import = r##"<div class="card">
  <h2>批量导入<span class="muted">Excel 填字 + 图片打包，一次灌几百个</span></h2>
  <p class="muted" style="line-height:1.8">
    <b>模板就是导出格式</b>（一张表，13 列，完全一致），所以「导出 → 改 → 导入」是闭环。<br>
    ① <b>一行一条规格</b>：同一个商品的几行用<strong>同一个商品名</strong>，类目/剂型/话术这些只写第一行就行，导入时按商品名合并；<br>
    ② 没写 69 码的行只当商品（不建规格）；69 码已存在就更新，不会重复建；<br>
    ③ 图片打包成 zip，文件名用 <code>商品名.jpg</code> 或 <code>69码.jpg</code>（支持 jpg/png/webp），和 Excel 一起传。
  </p>
  <div class="row" style="margin-bottom:14px">
    <a class="ghost" href="/admin/products/template.xlsx">下载空模板</a>
    <a class="ghost" href="/admin/products/export.xlsx">导出当前商品库</a>
  </div>
  <form method="post" action="/admin/products/import" enctype="multipart/form-data">
    <table>
      <tr><th style="width:110px">Excel 文件</th><td><input type="file" name="sheet" accept=".xlsx" required></td></tr>
      <tr><th>图片 zip</th><td><input type="file" name="images" accept=".zip"><span class="muted" style="margin-left:8px">可选；不传就只导入文字</span></td></tr>
    </table>
    <div class="row" style="margin-top:12px"><button type="submit">开始导入</button></div>
  </form>
</div>"##.to_string();

    /* ---- 分类码表 ---- */
    let mut dict_html = String::new();
    for (kind, title, editable) in [
        ("category", "类目", false),
        ("class", "类别", false),
        ("form", "剂型", true),
        ("function", "功能", true),
        ("unit", "规格单位", true),
        ("pack", "包装", true),
    ] {
        let items: Vec<&db::DictRow> =
            dict.iter().filter(|d| d.kind == kind && d.enabled).collect();
        let list = items
            .iter()
            .map(|d| {
                let del = if editable {
                    format!(
                        r#"<form method="post" action="/admin/products/dict/delete" style="display:inline">
  <input type="hidden" name="id" value="{id}">
  <button class="ghost" type="submit" title="删除">×</button>
</form>"#,
                        id = d.id
                    )
                } else {
                    String::new()
                };
                format!(
                    r#"<span class="tag" draggable="{drag}" data-kind="{kind}" data-id="{id}" style="margin:0 6px 6px 0;padding:5px 9px;font-size:12px">{value}{del}</span>"#,
                    drag = if editable { "true" } else { "false" },
                    kind = kind,
                    id = d.id,
                    value = crate::escape(&d.value),
                    del = del
                )
            })
            .collect::<String>();
        let add = if editable {
            format!(
                r#"<form class="row" method="post" action="/admin/products/dict/add" style="margin-top:8px">
  <input type="hidden" name="kind" value="{kind}">
  <input name="value" placeholder="加一个" style="width:160px">
  <button class="ghost" type="submit">添加</button>
</form>"#,
                kind = kind
            )
        } else {
            r#"<span class="muted">固定项，不可改</span>"#.to_string()
        };
        dict_html.push_str(&format!(
            r#"<div class="card"><h2>{title}<span class="muted">{n} 项{drag_hint}</span></h2>{list}{add}</div>"#,
            title = title,
            n = items.len(),
            drag_hint = if editable { " · 可拖动排序" } else { "" },
            list = list,
            add = add,
        ));
    }

    let dict = format!(
        r##"{dict_html}
<script>
(function () {{
  var dragging = null;
  document.querySelectorAll('[data-kind][draggable="true"]').forEach(function (el) {{
    el.addEventListener('dragstart', function () {{ dragging = el; el.style.opacity = .4; }});
    el.addEventListener('dragend', function () {{
      el.style.opacity = '';
      if (!dragging) return;
      var kind = dragging.dataset.kind;
      var ids = [];
      document.querySelectorAll('[data-kind="' + kind + '"][draggable="true"]').forEach(function (x) {{ ids.push(+x.dataset.id); }});
      dragging = null;
      fetch('/admin/products/dict/move', {{
        method: 'POST',
        headers: {{ 'Content-Type': 'application/json' }},
        body: JSON.stringify({{ kind: kind, ids: ids }})
      }}).then(function () {{ location.reload(); }});
    }});
    el.addEventListener('dragover', function (e) {{
      e.preventDefault();
      if (!dragging || dragging === el || dragging.dataset.kind !== el.dataset.kind) return;
      var box = el.getBoundingClientRect();
      var after = (e.clientX - box.left) > box.width / 2;
      el.parentNode.insertBefore(dragging, after ? el.nextSibling : el);
    }});
  }});
}})();
</script>"##
    );

    ProductPanes { list, import, dict }
}
