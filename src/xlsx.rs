//! 提交记录导出 xlsx。
//!
//! 两条规矩（都是踩过坑才写的）：
//!   1. **一次只导一个模板**，一个模板一个 sheet。跨模板并排导，列会稀疏得没法看。
//!   2. **列按提交自带的 schema 快照取**，同一模板多个版本混合时取并集、缺的留空 ——
//!      模板改过以后，用当前 schema 去导历史记录一定会串列。
//!
//! 物流快递字段：一个订单可以有多个单号。导出时**拆成两列**（单号 / 快递公司），
//! 订单按单号数**展开成多行**；其余「订单级」列（普通字段 + 系统列）跨这些行**垂直合并**。
//! 单号列和快递公司列**不合并**（每个单号可能不同快递）。

use std::collections::{HashMap, HashSet};

use rust_xlsxwriter::{Format, FormatAlign, Workbook};

use crate::db::FormSubmission;
use crate::forms::{express_items, Schema};

const SYSTEM_COLS: [&str; 3] = ["提交人", "填写时间", "接收时间"];
/// 快递字段拆成两列时，公司列标题的后缀
const CARRIER_SUFFIX: &str = "（快递公司）";

/// 一列从哪个字段、取哪一部分。
#[derive(Clone, Copy, PartialEq)]
enum Part {
    /// 普通字段（订单级，可垂直合并）
    Value,
    /// 快递字段拆出的「单号」列（每个单号一行，不合并）
    ExpressNumber,
    /// 快递字段拆出的「快递公司」列（每个单号一行，不合并）
    ExpressCarrier,
}

struct Col {
    key: String,
    header: String,
    part: Part,
}

/// 垂直合并区间（数据行下标，0 基、不含表头）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Merge {
    pub first_row: usize,
    pub last_row: usize,
    pub col: usize,
}

/// 表头顺序 + 每条记录该填什么，纯逻辑部分单独拆出来，方便单测。
pub struct Table {
    pub headers: Vec<String>,
    /// 与 `headers` 前 N 列一一对应（最后 3 列是系统列）。单测用它断言列顺序。
    #[allow(dead_code)]
    pub keys: Vec<String>,
    pub rows: Vec<Vec<String>>,
    /// 订单级列的垂直合并（一个订单 N 个单号 → 这些列跨 N 行合并）
    pub merges: Vec<Merge>,
    /// 必须按**文本**写的列（单号/快递公司）：否则纯数字单号会被写成数字，
    /// 前导零丢失、长单号变成科学计数。
    pub force_text: Vec<usize>,
}

/// 列并集：按"第一次出现"的顺序拼，label 以最新出现的为准。
fn build_columns(rows: &[FormSubmission]) -> Vec<Col> {
    let mut cols: Vec<Col> = Vec::new();
    let mut idx: HashMap<(String, u8), usize> = HashMap::new();

    for row in rows {
        let Ok(schema) = serde_json::from_str::<Schema>(&row.template_snapshot) else {
            continue;
        };
        for f in schema.fields {
            if f.kind == "express" {
                if let Some(&i) = idx.get(&(f.key.clone(), 1)) {
                    cols[i].header = f.label.clone();
                    if let Some(&j) = idx.get(&(f.key.clone(), 2)) {
                        cols[j].header = format!("{}{CARRIER_SUFFIX}", f.label);
                    }
                } else {
                    idx.insert((f.key.clone(), 1), cols.len());
                    cols.push(Col { key: f.key.clone(), header: f.label.clone(), part: Part::ExpressNumber });
                    idx.insert((f.key.clone(), 2), cols.len());
                    cols.push(Col {
                        key: f.key.clone(),
                        header: format!("{}{CARRIER_SUFFIX}", f.label),
                        part: Part::ExpressCarrier,
                    });
                }
            } else if let Some(&i) = idx.get(&(f.key.clone(), 0)) {
                cols[i].header = f.label.clone();
            } else {
                idx.insert((f.key.clone(), 0), cols.len());
                cols.push(Col { key: f.key.clone(), header: f.label.clone(), part: Part::Value });
            }
        }
    }
    cols
}

/// 把一批提交摊平成表格。`tz_offset_min` 用于时间列。
pub fn build_table(rows: &[FormSubmission], tz_offset_min: i32) -> Table {
    let cols = build_columns(rows);

    let mut headers: Vec<String> = cols.iter().map(|c| c.header.clone()).collect();
    headers.extend(SYSTEM_COLS.iter().map(|s| s.to_string()));

    let mut out: Vec<Vec<String>> = Vec::new();
    let mut merges: Vec<Merge> = Vec::new();
    let ncols = cols.len();

    for row in rows {
        let data: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str::<serde_json::Value>(&row.data_json)
                .ok()
                .and_then(|v| v.as_object().cloned())
                .unwrap_or_default();

        // 这条登记的每个快递字段拆成条目；订单按最多的那个展开成 max_n 行
        let mut items_map: HashMap<String, Vec<(String, String)>> = HashMap::new();
        let mut max_n = 1usize;
        if let Ok(schema) = serde_json::from_str::<Schema>(&row.template_snapshot) {
            for f in &schema.fields {
                if f.kind == "express" {
                    let items: Vec<(String, String)> = express_items(data.get(&f.key))
                        .into_iter()
                        .filter(|(n, c)| !n.is_empty() || !c.is_empty())
                        .collect();
                    max_n = max_n.max(items.len());
                    items_map.insert(f.key.clone(), items);
                }
            }
        }

        let base = out.len();
        for r in 0..max_n {
            let mut line: Vec<String> = Vec::with_capacity(ncols + SYSTEM_COLS.len());
            for c in &cols {
                let cell = match c.part {
                    Part::Value => {
                        if r == 0 {
                            data.get(&c.key).map(value_text).unwrap_or_default()
                        } else {
                            // 非首行留空，值在左上角（合并后显示一次）
                            String::new()
                        }
                    }
                    Part::ExpressNumber => {
                        items_map.get(&c.key).and_then(|v| v.get(r)).map(|(n, _)| n.clone()).unwrap_or_default()
                    }
                    Part::ExpressCarrier => {
                        items_map.get(&c.key).and_then(|v| v.get(r)).map(|(_, c)| c.clone()).unwrap_or_default()
                    }
                };
                line.push(cell);
            }
            line.push(if r == 0 { row.user_name.clone() } else { String::new() });
            line.push(if r == 0 { short_time(row.client_time, tz_offset_min) } else { String::new() });
            line.push(if r == 0 { short_time(row.created_at, tz_offset_min) } else { String::new() });
            out.push(line);
        }

        if max_n > 1 {
            let last = base + max_n - 1;
            for (ci, c) in cols.iter().enumerate() {
                if c.part == Part::Value {
                    merges.push(Merge { first_row: base, last_row: last, col: ci });
                }
            }
            for sc in 0..SYSTEM_COLS.len() {
                merges.push(Merge { first_row: base, last_row: last, col: ncols + sc });
            }
        }
    }

    Table {
        headers,
        keys: cols.iter().map(|c| c.key.clone()).collect(),
        force_text: cols.iter().enumerate().filter(|(_, c)| c.part != Part::Value).map(|(i, _)| i).collect(),
        rows: out,
        merges,
    }
}

fn value_text(v: &serde_json::Value) -> String {
    fn one(obj: &serde_json::Value) -> String {
        format!(
            "{} | {}",
            obj.get("tracking_number").and_then(serde_json::Value::as_str).unwrap_or(""),
            obj.get("carrier").and_then(serde_json::Value::as_str).unwrap_or("")
        )
    }
    match v {
        serde_json::Value::Null => String::new(),
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Array(items) => {
            items.iter().map(one).filter(|s| s != " | ").collect::<Vec<_>>().join(" / ")
        }
        serde_json::Value::Object(_) if v.get("tracking_number").is_some() && v.get("carrier").is_some() => one(v),
        other => other.to_string(),
    }
}

fn short_time(ts: i64, tz_offset_min: i32) -> String {
    use time::{OffsetDateTime, UtcOffset};
    let offset = UtcOffset::from_whole_seconds(tz_offset_min.saturating_mul(60))
        .unwrap_or(UtcOffset::UTC);
    match OffsetDateTime::from_unix_timestamp(ts) {
        Ok(t) => {
            let t = t.to_offset(offset);
            format!(
                "{:04}-{:02}-{:02} {:02}:{:02}",
                t.year(),
                t.month() as u8,
                t.day(),
                t.hour(),
                t.minute()
            )
        }
        Err(_) => String::new(),
    }
}

/// 生成 xlsx 字节。数字列如果整列都是数字，会写成数字单元格（Excel 里能直接求和）。
pub fn write_xlsx(table: &Table, sheet_name: &str) -> Result<Vec<u8>, String> {
    write_xlsx_sheets(&[(table, sheet_name.to_string())])
}

pub fn write_xlsx_sheets(sheets: &[(&Table, String)]) -> Result<Vec<u8>, String> {
    let mut book = Workbook::new();
    let mut used_names = HashSet::new();
    for (table, sheet_name) in sheets {
        let sheet = book.add_worksheet();
        let base = safe_sheet_name(sheet_name);
        let mut unique = base.clone();
        let mut suffix = 2;
        while !used_names.insert(unique.clone()) {
            let tail = format!("-{suffix}");
            unique = format!(
                "{}{}",
                base.chars().take(31usize.saturating_sub(tail.len())).collect::<String>(),
                tail
            );
            suffix += 1;
        }
        sheet.set_name(unique).map_err(|e| e.to_string())?;

        let head = Format::new().set_bold().set_align(FormatAlign::Center).set_text_wrap();
        for (c, h) in table.headers.iter().enumerate() {
            sheet
                .write_string_with_format(0, c as u16, h, &head)
                .map_err(|e| e.to_string())?;
        }

        // 每一列按内容判断是不是"纯数字列"（单号/快递公司列强制文本，不当数字）
        let numeric: Vec<bool> = (0..table.headers.len())
            .map(|c| {
                if table.force_text.contains(&c) {
                    return false;
                }
                let mut saw = false;
                for r in &table.rows {
                    let v = r.get(c).map(String::as_str).unwrap_or("");
                    if v.is_empty() {
                        continue;
                    }
                    if v.parse::<f64>().is_err() {
                        return false;
                    }
                    saw = true;
                }
                saw
            })
            .collect();

        // 合并区里只有左上角写值；其它格子留给 merge_range 填空白，避免重复数据
        let mut merged: HashSet<(usize, usize)> = HashSet::new();
        for m in &table.merges {
            for r in m.first_row..=m.last_row {
                merged.insert((r, m.col));
            }
        }

        for (ri, row) in table.rows.iter().enumerate() {
            let r = (ri + 1) as u32;
            for (ci, cell) in row.iter().enumerate() {
                if merged.contains(&(ri, ci)) {
                    continue;
                }
                let c = ci as u16;
                if numeric[ci] {
                    if let Ok(v) = cell.parse::<f64>() {
                        sheet.write_number(r, c, v).map_err(|e| e.to_string())?;
                        continue;
                    }
                }
                sheet.write_string(r, c, cell).map_err(|e| e.to_string())?;
            }
        }

        // 落实合并：先建合并区，再往左上角写值。
        // 这样金额这类数字列合并后仍是**数字单元格**（SUM 能算对），不是文本。
        for m in &table.merges {
            let first = (m.first_row + 1) as u32;
            let last = (m.last_row + 1) as u32;
            let c = m.col as u16;
            sheet
                .merge_range(first, c, last, c, "", &Format::new())
                .map_err(|e| e.to_string())?;
            let val = table
                .rows
                .get(m.first_row)
                .and_then(|r| r.get(m.col))
                .cloned()
                .unwrap_or_default();
            if val.is_empty() {
                continue;
            }
            if numeric[m.col] {
                if let Ok(v) = val.parse::<f64>() {
                    sheet.write_number(first, c, v).map_err(|e| e.to_string())?;
                    continue;
                }
            }
            sheet.write_string(first, c, &val).map_err(|e| e.to_string())?;
        }

        // 列宽：中文按 ~1.8 个字符宽算，封顶 40
        for (c, h) in table.headers.iter().enumerate() {
            let mut widest = h.chars().count() as f64 * 1.8;
            for row in table.rows.iter().take(200) {
                if let Some(v) = row.get(c) {
                    let w = v.chars().count() as f64 * 1.8;
                    if w > widest {
                        widest = w;
                    }
                }
            }
            sheet.set_column_width(c as u16, widest.clamp(6.0, 40.0)).map_err(|e| e.to_string())?;
        }
        sheet.set_freeze_panes(1, 0).map_err(|e| e.to_string())?;
    }

    book.save_to_buffer().map_err(|e| e.to_string())
}

/// Excel 的 sheet 名有长度和字符限制，超了直接报错，所以这里先洗一遍。
fn safe_sheet_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if matches!(c, '\\' | '/' | '?' | '*' | '[' | ']' | ':') { '_' } else { c })
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        "表单".to_string()
    } else {
        trimmed.chars().take(31).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sub(snapshot: &str, data: &str, name: &str) -> FormSubmission {
        FormSubmission {
            template_name: String::new(),
            id: 1,
            submission_id: "abc".into(),
            template_id: 1,
            template_version: 1,
            template_snapshot: snapshot.into(),
            user_id: 1,
            user_name: name.into(),
            data_json: data.into(),
            client_time: 1_764_000_000,
            created_at: 1_764_000_100,
        }
    }

    const S1: &str = r#"{"fields":[
        {"key":"order_no","label":"订单号","type":"text"},
        {"key":"qty","label":"数量","type":"number"}]}"#;

    const S_EXPRESS: &str = r#"{"fields":[
        {"key":"order_no","label":"订单号","type":"text"},
        {"key":"amount","label":"金额","type":"number"},
        {"key":"w","label":"物流快递","type":"express"}]}"#;

    #[test]
    fn headers_follow_schema_then_system_columns() {
        let rows = vec![sub(S1, r#"{"order_no":"A1","qty":3}"#, "张三")];
        let t = build_table(&rows, 480);
        assert_eq!(
            t.headers,
            vec!["订单号".to_string(), "数量".to_string(), "提交人".into(), "填写时间".into(), "接收时间".into()]
        );
        assert_eq!(t.rows[0][0], "A1");
        assert_eq!(t.rows[0][1], "3");
        assert_eq!(t.rows[0][2], "张三");
        assert!(t.rows[0][3].starts_with("2025-"));
        assert!(t.merges.is_empty());
    }

    #[test]
    fn mixed_versions_take_union_and_leave_gaps() {
        let old = r#"{"fields":[{"key":"a","label":"老字段","type":"text"}]}"#;
        let new = r#"{"fields":[
            {"key":"a","label":"老字段","type":"text"},
            {"key":"b","label":"新字段","type":"text"}]}"#;
        let rows = vec![
            sub(new, r#"{"a":"1","b":"2"}"#, "张三"),
            sub(old, r#"{"a":"1"}"#, "李四"),
        ];
        let t = build_table(&rows, 480);
        assert_eq!(t.headers[..2], ["老字段".to_string(), "新字段".to_string()]);
        assert_eq!(t.rows[1][1], "");
    }

    #[test]
    fn 多单号展开成多行_单号与公司各一列_订单列合并() {
        let data = r#"{"order_no":"A1","amount":87.6,"w":[
            {"tracking_number":"73672232232236","carrier":"中通快递"},
            {"tracking_number":"73672232232185","carrier":"中通快递"}]}"#;
        let t = build_table(&[sub(S_EXPRESS, data, "孙振玉")], 480);

        assert_eq!(
            t.headers,
            vec![
                "订单号".to_string(),
                "金额".to_string(),
                "物流快递".to_string(),
                "物流快递（快递公司）".to_string(),
                "提交人".into(),
                "填写时间".into(),
                "接收时间".into(),
            ]
        );
        assert_eq!(t.rows.len(), 2);
        // 首行：订单级值 + 第 1 个单号
        assert_eq!(t.rows[0][0], "A1");
        assert_eq!(t.rows[0][1], "87.6");
        assert_eq!(t.rows[0][2], "73672232232236");
        assert_eq!(t.rows[0][3], "中通快递");
        assert_eq!(t.rows[0][4], "孙振玉");
        // 第二行：订单级列留空（合并），只有第 2 个单号
        assert_eq!(t.rows[1][0], "");
        assert_eq!(t.rows[1][1], "");
        assert_eq!(t.rows[1][2], "73672232232185");
        assert_eq!(t.rows[1][3], "中通快递");
        assert_eq!(t.rows[1][4], "");

        // 合并：订单号(0)、金额(1)、提交人(4)、填写时间(5)、接收时间(6)；单号(2)/公司(3) 不合并
        let cols: Vec<usize> = {
            let mut v: Vec<_> = t.merges.iter().filter(|m| m.first_row == 0 && m.last_row == 1).map(|m| m.col).collect();
            v.sort_unstable();
            v
        };
        assert_eq!(cols, vec![0, 1, 4, 5, 6]);
    }

    #[test]
    fn 旧对象兼容成一行() {
        let data = r#"{"order_no":"A1","w":{"tracking_number":"SF123","carrier":"顺丰快递"}}"#;
        let t = build_table(&[sub(S_EXPRESS, data, "张三")], 480);
        assert_eq!(t.rows.len(), 1);
        assert!(t.merges.is_empty());
        assert_eq!(t.rows[0][2], "SF123");
        assert_eq!(t.rows[0][3], "顺丰快递");
    }

    #[test]
    fn sheet_name_is_sanitised_and_limited() {
        assert_eq!(safe_sheet_name("补发/申请:测试"), "补发_申请_测试");
        assert_eq!(safe_sheet_name("   "), "表单");
        assert_eq!(safe_sheet_name(&"很长".repeat(30)).chars().count(), 31);
    }

    #[test]
    fn xlsx_has_zip_signature() {
        let rows = vec![sub(S1, r#"{"order_no":"A1","qty":3}"#, "张三")];
        let bytes = write_xlsx(&build_table(&rows, 480), "补发申请").unwrap();
        assert!(bytes.starts_with(b"PK"));
        assert!(bytes.len() > 1000);
    }

    #[test]
    fn 多单号带合并也能生成合法xlsx() {
        let data = r#"{"order_no":"A1","amount":87.6,"w":[
            {"tracking_number":"73672232232236","carrier":"中通快递"},
            {"tracking_number":"73672232232185","carrier":"中通快递"}]}"#;
        let bytes = write_xlsx(&build_table(&[sub(S_EXPRESS, data, "孙振玉")], 480), "测试").unwrap();
        assert!(bytes.starts_with(b"PK"));
        assert!(bytes.len() > 1000);
    }
}
