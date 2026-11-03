//! 商品库的批量导入 / 导出 / 模板下载。
//!
//! 为什么用 xlsx 而不是 CSV：Excel 另存 CSV 在中文 Windows 上是 GBK，
//! 传到服务端必然乱码；xlsx 是 UTF-8 且能带多 sheet。
//!
//! 两个 sheet 靠**商品名**关联（69 码只属于规格，用它关联商品容易一码错全错）：
//!   「商品」：商品名 | 类目 | 类别 | 剂型 | 功能 | 使用方法 | 图片文件名
//!   「规格」：商品名 | 数值 | 单位 | 包装 | 69码 | 售价(元) | 促销

use std::collections::HashMap;
use std::io::Cursor;

use axum::extract::{Multipart, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};

use calamine::DataType;

use crate::{db, products, AppState};

const SHEET_PRODUCTS: &str = "商品";
const SHEET_SKUS: &str = "规格";

const PRODUCT_HEADERS: [&str; 7] = [
    "商品名", "类目", "类别", "剂型", "功能", "使用方法", "图片文件名",
];
const SKU_HEADERS: [&str; 7] = [
    "商品名", "数值", "单位", "包装", "69码", "售价(元)", "促销",
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

/// 模板：只有表头 + 两行示例，避免空表被 Excel 当成没有内容
pub async fn template(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !crate::is_admin(&headers, &state) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    match build_template() {
        Ok(bytes) => xlsx_response(bytes, "商品导入模板.xlsx"),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

fn build_template() -> Result<Vec<u8>, String> {
    use rust_xlsxwriter::{Format, Workbook};
    let mut wb = Workbook::new();
    let bold = Format::new().set_bold();

    let sheet = wb.add_worksheet();
    sheet.set_name(SHEET_PRODUCTS).map_err(|e| e.to_string())?;
    for (i, h) in PRODUCT_HEADERS.iter().enumerate() {
        sheet.write_with_format(0, i as u16, *h, &bold).map_err(|e| e.to_string())?;
    }
    let example = ["贝可施花卉营养液", "园艺", "园艺肥料", "液体", "绿叶肥、生长肥", "稀释 1000 倍灌根…", ""];
    for (i, v) in example.iter().enumerate() {
        sheet.write(1, i as u16, *v).map_err(|e| e.to_string())?;
    }
    sheet.set_column_width(0, 24).ok();
    sheet.set_column_width(5, 40).ok();

    let sheet = wb.add_worksheet();
    sheet.set_name(SHEET_SKUS).map_err(|e| e.to_string())?;
    for (i, h) in SKU_HEADERS.iter().enumerate() {
        sheet.write_with_format(0, i as u16, *h, &bold).map_err(|e| e.to_string())?;
    }
    let example = ["贝可施花卉营养液", "500", "ml", "瓶", "6931234567890", "39.9", "买2送1"];
    for (i, v) in example.iter().enumerate() {
        sheet.write(1, i as u16, *v).map_err(|e| e.to_string())?;
    }
    sheet.set_column_width(0, 24).ok();
    sheet.set_column_width(4, 18).ok();

    wb.save_to_buffer().map_err(|e| e.to_string())
}

/// 导出：把库里现有的商品和规格原样导成同样的两张表（备份 + 手工改价都靠它）
pub async fn export(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !crate::is_admin(&headers, &state) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let (products, skus) = {
        let conn = state.conn.get().unwrap();
        (db::products(&conn, false).unwrap_or_default(), {
            let mut all = Vec::new();
            for p in db::products(&conn, false).unwrap_or_default() {
                all.extend(db::skus_of(&conn, p.id).unwrap_or_default());
            }
            all
        })
    };
    match build_export(&products, &skus) {
        Ok(bytes) => xlsx_response(bytes, "商品库导出.xlsx"),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

fn build_export(products: &[db::ProductRow], skus: &[db::SkuRow]) -> Result<Vec<u8>, String> {
    use rust_xlsxwriter::{Format, Workbook};
    let mut wb = Workbook::new();
    let bold = Format::new().set_bold();

    let sheet = wb.add_worksheet();
    sheet.set_name(SHEET_PRODUCTS).map_err(|e| e.to_string())?;
    for (i, h) in PRODUCT_HEADERS.iter().enumerate() {
        sheet.write_with_format(0, i as u16, *h, &bold).map_err(|e| e.to_string())?;
    }
    for (r, p) in products.iter().enumerate() {
        let row = r as u32 + 1;
        let forms: Vec<String> = serde_json::from_str(&p.forms_json).unwrap_or_default();
        let funcs: Vec<String> = serde_json::from_str(&p.funcs_json).unwrap_or_default();
        let image_name = if p.image.is_empty() {
            String::new()
        } else {
            format!("{}.jpg", p.image)
        };
        let cells = [
            p.name.clone(),
            p.category.clone(),
            p.class_name.clone(),
            forms.join("、"),
            funcs.join("、"),
            p.usage_text.clone(),
            image_name,
        ];
        for (i, v) in cells.iter().enumerate() {
            sheet.write(row, i as u16, v.as_str()).map_err(|e| e.to_string())?;
        }
    }
    sheet.set_column_width(0, 24).ok();
    sheet.set_column_width(5, 40).ok();

    let sheet = wb.add_worksheet();
    sheet.set_name(SHEET_SKUS).map_err(|e| e.to_string())?;
    for (i, h) in SKU_HEADERS.iter().enumerate() {
        sheet.write_with_format(0, i as u16, *h, &bold).map_err(|e| e.to_string())?;
    }
    let names: HashMap<i64, &str> = products.iter().map(|p| (p.id, p.name.as_str())).collect();
    for (r, s) in skus.iter().enumerate() {
        let row = r as u32 + 1;
        let cells = [
            names.get(&s.product_id).copied().unwrap_or("").to_string(),
            if s.spec_value > 0.0 { format!("{}", s.spec_value) } else { String::new() },
            s.spec_unit.clone(),
            s.spec_pack.clone(),
            s.barcode69.clone(),
            format!("{}.{:02}", s.price_cents / 100, s.price_cents % 100),
            s.promo.clone(),
        ];
        for (i, v) in cells.iter().enumerate() {
            sheet.write(row, i as u16, v.as_str()).map_err(|e| e.to_string())?;
        }
    }
    sheet.set_column_width(0, 24).ok();
    sheet.set_column_width(4, 18).ok();

    wb.save_to_buffer().map_err(|e| e.to_string())
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

/// 一行里的单元格按列序取出（缺列按空串，数字转成文本）
fn row_cells(row: &[calamine::Data], n: usize) -> Vec<String> {
    (0..n)
        .map(|i| {
            row.get(i)
                .map(|c| match c {
                    calamine::Data::Int(v) => v.to_string(),
                    calamine::Data::Float(v) => {
                        if (v - v.round()).abs() < 1e-9 {
                            format!("{}", v.round() as i64)
                        } else {
                            v.to_string()
                        }
                    }
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

    // 解析 + 写库都在阻塞线程里做：几百行 xlsx 解析不该占着异步线程
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

    let mut created = 0usize;
    let mut updated = 0usize;
    let mut sku_created = 0usize;
    let mut sku_updated = 0usize;
    let mut warnings: Vec<String> = Vec::new();

    // 商品名 -> id（导入过程里新建的商品也能马上被规格引用）
    let mut known: HashMap<String, i64> = HashMap::new();
    if let Ok(range) = wb.worksheet_range(SHEET_PRODUCTS) {
        for row in range.rows().skip(1) {
            let cells = row_cells(row, PRODUCT_HEADERS.len());
            let name = cells[0].clone();
            if name.is_empty() {
                continue;
            }
            let forms = split_multi(&cells[3]);
            let funcs = split_multi(&cells[4]);
            let conn = state.conn.get().unwrap();
            let existing = db::product_by_name(&conn, &name).ok().flatten();
            let id = existing.as_ref().map(|p| p.id);
            let forms_json = serde_json::to_string(&forms).unwrap_or_else(|_| "[]".into());
            let funcs_json = serde_json::to_string(&funcs).unwrap_or_else(|_| "[]".into());
            // 图片这一列在导入时先保留原值（稍后按 zip 覆盖），避免二次导入把图冲掉
            let image = existing
                .as_ref()
                .map(|p| p.image.clone())
                .unwrap_or_default();
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
                Ok(pid) => {
                    known.insert(name.clone(), pid);
                    if id.is_some() {
                        updated += 1;
                    } else {
                        created += 1;
                    }
                }
                Err(e) => warnings.push(format!("商品「{name}」写入失败：{e}")),
            }
        }
    } else {
        warnings.push(format!("没找到「{SHEET_PRODUCTS}」这个 sheet"));
    }

    // 规格：69 码是主键，已存在就更新（找到它属于哪个商品）
    if let Ok(range) = wb.worksheet_range(SHEET_SKUS) {
        for (i, row) in range.rows().skip(1).enumerate() {
            let cells = row_cells(row, SKU_HEADERS.len());
            let name = cells[0].clone();
            let barcode = cells[4].clone();
            if name.is_empty() && barcode.is_empty() {
                continue;
            }
            let conn = state.conn.get().unwrap();
            let Some(pid) = known
                .get(&name)
                .copied()
                .or_else(|| db::product_by_name(&conn, &name).ok().flatten().map(|p| p.id))
            else {
                warnings.push(format!("第 {} 行：商品名「{name}」不存在，跳过", i + 2));
                continue;
            };
            if barcode.is_empty() {
                warnings.push(format!("第 {} 行：没有 69 码，跳过", i + 2));
                continue;
            }
            let Some(price_cents) = products::price_to_cents(&cells[5]) else {
                warnings.push(format!("第 {} 行：售价格式不对（{}）", i + 2, cells[5]));
                continue;
            };
            let existing = db::sku_by_barcode(&conn, &barcode).ok().flatten();
            let sku_id = existing.as_ref().map(|s| s.id);
            if let Some(s) = &existing {
                if s.product_id != pid {
                    warnings.push(format!(
                        "69 码 {barcode} 已经挂在别的商品上（id {}），跳过",
                        s.product_id
                    ));
                    continue;
                }
            }
            let new = db::NewSku {
                product_id: pid,
                spec_value: cells[1].parse::<f64>().unwrap_or(0.0),
                spec_unit: &cells[2],
                spec_pack: &cells[3],
                barcode69: &barcode,
                price_cents,
                promo: &cells[6],
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
                Err(e) => warnings.push(format!("69 码 {barcode} 写入失败：{e}")),
            }
        }
    } else {
        warnings.push(format!("没找到「{SHEET_SKUS}」这个 sheet"));
    }

    // 图片：zip 里每个文件按「文件名去扩展名」找商品（先按商品名，再按 69 码）
    let mut img_ok = 0usize;
    let mut img_miss: Vec<String> = Vec::new();
    if let Some(zb) = zip_bytes {
        let mut archive = zip::ZipArchive::new(Cursor::new(zb))
            .map_err(|e| format!("图片 zip 打不开：{e}"))?;
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
            let pid = known.get(&stem).copied().or_else(|| {
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
            tail.push(format!("{} 张图没匹配上商品：{}", img_miss.len(), img_miss.iter().take(3).cloned().collect::<Vec<_>>().join("、")));
        }
        msg.push_str(&format!("。要注意：{}", tail.iter().take(4).cloned().collect::<Vec<_>>().join("；")));
    }
    Ok(msg)
}
