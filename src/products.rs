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
/// 一个商品最多几张图。**第一个就是主图**（列表默认显示它）。
pub const MAX_IMAGES: usize = 5;

/// 图片 hash 只允许 16 位十六进制：挡掉 `../` 和 Excel 里手填的怪名字。
pub fn valid_hash(s: &str) -> bool {
    s.len() == 16 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// 规范化图片列表：过滤非法 hash、去重、最多留 `MAX_IMAGES` 张。
/// **顺序有意义**：第一个是主图，设主图 = 挪到数组开头。
pub fn normalize_images<I, S>(list: I) -> Vec<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut out: Vec<String> = Vec::new();
    for item in list {
        let s = item.as_ref().trim().to_string();
        if !valid_hash(&s) || out.contains(&s) {
            continue;
        }
        out.push(s);
        if out.len() == MAX_IMAGES {
            break;
        }
    }
    out
}

/// 数据库里存的是 JSON 数组字符串；坏数据一律当成「没有图」，不让整页 500。
pub fn parse_images(json: &str) -> Vec<String> {
    let raw: Vec<String> = serde_json::from_str(json).unwrap_or_default();
    normalize_images(raw)
}

pub fn images_to_json(list: &[String]) -> String {
    serde_json::to_string(&normalize_images(list)).unwrap_or_else(|_| "[]".into())
}

/// 导入时校验 Excel 里写的图片文件是不是真的在磁盘上（否则客户端会显示一排裂图）。
pub fn has_image(state: &AppState, hash: &str) -> bool {
    valid_hash(hash) && image_dir(state).join(format!("{hash}.jpg")).exists()
}

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
    if !valid_hash(hash) {
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
    // 老库迁移后 images_json 有值；万一没迁到，退回单张 image，老客户端永远有主图看。
    let mut images = parse_images(&p.images_json);
    if images.is_empty() && valid_hash(&p.image) {
        images.push(p.image.clone());
    }
    let main = images.first().cloned().unwrap_or_else(|| p.image.clone());
    json!({
        "id": p.id,
        "name": p.name,
        "category": p.category,
        "class": p.class_name,
        "forms": forms,
        "functions": funcs,
        "usage": p.usage_text,
        // `image` 保留给老客户端（= 主图）；新客户端读 `images` 数组翻多图
        "image": main,
        "images": images,
        "listed_at": p.listed_at,
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

/// 上架时间只收 `YYYY-MM-DD`（`<input type="date">` 传的就是这个格式）；空 = 不填。
pub fn clean_date(raw: &str) -> Option<String> {
    let v = raw.trim();
    if v.is_empty() {
        return Some(String::new());
    }
    let ok = v.len() == 10
        && v.bytes()
            .enumerate()
            .all(|(i, b)| if i == 4 || i == 7 { b == b'-' } else { b.is_ascii_digit() });
    if ok {
        Some(v.to_string())
    } else {
        None
    }
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
        // 单位 / 包装是下拉框，新增的空白行也总带着一个默认值，
        // 所以**不能**把它们算进「有没有填」——否则点一次保存就会凭空多一条空规格。
        // 用户至少填了数值、售价、促销或 69 码之一，才算真的想建一条规格。
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

    // 1) 先校验：空行跳过。**69 码选填** —— 只有填了的码才做去重和全局占用检查，
    //    留空可以有很多条（老商品经常还没申请条码）。报错统一带序号，
    //    否则没码的规格只会显示「69 码  已被占用」这种看不懂的话。
    let mut seen: HashSet<&str> = HashSet::new();
    let mut clean: Vec<&SkuInput> = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        if row.remove || row.is_blank() {
            continue;
        }
        let label = if row.barcode.is_empty() {
            format!("第 {} 条规格（未填 69 码）", i + 1)
        } else {
            format!("69 码 {}", row.barcode)
        };
        if !row.barcode.is_empty() && !seen.insert(row.barcode.as_str()) {
            return Err(format!("{} 在这个商品里填了两次", label));
        }
        if price_to_cents(&row.price).is_none() {
            return Err(format!("{} 的售价格式不对（最多两位小数）", label));
        }
        if !row.barcode.is_empty()
            && db::sku_barcode_taken(conn, &row.barcode, row.id.unwrap_or(0))
                .map_err(|e| format!("校验失败：{e}"))?
        {
            return Err(format!("{} 已被别的规格占用", label));
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
    // 图片由独立的上传/删除动作维护。保存商品时**不能**拿表单里的旧值去写 image，
    // 否则另一个标签刚传的图会被这里的旧隐藏字段覆盖掉。直接从库里读现有主图。
    let conn = state.conn.get().unwrap();
    let image = id
        .and_then(|pid| db::product_by_id(&conn, pid).ok().flatten())
        .map(|p| parse_images(&p.images_json).first().cloned().unwrap_or(p.image))
        .unwrap_or_default();
    let listed_raw = form.get("listed_at").map(String::as_str).unwrap_or("");
    // 上架时间是**业务上正式开卖的日期**，不是录入时间 —— 不替用户猜，
    // 空着就空着（客户端不显示这一行），要填由人来填。
    let Some(listed_at) = clean_date(listed_raw) else {
        return back_edit(id.unwrap_or(0), None, Some("上架时间格式不对，要 YYYY-MM-DD"));
    };
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
        listed_at: &listed_at,
    };
    let rows = parse_sku_rows(&form);

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

/// 传图：multipart（product_id + **多个** file，来自 `<input multiple>`）。
/// 追加到现有列表后面；总数超过 `MAX_IMAGES` 的部分不收。
pub async fn image_upload(
    State(state): State<AppState>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> Response {
    if let Some(r) = require_admin(&state, &headers) {
        return r;
    }
    let mut product_id: Option<i64> = None;
    let mut files: Vec<Vec<u8>> = Vec::new();

    while let Ok(Some(field)) = multipart.next_field().await {
        match field.name().unwrap_or("") {
            "product_id" => {
                product_id = field.text().await.ok().and_then(|t| t.trim().parse().ok());
            }
            "file" => {
                if let Ok(data) = field.bytes().await {
                    // 一次选太多也只处理前 12 张，别让 100 张图把内存吃满
                    if !data.is_empty() && files.len() < 12 {
                        files.push(data.to_vec());
                    }
                }
            }
            _ => {}
        }
    }

    let Some(pid) = product_id.filter(|v| *v > 0) else {
        return back("list", None, Some("请先保存商品，再上传图片"));
    };
    if files.is_empty() {
        return back_edit(pid, None, Some("没有收到图片文件"));
    }

    let current = {
        let conn = state.conn.get().unwrap();
        db::product_by_id(&conn, pid)
            .ok()
            .flatten()
            .map(|p| parse_images(&p.images_json))
            .unwrap_or_default()
    };
    let room = MAX_IMAGES.saturating_sub(current.len());
    if room == 0 {
        return back_edit(pid, None, Some("已经有 5 张图了，先删掉一张再传"));
    }
    let take = room.min(files.len());
    let skipped = files.len() - take;

    // 图片处理是 CPU 活，别占着运行时线程
    let stored = {
        let state2 = state.clone();
        let batch: Vec<Vec<u8>> = files[..take].to_vec();
        match tokio::task::spawn_blocking(move || {
            let mut hashes = Vec::new();
            let mut errs = Vec::new();
            for b in &batch {
                match store_image(&state2, b) {
                    Ok(h) => hashes.push(h),
                    Err(e) => errs.push(e),
                }
            }
            (hashes, errs)
        })
        .await
        {
            Ok(v) => v,
            Err(e) => return back_edit(pid, None, Some(&format!("处理中断：{e}"))),
        }
    };
    let (hashes, errs) = stored;

    let mut merged = current.clone();
    merged.extend(hashes);
    let merged = normalize_images(merged);
    let added = merged.len().saturating_sub(current.len());

    let conn = state.conn.get().unwrap();
    if let Err(e) = db::product_set_images(&conn, pid, &merged) {
        return back_edit(pid, None, Some(&format!("保存失败：{e}")));
    }

    let mut msg = format!(
        "已上传 {} 张，现有 {} 张（最多 {} 张）",
        added,
        merged.len(),
        MAX_IMAGES
    );
    if skipped > 0 {
        msg.push_str(&format!("；另有 {skipped} 张超出上限没传"));
    }
    if !errs.is_empty() {
        msg.push_str(&format!("；{} 张处理失败：{}", errs.len(), errs.join("、")));
    }
    back_edit(pid, Some(&msg), None)
}

/// 删掉某一张 / 把某一张设为主图。按钮的 name/value 会随外部表单一起提交，
/// 所以整页一个 form 就够，不需要 JS 也不需要嵌套表单。
pub async fn image_action(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    if let Some(r) = require_admin(&state, &headers) {
        return r;
    }
    let Some(pid) = form.get("product_id").and_then(|s| s.trim().parse::<i64>().ok()) else {
        return back("list", None, Some("缺少商品 id"));
    };
    let act = form.get("act").map(String::as_str).unwrap_or("");
    let Some((action, hash)) = act.split_once(':') else {
        return back_edit(pid, None, Some("不知道要做什么"));
    };
    if !valid_hash(hash) {
        return back_edit(pid, None, Some("图片编号不对"));
    }
    if !matches!(action, "delete" | "primary") {
        return back_edit(pid, None, Some("不支持的操作"));
    }

    let conn = state.conn.get().unwrap();
    let Some(p) = db::product_by_id(&conn, pid).ok().flatten() else {
        return back("list", None, Some("商品不存在"));
    };
    let mut images = parse_images(&p.images_json);
    if !images.iter().any(|h| h == hash) {
        return back_edit(pid, None, Some("这张图已经不在了"));
    }
    images.retain(|h| h != hash);
    if action == "primary" {
        images.insert(0, hash.to_string());
    }
    match db::product_set_images(&conn, pid, &images) {
        Ok(_) => back_edit(
            pid,
            Some(if action == "primary" {
                "已把这张设为主图"
            } else {
                "已删除这张图"
            }),
            None,
        ),
        Err(e) => back_edit(pid, None, Some(&format!("保存失败：{e}"))),
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

/* -------------------------------------------------- 商品库搜索（客户端同款） */

/// 四组筛选维度：`(参数名, 中文名, 是否单选)`。
/// 前两组跟客户端一样是单选（一个商品只有一个类目 / 类别），后两组多选；
/// **组内 OR、组间 AND**，规则与 `web/assets/product-data.js::filterProducts` 完全一致。
/// 管理页字段叫 `class_name` / `funcs_json`，但 data 属性名沿用客户端的 `class` / `function`。
pub const PRODUCT_FILTER_KINDS: &[(&str, &str, bool)] = &[
    ("category", "类目", true),
    ("class", "类别", true),
    ("form", "剂型", false),
    ("function", "功能", false),
];

/// 一行商品的搜索文本：**商品名 + 所有 69 码**，空格分隔。
/// 客户端只搜这两样，管理页保持同款；别擅自把规格、话术、上架时间也塞进来
/// —— 两边结果不一致时管理员会以为数据没同步。
pub fn product_search_blob(name: &str, skus: &[db::SkuRow]) -> String {
    let mut parts = vec![name.to_string()];
    for s in skus {
        if !s.barcode69.is_empty() {
            parts.push(s.barcode69.clone());
        }
    }
    parts.join(" ")
}

fn json_attr(raw: &str) -> String {
    let list: Vec<String> = serde_json::from_str(raw).unwrap_or_default();
    serde_json::to_string(&list).unwrap_or_else(|_| "[]".into())
}

/// 表格行上的 `data-*`：JS 过滤只读属性，不解析单元格文字（单元格里有 ¥、标签、按钮）。
/// `escape` 会把 JSON 里的 `"` 变成 `&quot;`，浏览器还原后仍是合法 JSON。
pub fn product_row_attrs(p: &db::ProductRow, skus: &[db::SkuRow]) -> String {
    format!(
        r#" data-ps-row="1" data-search="{}" data-category="{}" data-class="{}" data-form="{}" data-function="{}""#,
        crate::escape(&product_search_blob(&p.name, skus)),
        crate::escape(&p.category),
        crate::escape(&p.class_name),
        crate::escape(&json_attr(&p.forms_json)),
        crate::escape(&json_attr(&p.funcs_json)),
    )
}

/// 搜索工具条。默认带 `hidden`：没有 JS 时不显示一个点了没反应的输入框，
/// 表格照常全量展示；JS 初始化成功后才揭开。
pub fn product_search_toolbar(dict: &[db::DictRow]) -> String {
    let groups: String = PRODUCT_FILTER_KINDS
        .iter()
        .map(|(kind, label, single)| {
            let items: Vec<&db::DictRow> =
                dict.iter().filter(|d| d.kind == *kind && d.enabled).collect();
            if items.is_empty() {
                return String::new();
            }
            let opts: String = items
                .iter()
                .map(|d| {
                    let v = crate::escape(&d.value);
                    format!(
                        r#"<label class="pf-item"><input type="checkbox" value="{v}"><span>{v}</span></label>"#,
                        v = v
                    )
                })
                .collect();
            format!(
                r##"<div class="pf" data-kind="{kind}" data-single="{single}">
  <button type="button" class="pf-btn" data-act="toggle">{label}<span class="pf-n" hidden>0</span><span class="pf-caret">▾</span></button>
  <div class="pf-pop" hidden>{opts}</div>
</div>"##,
                kind = kind,
                single = if *single { "1" } else { "0" },
                label = label,
                opts = opts,
            )
        })
        .collect();

    format!(
        r##"<div class="ps" id="ps" hidden>
  <div class="ps-row">
    <input type="text" id="ps-q" placeholder="搜索商品名称或 69 码" autocomplete="off" spellcheck="false">
    <span class="muted ps-count" id="ps-count"></span>
  </div>
  <div class="ps-filters" id="ps-filters">{groups}</div>
  <div class="ps-active" id="ps-active" hidden></div>
</div>"##,
        groups = groups,
    )
}

/// 搜索脚本：独立常量，不进 `format!`，所以花括号不用双写。
/// 交互跟客户端一致：120ms 防抖、单选/多选规则、已选条件点掉、清空筛选、结果计数。
const PRODUCT_SEARCH_JS: &str = r##"<script>
(function () {
  var ps = document.getElementById('ps');
  var q = document.getElementById('ps-q');
  var body = document.getElementById('ps-body');
  var filtersBox = document.getElementById('ps-filters');
  var active = document.getElementById('ps-active');
  if (!ps || !q || !body || !filtersBox || !active) return;
  var rows = Array.prototype.slice.call(body.querySelectorAll('[data-ps-row]'));
  var empty = document.getElementById('ps-empty');
  var count = document.getElementById('ps-count');
  var labels = { category: '类目', 'class': '类别', form: '剂型', 'function': '功能' };
  var filters = { category: [], 'class': [], form: [], 'function': [] };
  var kinds = ['category', 'class', 'form', 'function'];
  var timer = null;

  function esc(s) {
    return String(s).replace(/&/g, '&amp;').replace(/</g, '&lt;')
      .replace(/>/g, '&gt;').replace(/"/g, '&quot;');
  }
  function anyFilter() {
    return kinds.some(function (k) { return filters[k].length > 0; });
  }
  function values(row, kind) {
    if (kind === 'category' || kind === 'class') {
      return [row.getAttribute('data-' + kind) || ''];
    }
    try {
      var v = JSON.parse(row.getAttribute('data-' + kind) || '[]');
      return Object.prototype.toString.call(v) === '[object Array]' ? v : [];
    } catch (e) { return []; }
  }
  function matches(row) {
    var needle = q.value.trim().toLowerCase();
    if (needle && (row.getAttribute('data-search') || '').toLowerCase().indexOf(needle) < 0) {
      return false;
    }
    for (var i = 0; i < kinds.length; i++) {
      var picked = filters[kinds[i]];
      if (!picked.length) continue;
      var have = values(row, kinds[i]);
      var hit = picked.some(function (v) { return have.indexOf(v) >= 0; });
      if (!hit) return false;
    }
    return true;
  }
  function closePops() {
    filtersBox.querySelectorAll('.pf-pop').forEach(function (p) { p.hidden = true; });
  }
  function updateButtons() {
    filtersBox.querySelectorAll('.pf').forEach(function (box) {
      var n = filters[box.getAttribute('data-kind')].length;
      var btn = box.querySelector('.pf-btn');
      var badge = box.querySelector('.pf-n');
      if (btn) btn.classList.toggle('on', n > 0);
      if (badge) { badge.textContent = String(n); badge.hidden = n === 0; }
    });
  }
  function syncChecks() {
    filtersBox.querySelectorAll('.pf').forEach(function (box) {
      var picked = filters[box.getAttribute('data-kind')];
      box.querySelectorAll('input[type="checkbox"]').forEach(function (cb) {
        cb.checked = picked.indexOf(cb.value) >= 0;
      });
    });
  }
  function renderActive() {
    var html = '';
    kinds.forEach(function (kind) {
      filters[kind].forEach(function (v) {
        html += '<button type="button" class="ps-chip" data-kind="' + kind +
          '" data-value="' + esc(v) + '">' + labels[kind] + '：' + esc(v) +
          ' <span>×</span></button>';
      });
    });
    if (anyFilter()) html += '<button type="button" class="ps-chip clear" data-act="clear">清空筛选</button>';
    active.innerHTML = html;
    active.hidden = !html;
  }
  function apply() {
    var n = 0;
    rows.forEach(function (row) {
      var ok = matches(row);
      row.hidden = !ok;
      if (ok) n++;
    });
    var filtering = !!q.value.trim() || anyFilter();
    if (count) count.textContent = filtering
      ? '共 ' + rows.length + ' 个商品 · 符合条件 ' + n + ' 个'
      : '共 ' + rows.length + ' 个商品';
    if (empty) empty.hidden = !(n === 0 && filtering);
    renderActive();
    updateButtons();
  }

  q.addEventListener('input', function () {
    clearTimeout(timer);
    timer = setTimeout(apply, 120);
  });
  filtersBox.addEventListener('click', function (e) {
    var toggle = e.target.closest && e.target.closest('[data-act="toggle"]');
    if (!toggle) return;
    var pop = toggle.closest('.pf').querySelector('.pf-pop');
    var willOpen = pop.hidden;
    closePops();
    pop.hidden = !willOpen;
  });
  filtersBox.addEventListener('change', function (e) {
    var cb = e.target;
    if (!cb || cb.type !== 'checkbox') return;
    var box = cb.closest('.pf');
    var kind = box.getAttribute('data-kind');
    var val = cb.value;
    if (box.getAttribute('data-single') === '1') {
      var wasOn = filters[kind].indexOf(val) >= 0;
      filters[kind] = wasOn ? [] : [val];
      syncChecks();
    } else if (cb.checked) {
      if (filters[kind].indexOf(val) < 0) filters[kind].push(val);
    } else {
      filters[kind] = filters[kind].filter(function (v) { return v !== val; });
    }
    apply();
  });
  active.addEventListener('click', function (e) {
    var chip = e.target.closest && e.target.closest('.ps-chip');
    if (!chip) return;
    if (chip.getAttribute('data-act') === 'clear') {
      kinds.forEach(function (k) { filters[k] = []; });
    } else {
      var kind = chip.getAttribute('data-kind');
      var val = chip.getAttribute('data-value');
      filters[kind] = filters[kind].filter(function (v) { return v !== val; });
    }
    syncChecks();
    apply();
  });
  document.addEventListener('click', function (e) {
    if (!(e.target.closest && e.target.closest('.pf'))) closePops();
  });

  ps.hidden = false;
  apply();
})();
</script>"##;

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
    // 老库可能只有单张 image、images_json 还是空数组，这里兜一下
    let mut images = product.map(|p| parse_images(&p.images_json)).unwrap_or_default();
    if images.is_empty() && valid_hash(&image) {
        images.push(image.clone());
    }
    let listed_at = product.map(|p| p.listed_at.clone()).unwrap_or_default();
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

    // 每张图一个缩略图 + 「设为主图」/「删除」。按钮用 form 属性挂到页面底部的
    // 独立表单上，name/value 随表单一并提交 —— 无 JS、也没有嵌套表单。
    let image_block = if images.is_empty() {
        r#"<span class="muted">还没有图片</span>"#.to_string()
    } else {
        images
            .iter()
            .enumerate()
            .map(|(i, h)| {
                let label = if i == 0 {
                    "主图".to_string()
                } else {
                    format!("第 {} 张", i + 1)
                };
                let primary = if i > 0 {
                    format!(
                        r#"<button class="ghost" type="submit" form="image-action-form" name="act" value="primary:{h}" style="padding:2px 7px;font-size:12px">设为主图</button>"#,
                        h = crate::escape(h)
                    )
                } else {
                    String::new()
                };
                format!(
                    r##"<span style="display:inline-flex;flex-direction:column;align-items:center;gap:4px;margin:0 12px 8px 0;vertical-align:top">
          <img src="/product-image/{h}?s=thumb" width="72" height="72" alt="" style="object-fit:contain;border-radius:8px;border:1px solid #E2E8D8;background:#fff">
          <span class="muted" style="font-size:11px">{label}</span>
          <span class="row" style="gap:4px">
            {primary}
            <button class="ghost" type="submit" form="image-action-form" name="act" value="delete:{h}" style="padding:2px 7px;font-size:12px">删除</button>
          </span>
        </span>"##,
                    h = crate::escape(h),
                    label = label,
                    primary = primary,
                )
            })
            .collect::<String>()
    };

    let upload_controls = if pid > 0 {
        format!(
            r##"<div class="row" style="align-items:center;margin-top:8px">
          <input type="file" name="file" form="image-upload-form" accept="image/*" multiple required
                 style="width:auto;max-width:230px;height:auto;padding:4px 6px;background:transparent;border:1px dashed #D8E0CC">
          <button class="ghost" type="submit" form="image-upload-form">上传图片</button>
          <span class="muted">一次可选多张，最多 {max} 张；传完立刻生效，不用点保存</span>
        </div>"##,
            max = MAX_IMAGES
        )
    } else {
        r#"<div class="muted" style="margin-top:8px">先填好上面的信息、点「保存商品」，再回来传图片。</div>"#
            .to_string()
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
  <input name="sku_{i}_barcode" value="{barcode}" style="width:132px" placeholder="69 码（选填）">
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
    <table>
      <tr><th style="width:90px">商品名称</th><td><input name="name" value="{name}" required></td></tr>
      <tr><th>类目 / 类别</th><td class="row">
        <select name="category" style="width:120px">{cats}</select>
        <select name="class_name" style="width:140px">{classes}</select>
      </td></tr>
      <tr><th>剂型（多选）</th><td>{form_checks}</td></tr>
      <tr><th>功能（多选）</th><td>{func_checks}</td></tr>
      <tr><th>使用方法</th><td><textarea name="usage" rows="6" placeholder="客服话术，可长文">{usage}</textarea></td></tr>
      <tr><th>上架时间</th><td class="row">
        <input type="date" name="listed_at" value="{listed_at}" style="width:170px">
        <span class="muted">产品正式上架售卖的日期（不是录入时间）；不填则客户端不显示</span>
      </td></tr>
      <tr><th style="vertical-align:top;padding-top:12px">商品图片</th><td style="padding-top:8px">
        <div class="muted" style="margin-bottom:6px">
          最多 5 张；<b>第一张是主图</b>（客户端列表默认显示它），点图片可以看全部。
          上传后自动统一成 800×800（白底补边，不切产品）。
        </div>
        <div>{image_block}</div>
        {upload_controls}
      </td></tr>

      <tr><th style="vertical-align:top;padding-top:12px">规格</th><td style="padding-top:8px">
        <div class="muted" style="margin-bottom:6px">
          一行一条规格，<b>69 码可以留空</b>（没申请条码的商品照常建规格）；
          <b>填完直接点最下面的「保存商品」就一起存</b>，不用单独保存
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

  <!-- 传图 / 删图 / 设主图都是独立动作：控件挂在主表单里，但通过 form 属性
       关联到这两个外部表单（HTML 不允许 form 嵌套）。按钮的 name/value 会随
       所属表单一起提交，所以整页只需要各一个 form。 -->
  <form id="image-upload-form" method="post" action="/admin/products/image" enctype="multipart/form-data">
    <input type="hidden" name="product_id" value="{pid}">
  </form>
  <form id="image-action-form" method="post" action="/admin/products/image/action">
    <input type="hidden" name="product_id" value="{pid}">
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
        cats = options(&cats, &category),
        classes = options(&classes, &class_name),
        form_checks = checks("form", &forms, "mk-form:"),
        func_checks = checks("function", &funcs, "mk-function:"),
        image_block = image_block,
        upload_controls = upload_controls,
        listed_at = crate::escape(&listed_at),
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
                            if s.barcode69.is_empty() {
                                "未填 69 码".to_string()
                            } else {
                                crate::escape(&s.barcode69)
                            },
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
            // 列表只显示第一张（主图），有多张时挂个「N 张」角标
            let images = parse_images(&p.images_json);
            let main = images.first().cloned().unwrap_or_else(|| p.image.clone());
            let thumb = if main.is_empty() {
                r#"<div style="width:48px;height:48px;border-radius:8px;background:#F1EFE8"></div>"#.to_string()
            } else {
                let badge = if images.len() > 1 {
                    format!(
                        r#"<span style="position:absolute;right:-6px;bottom:-4px;background:#4C6B3C;color:#fff;border-radius:9px;padding:1px 5px;font-size:10px;line-height:14px;white-space:nowrap">{} 张</span>"#,
                        images.len()
                    )
                } else {
                    String::new()
                };
                format!(
                    r#"<span style="position:relative;display:inline-block"><img src="/product-image/{}?s=thumb" width="48" height="48" alt="" style="display:block;object-fit:contain;background:#fff;border-radius:8px;border:1px solid #E2E8D8">{badge}</span>"#,
                    crate::escape(&main),
                    badge = badge
                )
            };
            let listed = if p.listed_at.is_empty() {
                String::new()
            } else {
                format!(
                    r#" <span class="muted" style="font-size:12px">上架 {}</span>"#,
                    crate::escape(&p.listed_at)
                )
            };
            let toggle_label = if p.enabled { "停用" } else { "启用" };
            // 搜索用的 data-* 打在行上，JS 不解析单元格（里面有 ¥、标签、按钮）
            let attrs = product_row_attrs(p, &skus);
            format!(
                r##"<tr{attrs}>
  <td>{thumb}</td>
  <td><b>{name}</b>{off}{listed}<div style="margin-top:4px">{tags}</div></td>
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
                listed = listed,
                tags = tags,
                specs = spec_lines,
                id = p.id,
                next_enabled = if p.enabled { "0" } else { "1" },
                toggle_label = toggle_label,
                attrs = attrs,
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

    // 没有商品时不渲染工具条和脚本：一个空搜索框没意义，JS 也不该对着空表跑
    let has_products = !products.is_empty();
    let toolbar = if has_products {
        product_search_toolbar(&dict)
    } else {
        String::new()
    };
    let search_script = if has_products { PRODUCT_SEARCH_JS } else { "" };
    let empty_row = if has_products {
        r##"<tr class="ps-empty" id="ps-empty" hidden><td colspan="4" class="muted">没有符合条件的商品，换个关键词试试。</td></tr>"##
    } else {
        ""
    };

    let list = format!(
        r##"{editing}
<div class="card">
  <h2>商品<span class="muted">69 码选填 · 图片最多 5 张</span>
    <span class="row" style="float:right">
      <a class="ghost" href="/admin/products/export.xlsx" title="导出成 Excel（和导入模板同一套列）">导出</a>
      <a class="ghost" href="/admin?tab=products&amp;sub=list&amp;new=1">新增商品</a>
    </span>
  </h2>
  {toolbar}
  <table>
    <thead><tr><th style="width:60px">图</th><th>商品</th><th>规格 · 69 码 · 售价</th><th style="width:180px">操作</th></tr></thead>
    <tbody id="ps-body">
    {rows}
    {empty_row}
    </tbody>
  </table>
</div>{search_script}"##,
        editing = editing,
        toolbar = toolbar,
        rows = rows,
        empty_row = empty_row,
        search_script = search_script,
    );

    /* ---- 批量导入 ---- */
    let import = r##"<div class="card">
  <h2>批量导入<span class="muted">Excel 填字 + 图片打包，一次灌几百个</span></h2>
  <p class="muted" style="line-height:1.8">
    <b>模板就是导出格式</b>（一张表，14 列，完全一致），所以「导出 → 改 → 导入」是闭环。<br>
    ① <b>一行一条规格</b>：同一个商品的几行用<strong>同一个商品名</strong>，类目/剂型/话术这些只写第一行就行，导入时按商品名合并；<br>
    ② <b>69 码选填</b>：规格列（数值/单位/包装/售价/促销）全空的行只当商品、不建规格；
       填了规格列的就算 69 码留空也建规格。有 69 码按码认人、没码按「数值+单位+包装」认人，
       已存在就更新、不会重复建（所以导出→改→导入是闭环）；<br>
    ③ <b>图片文件名</b>列导出的是 hash 文件名，多张用 <code>;</code> 分开，<b>第一张是主图</b>；导入时只认服务器上真有的图，留空就沿用原来的；<br>
    ④ <b>图片 zip</b>：文件名用 <code>商品名.jpg</code> 或 <code>69码.jpg</code>；同一商品多张用
       <code>商品名-2.jpg</code>、<code>商品名-3.jpg</code>，或建 <code>商品名/</code> 文件夹放
       <code>01.jpg</code>、<code>02.jpg</code>。<b>一个商品最多 5 张</b>，zip 里给了图就整组替换（支持 jpg/png/webp）；<br>
    ⑤ <b>上架时间</b>是「产品正式开卖的日期」，留空就沿用原来的（新商品留空），填了就按 <code>2026-11-04</code> 这种格式写。
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
