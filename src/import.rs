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
const HEADERS: [&str; 13] = [
    "商品名", "类目", "类别", "剂型", "功能", "使用方法", "图片文件名",
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
        let image_name = if p.image.is_empty() {
            String::new()
        } else {
            format!("{}.jpg", p.image)
        };
        let own: Vec<&db::SkuRow> = skus.iter().filter(|s| s.product_id == p.id).collect();

        let mut write_row = |spec: Option<&db::SkuRow>| -> Result<(), String> {
            let cells: [String; 13] = [
                p.name.clone(),
                p.category.clone(),
                p.class_name.clone(),
                forms.join("、"),
                funcs.join("、"),
                p.usage_text.clone(),
                image_name.clone(),
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
                let new = db::NewProduct {
                    name: &name,
                    category: &cells[1],
                    class_name: &cells[2],
                    forms_json: &forms_json,
                    funcs_json: &funcs_json,
                    usage_text: &cells[5],
                    image: &image,
                };
                match db::product_save(&conn, id, &new) {
                    Ok(v) => {
                        if id.is_some() {
                            updated += 1;
                        } else {
                            created += 1;
                        }
                        seen.insert(name.clone(), v);
                        v
                    }
                    Err(e) => {
                        warnings.push(format!("第 {line} 行：商品「{name}」写入失败：{e}"));
                        continue;
                    }
                }
            }
        };

        // ---- 规格：69 码是主键，有则更新无则新建 ----
        let barcode = cells[10].clone();
        if barcode.is_empty() {
            // 这一行只写了商品没写规格：允许（商品可以先没有规格）
            continue;
        }
        let Some(price_cents) = products::price_to_cents(&cells[11]) else {
            warnings.push(format!("第 {line} 行：售价格式不对（{}）", cells[11]));
            continue;
        };
        let existing = db::sku_by_barcode(&conn, &barcode).ok().flatten();
        let sku_id = existing.as_ref().map(|s| s.id);
        if let Some(s) = &existing {
            if s.product_id != pid {
                warnings.push(format!("第 {line} 行：69 码 {barcode} 已经挂在别的商品上，跳过"));
                continue;
            }
        }
        let new = db::NewSku {
            product_id: pid,
            spec_value: cells[7].parse::<f64>().unwrap_or(0.0),
            spec_unit: &cells[8],
            spec_pack: &cells[9],
            barcode69: &barcode,
            price_cents,
            promo: &cells[12],
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
            Err(e) => warnings.push(format!("第 {line} 行：69 码 {barcode} 写入失败：{e}")),
        }
    }

    // ---- 图片 zip：文件名按商品名或 69 码匹配 ----
    let mut img_ok = 0usize;
    let mut img_miss: Vec<String> = Vec::new();
    if let Some(zb) = zip_bytes {
        let mut archive =
            zip::ZipArchive::new(Cursor::new(zb)).map_err(|e| format!("图片 zip 打不开：{e}"))?;
        for i in 0..archive.len() {
            let Ok(mut file) = archive.by_index(i) else { continue };
            if file.is_dir() {
                continue;
            }
            let raw_name = file.name().to_string();
            let stem = raw_name
                .rsplit('/')
                .next()
                .unwrap_or(&raw_name)
                .rsplit_once('.')
                .map(|(a, _)| a.to_string())
                .unwrap_or_else(|| raw_name.clone());
            let mut data = Vec::new();
            if std::io::Read::read_to_end(&mut file, &mut data).is_err() || data.is_empty() {
                continue;
            }
            let conn = state.conn.get().unwrap();
            let pid = seen.get(&stem).copied().or_else(|| {
                db::product_by_name(&conn, &stem).ok().flatten().map(|p| p.id)
            });
            let pid = match pid {
                Some(v) => v,
                None => match db::sku_by_barcode(&conn, &stem).ok().flatten() {
                    Some(s) => s.product_id,
                    None => {
                        img_miss.push(raw_name);
                        continue;
                    }
                },
            };
            match products::store_image(state, &data) {
                Ok(hash) => {
                    let _ = db::product_set_image(&conn, pid, &hash);
                    img_ok += 1;
                }
                Err(e) => img_miss.push(format!("{raw_name}（{e}）")),
            }
        }
    }

    let mut msg = format!(
        "商品 新增 {created} / 更新 {updated}；规格 新增 {sku_created} / 更新 {sku_updated}"
    );
    if img_ok > 0 {
        msg.push_str(&format!("；图片 {img_ok} 张"));
    }
    if !warnings.is_empty() || !img_miss.is_empty() {
        let mut tail: Vec<String> = warnings;
        if !img_miss.is_empty() {
            tail.push(format!(
                "{} 张图没匹配上商品：{}",
                img_miss.len(),
                img_miss.iter().take(3).cloned().collect::<Vec<_>>().join("、")
            ));
        }
        msg.push_str(&format!(
            "。要注意：{}",
            tail.iter().take(4).cloned().collect::<Vec<_>>().join("；")
        ));
    }
    Ok(msg)
}
