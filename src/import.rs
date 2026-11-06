//! 商品库的批量导入 / 导出 / 模板下载。
//!
//! 为什么用 xlsx 而不是 CSV：Excel 另存 CSV 在中文 Windows 上是 GBK，
//! 传到服务端必然乱码；xlsx 是 UTF-8。
//!
//! **一张表，一行一条规格**（不是两个 sheet）：商品名相同的几行属于同一个商品，
//! 商品级字段（类目/类别/剂型/功能/使用方法/图片）只在第一行写就行，导入时按商品名合并。
//! 这样「导出 → 改 → 导入」在 Excel 里就是一张连续的表格，不用来回切标签页，也能直接排序筛选。

use std::collections::HashMap;
use std::io::Cursor;

use axum::extract::{Multipart, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};

use calamine::DataType;

use crate::{db, products, AppState};

const SHEET: &str = "商品";

/// 列顺序即导入模板顺序；改这里导入导出一起变
const HEADERS: [&str; 14] = [
    "商品名", "类目", "类别", "剂型", "功能", "使用方法", "图片文件名(多张用;分隔)", "上架时间",
    "规格数值", "单位", "包装", "69码", "售价(元)", "促销",
];

fn back(ok: Option<&str>, err: Option<&str>) -> Response {
    let mut url = "/admin?tab=products&sub=import".to_string();
    if let Some(m) = ok {
        url.push_str(&format!("&ok={}", crate::admin::encode(m)));
    }
    if let Some(m) = err {
        url.push_str(&format!("&err={}", crate::admin::encode(m)));
    }
    Redirect::to(&url).into_response()
}

/// 导入模板：**和导出完全同一套列**，只是没有数据行。
pub async fn template(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !crate::is_admin(&headers, &state) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    match build_export(&[], &[]) {
        Ok(bytes) => xlsx_response(bytes, "商品导入模板.xlsx"),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

pub async fn export(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !crate::is_admin(&headers, &state) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let (products, skus) = {
        let conn = state.conn.get().unwrap();
        let products = db::products(&conn, false).unwrap_or_default();
        let mut skus = Vec::new();
        for p in &products {
            skus.extend(db::skus_of(&conn, p.id).unwrap_or_default());
        }
        (products, skus)
    };
    match build_export(&products, &skus) {
        Ok(bytes) => xlsx_response(bytes, "商品库导出.xlsx"),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

/// 一行一条规格；没有规格的商品也占一行（后半段留空），不然导出会把它丢掉。
fn build_export(products: &[db::ProductRow], skus: &[db::SkuRow]) -> Result<Vec<u8>, String> {
    use rust_xlsxwriter::{Format, Workbook};

    let mut wb = Workbook::new();
    let sheet = wb.add_worksheet();
    sheet.set_name(SHEET).map_err(|e| e.to_string())?;
    let bold = Format::new().set_bold();
    for (i, h) in HEADERS.iter().enumerate() {
        sheet
            .write_with_format(0, i as u16, *h, &bold)
            .map_err(|e| e.to_string())?;
    }

    let mut row = 1u32;
    for p in products {
        let forms: Vec<String> = serde_json::from_str(&p.forms_json).unwrap_or_default();
        let funcs: Vec<String> = serde_json::from_str(&p.funcs_json).unwrap_or_default();
        // 导出的是图片 hash 文件名；多张用 `;` 分隔，顺序即客户端的看图顺序。
        let mut image_hashes = products::parse_images(&p.images_json);
        if image_hashes.is_empty() && products::valid_hash(&p.image) {
            image_hashes.push(p.image.clone());
        }
        let image_name = image_hashes
            .iter()
            .map(|h| format!("{h}.jpg"))
            .collect::<Vec<_>>()
            .join(";");
        let own: Vec<&db::SkuRow> = skus.iter().filter(|s| s.product_id == p.id).collect();

        let mut write_row = |spec: Option<&db::SkuRow>| -> Result<(), String> {
            let cells: [String; 14] = [
                p.name.clone(),
                p.category.clone(),
                p.class_name.clone(),
                forms.join("、"),
                funcs.join("、"),
                p.usage_text.clone(),
                image_name.clone(),
                p.listed_at.clone(),
                spec.map(|s| if s.spec_value > 0.0 { fmt_num(s.spec_value) } else { String::new() })
                    .unwrap_or_default(),
                spec.map(|s| s.spec_unit.clone()).unwrap_or_default(),
                spec.map(|s| s.spec_pack.clone()).unwrap_or_default(),
                spec.map(|s| s.barcode69.clone()).unwrap_or_default(),
                spec.map(|s| format!("{}.{:02}", s.price_cents / 100, s.price_cents % 100))
                    .unwrap_or_default(),
                spec.map(|s| s.promo.clone()).unwrap_or_default(),
            ];
            for (i, v) in cells.iter().enumerate() {
                sheet
                    .write(row, i as u16, v.as_str())
                    .map_err(|e| e.to_string())?;
            }
            row += 1;
            Ok(())
        };

        if own.is_empty() {
            write_row(None)?;
        } else {
            for s in own {
                write_row(Some(s))?;
            }
        }
    }

    sheet.set_column_width(0, 24).ok();
    sheet.set_column_width(5, 40).ok();
    sheet.set_column_width(10, 18).ok();

    wb.save_to_buffer().map_err(|e| e.to_string())
}

/// 「图片文件名」单元格 → hash 列表。只认内容寻址的文件名：
/// 手填的「正面.jpg」在服务器上没有对应文件，不能拿它当图（否则客户端一排裂图）。
fn split_image_names(cell: &str) -> Vec<String> {
    let names: Vec<String> = cell
        .split([';', ',', '、', '\n'])
        .map(|s| {
            let s = s.trim();
            let lower = s.to_ascii_lowercase();
            for ext in [".jpg", ".jpeg", ".png", ".webp"] {
                if lower.ends_with(ext) {
                    return s[..s.len() - ext.len()].to_string();
                }
            }
            s.to_string()
        })
        .collect();
    products::normalize_images(names)
}

fn fmt_num(v: f64) -> String {
    if (v - v.round()).abs() < 1e-9 {
        format!("{}", v.round() as i64)
    } else {
        format!("{v}")
    }
}

fn xlsx_response(bytes: Vec<u8>, filename: &str) -> Response {
    let encoded = crate::admin::encode(filename);
    (
        [
            (
                header::CONTENT_TYPE,
                "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet".to_string(),
            ),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"products.xlsx\"; filename*=UTF-8''{encoded}"),
            ),
        ],
        bytes,
    )
        .into_response()
}

/* ============================================================ 导入 */

/// 一行按列序取出（缺列按空串，数字转文本）
fn row_cells(row: &[calamine::Data], n: usize) -> Vec<String> {
    (0..n)
        .map(|i| {
            row.get(i)
                .map(|c| match c {
                    calamine::Data::Int(v) => v.to_string(),
                    calamine::Data::Float(v) => fmt_num(*v),
                    other => other.as_string().unwrap_or_default().trim().to_string(),
                })
                .unwrap_or_default()
        })
        .collect()
}

fn split_multi(raw: &str) -> Vec<String> {
    raw.split(['、', ',', '，', '/', ';', '；'])
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

pub async fn import(
    State(state): State<AppState>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> Response {
    if !crate::is_admin(&headers, &state) {
        return back(None, Some("登录已过期"));
    }

    let mut sheet_bytes: Option<Vec<u8>> = None;
    let mut zip_bytes: Option<Vec<u8>> = None;
    while let Ok(Some(field)) = multipart.next_field().await {
        let name = field.name().unwrap_or("").to_string();
        let Ok(data) = field.bytes().await else { continue };
        if data.is_empty() {
            continue;
        }
        match name.as_str() {
            "sheet" => sheet_bytes = Some(data.to_vec()),
            "images" => zip_bytes = Some(data.to_vec()),
            _ => {}
        }
    }

    let Some(sheet_bytes) = sheet_bytes else {
        return back(None, Some("没有收到 Excel 文件"));
    };

    // 几百行解析 + 写库都在阻塞线程里做，别占异步线程
    let result = {
        let state = state.clone();
        tokio::task::spawn_blocking(move || parse_and_import(&state, &sheet_bytes, zip_bytes)).await
    };
    match result {
        Ok(Ok(msg)) => back(Some(&msg), None),
        Ok(Err(msg)) => back(None, Some(&msg)),
        Err(e) => back(None, Some(&format!("导入中断：{e}"))),
    }
}

fn parse_and_import(
    state: &AppState,
    sheet_bytes: &[u8],
    zip_bytes: Option<Vec<u8>>,
) -> Result<String, String> {
    use calamine::{Reader, Xlsx};

    let mut wb: Xlsx<_> = calamine::open_workbook_from_rs(Cursor::new(sheet_bytes.to_vec()))
        .map_err(|e| format!("读不了这个 Excel：{e:?}"))?;

    // 只有一张表；万一被改名了，退而用第一张
    let range = match wb.worksheet_range(SHEET) {
        Ok(r) => r,
        Err(_) => {
            let first = wb
                .sheet_names()
                .first()
                .cloned()
                .ok_or_else(|| "这个 Excel 里没有工作表".to_string())?;
            wb.worksheet_range(&first)
                .map_err(|e| format!("读不到工作表「{first}」：{e:?}"))?
        }
    };

    let mut created = 0usize;
    let mut updated = 0usize;
    let mut sku_created = 0usize;
    let mut sku_updated = 0usize;
    let mut img_from_sheet = 0usize;
    let mut warnings: Vec<String> = Vec::new();
    // 商品名 -> id：同一个商品在表里出现多行，商品只写一次
    let mut seen: HashMap<String, i64> = HashMap::new();

    for (i, row) in range.rows().skip(1).enumerate() {
        let line = i + 2; // Excel 行号（表头是第 1 行）
        let cells = row_cells(row, HEADERS.len());
        let name = cells[0].clone();
        if name.is_empty() {
            continue;
        }
        let conn = state.conn.get().unwrap();

        // ---- 商品：同名只处理第一行 ----
        let pid = match seen.get(&name) {
            Some(id) => *id,
            None => {
                let existing = db::product_by_name(&conn, &name).ok().flatten();
                let id = existing.as_ref().map(|p| p.id);
                // 图片沿用原值（zip 传图时再覆盖），二次导入不会把图冲掉
                let image = existing.as_ref().map(|p| p.image.clone()).unwrap_or_default();
                let forms_json =
                    serde_json::to_string(&split_multi(&cells[3])).unwrap_or_else(|_| "[]".into());
                let funcs_json =
                    serde_json::to_string(&split_multi(&cells[4])).unwrap_or_else(|_| "[]".into());
                // 上架时间：表里填了就按填的；空着就沿用原值（新商品留空）。
                // 导入不该因为漏一列就把已有的上架时间冲成空。
                let listed_at = match products::clean_date(&cells[7]) {
                    Some(v) if !v.is_empty() => v,
                    _ => existing.as_ref().map(|p| p.listed_at.clone()).unwrap_or_default(),
                };
                let new = db::NewProduct {
                    name: &name,
                    category: &cells[1],
                    class_name: &cells[2],
                    forms_json: &forms_json,
                    funcs_json: &funcs_json,
                    usage_text: &cells[5],
                    image: &image,
                    listed_at: &listed_at,
                };
                match db::product_save(&conn, id, &new) {
                    Ok(v) => {
                        if id.is_some() {
                            updated += 1;
                        } else {
                            created += 1;
                        }
                        seen.insert(name.clone(), v);
                        // 图片文件名列：填了就按它设图片列表（只采用磁盘上真有的 hash），
                        // 留空则沿用原来的图，二次导入不会把图片冲掉。
                        // 后面如果还传了图片 zip，会再整组覆盖一次。
                        let wanted = split_image_names(&cells[6]);
                        if !wanted.is_empty() {
                            let usable: Vec<String> = wanted
                                .into_iter()
                                .filter(|h| products::has_image(state, h))
                                .collect();
                            if !usable.is_empty() {
                                let _ = db::product_set_images(&conn, v, &usable);
                                img_from_sheet += 1;
                            }
                        }
                        v
                    }
                    Err(e) => {
                        warnings.push(format!("第 {line} 行：商品「{name}」写入失败：{e}"));
                        continue;
                    }
                }
            }
        };

        // ---- 规格：69 码选填 ----
        // 有码按 69 码认人（全局唯一），没码按「数值 + 单位 + 包装」认人，
        // 否则每次导入都会给空码行新建一条，导出→导入闭环会越滚越多。
        let barcode = cells[11].clone();
        let value = cells[8].parse::<f64>().unwrap_or(0.0);
        let unit = cells[9].clone();
        let pack = cells[10].clone();
        let price = cells[12].clone();
        let promo = cells[13].clone();
        // 规格列全空 = 这一行只写了商品（没有规格的商品导出时就是这种行），不建规格
        let spec_blank = value == 0.0
            && unit.is_empty()
            && pack.is_empty()
            && barcode.is_empty()
            && price.is_empty()
            && promo.is_empty();
        if spec_blank {
            continue;
        }
        let Some(price_cents) = products::price_to_cents(&price) else {
            warnings.push(format!("第 {line} 行：售价格式不对（{}）", price));
            continue;
        };
        let existing = if barcode.is_empty() {
            db::sku_find_blank_by_spec(&conn, pid, value, &unit, &pack)
                .ok()
                .flatten()
        } else {
            db::sku_by_barcode(&conn, &barcode).ok().flatten()
        };
        let sku_id = existing.as_ref().map(|s| s.id);
        if let Some(s) = &existing {
            if s.product_id != pid {
                warnings.push(format!("第 {line} 行：69 码 {barcode} 已经挂在别的商品上，跳过"));
                continue;
            }
        }
        let label = if barcode.is_empty() {
            format!("第 {line} 行规格")
        } else {
            format!("69 码 {barcode}")
        };
        let new = db::NewSku {
            product_id: pid,
            spec_value: value,
            spec_unit: &unit,
            spec_pack: &pack,
            barcode69: &barcode,
            price_cents,
            promo: &promo,
            sort_order: i as i64,
        };
        match db::sku_save(&conn, sku_id, &new) {
            Ok(_) => {
                if sku_id.is_some() {
                    sku_updated += 1;
                } else {
                    sku_created += 1;
                }
            }
            Err(e) => warnings.push(format!("{} 写入失败：{e}", label)),
        }
    }

    // ---- 图片 zip：按商品分组，一个商品最多 5 张，整组替换 ----
    // 三种命名都认：
    //   商品名.jpg          主图
    //   商品名-2.jpg        副图（-数字后缀只用来排序，不参与商品名匹配）
    //   商品名/01.jpg       目录形式，目录名就是商品名
    // 商品名 / 文件名也都可以用 69 码。
    let mut img_ok = 0usize;
    let mut img_products = 0usize;
    let mut img_miss: Vec<String> = Vec::new();
    let mut img_fail: Vec<String> = Vec::new();
    if let Some(zb) = zip_bytes {
        let mut archive =
            zip::ZipArchive::new(Cursor::new(zb)).map_err(|e| format!("图片 zip 打不开：{e}"))?;
        // pid -> (排序键, 原始文件名, 字节)
        let mut groups: HashMap<i64, Vec<(String, String, Vec<u8>)>> = HashMap::new();
        for i in 0..archive.len() {
            let Ok(mut file) = archive.by_index(i) else { continue };
            if file.is_dir() {
                continue;
            }
            let raw_name = file.name().to_string();
            if raw_name.starts_with("__MACOSX/") || raw_name.starts_with('.') {
                continue;
            }
            let mut data = Vec::new();
            if std::io::Read::read_to_end(&mut file, &mut data).is_err() || data.is_empty() {
                continue;
            }
            let (stem, order) = match raw_name.split_once('/') {
                Some((dir, rest)) => (dir.to_string(), rest.to_string()),
                None => {
                    let s = raw_name
                        .rsplit_once('.')
                        .map(|(a, _)| a.to_string())
                        .unwrap_or_else(|| raw_name.clone());
                    (s.clone(), s)
                }
            };
            let key = match stem.rsplit_once('-') {
                Some((base, n)) if !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) => {
                    base.to_string()
                }
                _ => stem,
            };
            let conn = state.conn.get().unwrap();
            let pid = seen.get(&key).copied().or_else(|| {
                db::product_by_name(&conn, &key).ok().flatten().map(|p| p.id)
            });
            let pid = match pid {
                Some(v) => v,
                None => match db::sku_by_barcode(&conn, &key).ok().flatten() {
                    Some(s) => s.product_id,
                    None => {
                        img_miss.push(raw_name);
                        continue;
                    }
                },
            };
            groups.entry(pid).or_default().push((order, raw_name, data));
        }

        for (pid, mut items) in groups {
            // 目录里用 01.jpg / 02.jpg 命名就能保证顺序，-2/-3 后缀同理
            items.sort_by(|a, b| a.0.cmp(&b.0));
            let mut hashes = Vec::new();
            for (_, raw, data) in items.into_iter().take(12) {
                match products::store_image(state, &data) {
                    Ok(h) => hashes.push(h),
                    Err(e) => img_fail.push(format!("{raw}（{e}）")),
                }
            }
            if !hashes.is_empty() {
                // normalize_images 负责去重 + 截到 5 张
                let final_list = products::normalize_images(&hashes);
                let conn = state.conn.get().unwrap();
                let _ = db::product_set_images(&conn, pid, &final_list);
                img_ok += final_list.len();
                img_products += 1;
            }
        }
    }

    let mut msg = format!(
        "商品 新增 {created} / 更新 {updated}；规格 新增 {sku_created} / 更新 {sku_updated}"
    );
    if img_ok > 0 {
        msg.push_str(&format!("；图片 {img_ok} 张（{img_products} 个商品）"));
    }
    if img_from_sheet > 0 {
        msg.push_str(&format!("；按表格图片列设置了 {img_from_sheet} 个商品"));
    }
    if !warnings.is_empty() || !img_miss.is_empty() || !img_fail.is_empty() {
        let mut tail: Vec<String> = warnings;
        if !img_miss.is_empty() {
            tail.push(format!(
                "{} 张图没匹配上商品：{}",
                img_miss.len(),
                img_miss.iter().take(3).cloned().collect::<Vec<_>>().join("、")
            ));
        }
        if !img_fail.is_empty() {
            tail.push(format!(
                "{} 张图处理失败：{}",
                img_fail.len(),
                img_fail.iter().take(3).cloned().collect::<Vec<_>>().join("、")
            ));
        }
        msg.push_str(&format!(
            "。要注意：{}",
            tail.iter().take(4).cloned().collect::<Vec<_>>().join("；")
        ));
    }
    Ok(msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn h(c: char) -> String {
        c.to_string().repeat(16)
    }

    #[test]
    fn 图片文件名列支持多张分号分隔() {
        assert_eq!(
            split_image_names(&format!("{}.jpg;{}.jpg", h('a'), h('b'))),
            vec![h('a'), h('b')]
        );
        assert_eq!(split_image_names(&format!("{}.JPG", h('a'))), vec![h('a')]);
        assert_eq!(
            split_image_names(&format!("{}.jpg、{}.jpg", h('a'), h('a'))),
            vec![h('a')],
            "重复图只留一张"
        );
        assert_eq!(split_image_names("正面.jpg"), Vec::<String>::new(), "手填名字不算图");
        assert_eq!(split_image_names(""), Vec::<String>::new());
    }

    #[test]
    fn 导出把多张图写进同一列() {
        use calamine::{open_workbook_from_rs, Reader, Xlsx};
        let conn = Connection::open_in_memory().unwrap();
        db::init_products(&conn).unwrap();
        let pid = db::product_save(
            &conn,
            None,
            &db::NewProduct {
                name: "多图商品",
                category: "园艺",
                class_name: "园艺肥料",
                forms_json: "[]",
                funcs_json: "[]",
                usage_text: "",
                image: "",
                listed_at: "",
            },
        )
        .unwrap();
        db::product_set_images(&conn, pid, &[h('a'), h('b')]).unwrap();

        let products = db::products(&conn, false).unwrap();
        let bytes = build_export(&products, &[]).unwrap();
        let mut wb: Xlsx<_> = open_workbook_from_rs(Cursor::new(bytes)).unwrap();
        let range = wb.worksheet_range(SHEET).unwrap();
        let cell = range.get((1, 6)).unwrap(); // 第 2 行 · 第 7 列「图片文件名」
        assert_eq!(
            cell.as_string().unwrap_or_default(),
            format!("{}.jpg;{}.jpg", h('a'), h('b'))
        );
    }
}
