//! 提交记录导出 xlsx。
//!
//! 两条规矩（都是踩过坑才写的）：
//!   1. **一次只导一个模板**，一个模板一个 sheet。跨模板并排导，列会稀疏得没法看。
//!   2. **列按提交自带的 schema 快照取**，同一模板多个版本混合时取并集、缺的留空 ——
//!      模板改过以后，用当前 schema 去导历史记录一定会串列。

use std::collections::HashMap;

use rust_xlsxwriter::{Format, FormatAlign, Workbook};

use crate::db::FormSubmission;
use crate::forms::Schema;

const SYSTEM_COLS: [&str; 3] = ["提交人", "填写时间", "接收时间"];

/// 表头顺序 + 每条记录该填什么，纯逻辑部分单独拆出来，方便单测。
pub struct Table {
    pub headers: Vec<String>,
    /// 与 `headers` 前 N 列一一对应（最后 3 列是系统列）。单测用它断言列顺序。
    #[allow(dead_code)]
    pub keys: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

/// 把一批提交摊平成表格。`tz_offset_min` 用于时间列。
pub fn build_table(rows: &[FormSubmission], tz_offset_min: i32) -> Table {
    // 列并集：按"第一次出现"的顺序拼，label 以最新出现的为准
    let mut keys: Vec<String> = Vec::new();
    let mut labels: HashMap<String, String> = HashMap::new();

    for row in rows {
        if let Ok(schema) = serde_json::from_str::<Schema>(&row.template_snapshot) {
            for f in schema.fields {
                if !labels.contains_key(&f.key) {
                    keys.push(f.key.clone());
                }
                labels.insert(f.key.clone(), f.label);
            }
        }
    }

    let mut headers: Vec<String> =
        keys.iter().map(|k| labels.get(k).cloned().unwrap_or_else(|| k.clone())).collect();
    headers.extend(SYSTEM_COLS.iter().map(|s| s.to_string()));

    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let data: HashMap<String, String> = serde_json::from_str::<serde_json::Value>(&row.data_json)
            .ok()
            .and_then(|v| v.as_object().cloned())
            .map(|m| m.into_iter().map(|(k, v)| (k, value_text(&v))).collect())
            .unwrap_or_default();

        let mut line: Vec<String> = keys.iter().map(|k| data.get(k).cloned().unwrap_or_default()).collect();
        line.push(row.user_name.clone());
        line.push(short_time(row.client_time, tz_offset_min));
        line.push(short_time(row.created_at, tz_offset_min));
        out.push(line);
    }

    Table { headers, keys, rows: out }
}

fn value_text(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Null => String::new(),
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
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
    let mut book = Workbook::new();
    let sheet = book.add_worksheet();
    sheet.set_name(safe_sheet_name(sheet_name)).map_err(|e| e.to_string())?;

    let head = Format::new().set_bold().set_align(FormatAlign::Center).set_text_wrap();
    for (c, h) in table.headers.iter().enumerate() {
        sheet
            .write_string_with_format(0, c as u16, h, &head)
            .map_err(|e| e.to_string())?;
    }

    // 每一列按内容判断是不是"纯数字列"
    let numeric: Vec<bool> = (0..table.headers.len())
        .map(|c| {
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

    for (ri, row) in table.rows.iter().enumerate() {
        let r = (ri + 1) as u32;
        for (ci, cell) in row.iter().enumerate() {
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
        // 时间按 +8 输出
        assert!(t.rows[0][3].starts_with("2025-"));
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
        assert_eq!(t.rows[1][1], ""); // 老版本没有 b 列，留空
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
}
