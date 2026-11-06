//! 服务端第一批自动化测试。
//!
//! 为什么现在才补：表单数据是"账"，改模板、重试提交都可能造成**静默**错误
//! —— 串列、重复入库，界面上都看不出来。这几条测的都是那种"错了也不报错"的地方。

use rusqlite::Connection;

use crate::db;
use crate::forms::{validate_data, validate_schema, Schema};
use crate::xlsx;

fn fresh() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    db::init_forms(&conn).unwrap();
    conn
}

fn schema(fields: &str) -> String {
    format!(r#"{{"title":"t","fields":{fields}}}"#)
}

fn parse(s: &str) -> Schema {
    serde_json::from_str(s).unwrap()
}

/* ------------------------------------------------------------ 1. 幂等 */

#[test]
fn 同一_submission_id_只落一条() {
    let conn = fresh();
    let base = db::NewSubmission {
        template_name: "测试模板",
        submission_id: "sid-0001",
        template_id: 1,
        template_version: 1,
        template_snapshot: "{}",
        user_id: 1,
        user_name: "张三",
        data_json: "{}",
        client_time: 1_700_000_000,
        client_id: "pc",
    };
    assert!(db::form_submit_insert(&conn, &base).unwrap(), "第一次应该是新建");
    assert!(
        !db::form_submit_insert(&conn, &base).unwrap(),
        "重试必须返回 duplicate，否则网络一抖就重复入库"
    );

    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM form_submissions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1);
}

/* ------------------------------------------------------------ 2. 快照冻结 */

#[test]
fn 模板升版后旧提交仍带旧快照() {
    let conn = fresh();

    let v1 = schema(r#"[{"key":"a","label":"A","type":"text"}]"#);
    let v2 = schema(r#"[{"key":"a","label":"A","type":"text"},{"key":"b","label":"B","type":"text"}]"#);

    assert!(matches!(
        db::form_template_save(&conn, None, "表单", &v1, None, 0).unwrap(),
        db::TemplateSave::Saved { version: 1, .. }
    ));
    assert!(matches!(
        db::form_template_save(&conn, Some(1), "表单", &v2, Some(1), 0).unwrap(),
        db::TemplateSave::Saved { version: 2, .. }
    ));

    // 按 v1 交一笔（客服离线几天后补交，手上还是旧模板）
    db::form_submit_insert(
        &conn,
        &db::NewSubmission {
            template_name: "测试模板",
            submission_id: "old-0001",
            template_id: 1,
            template_version: 1,
            template_snapshot: &v1,
            user_id: 1,
            user_name: "张三",
            data_json: r#"{"a":"1"}"#,
            client_time: 1_700_000_000,
            client_id: "pc",
        },
    )
    .unwrap();

    let saved: String = conn
        .query_row(
            "SELECT template_snapshot FROM form_submissions WHERE submission_id='old-0001'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(saved, v1, "提交那一刻的字段定义必须原样冻结");
    assert_eq!(
        db::form_template_schema(&conn, 1, 1).unwrap().unwrap(),
        v1,
        "旧版本要能取回来，否则补交的单子没法复验"
    );
    assert_eq!(db::form_template_schema(&conn, 1, 2).unwrap().unwrap(), v2);
}

/* ------------------------------------------------------------ 3. 乐观锁 */

#[test]
fn 版本过期时保存被拒且不动库() {
    let conn = fresh();
    let s = schema(r#"[{"key":"a","label":"A","type":"text"}]"#);
    db::form_template_save(&conn, None, "表单", &s, None, 0).unwrap();
    db::form_template_save(&conn, Some(1), "表单", &s, Some(1), 0).unwrap();

    match db::form_template_save(&conn, Some(1), "表单", &s, Some(1), 0).unwrap() {
        db::TemplateSave::Conflict { current } => assert_eq!(current, 2),
        other => panic!("应该返回冲突，实际 {other:?}"),
    }

    let version: i64 = conn
        .query_row("SELECT version FROM form_templates WHERE id=1", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, 2, "被拒的保存不能改动库里的模板");
}

/* ------------------------------------------------------------ 4. 登录白名单 */

#[test]
fn 不在名单里的人登录失败() {
    let conn = fresh();
    db::form_user_save(&conn, None, "张三", None).unwrap();

    assert!(db::form_user_by_name(&conn, "张三").unwrap().is_some());
    assert!(
        db::form_user_by_name(&conn, "查无此人").unwrap().is_none(),
        "名单外必须查不到，不然人人都能填"
    );

    db::form_user_toggle(&conn, 1, false).unwrap();
    assert!(
        db::form_user_by_name(&conn, "张三").unwrap().is_none(),
        "停用后也要登录失败"
    );
}

/* ------------------------------------------------------------ 5. schema 校验 */

#[test]
fn 坏模板存不进去() {
    let empty = parse(&schema("[]"));
    assert!(validate_schema(&empty).is_err(), "没字段的表单没意义");

    let no_options =
        parse(&schema(r#"[{"key":"a","label":"A","type":"select"}]"#));
    assert!(validate_schema(&no_options).is_err(), "下拉不给选项等于没法填");

    let dup = parse(&schema(
        r#"[{"key":"a","label":"A","type":"text"},{"key":"a","label":"A2","type":"text"}]"#,
    ));
    assert!(validate_schema(&dup).is_err(), "key 重复会让导出串列");

    let good = parse(&schema(
        r#"[{"key":"a","label":"A","type":"text","required":true},{"key":"b","label":"B","type":"select","options":["x","y"]}]"#,
    ));
    assert!(validate_schema(&good).is_ok());
}

#[test]
fn 提交数据按模板复验() {
    let s = parse(&schema(
        r#"[{"key":"a","label":"A","type":"text","required":true},
            {"key":"b","label":"B","type":"select","options":["x","y"]},
            {"key":"n","label":"N","type":"number"}]"#,
    ));

    let ok = serde_json::from_str(r#"{"a":"1","b":"x","n":3}"#).unwrap();
    assert!(validate_data(&s, &ok).is_empty());

    let missing = serde_json::from_str(r#"{"b":"x"}"#).unwrap();
    let errs = validate_data(&s, &missing);
    assert_eq!(errs.get("a").map(|s| s.as_str()), Some("必填"));

    let bad_option = serde_json::from_str(r#"{"a":"1","b":"z"}"#).unwrap();
    assert!(validate_data(&s, &bad_option).contains_key("b"));

    let not_num = serde_json::from_str(r#"{"a":"1","n":"abc"}"#).unwrap();
    assert!(validate_data(&s, &not_num).contains_key("n"));
}

#[test]
fn 姓名类型能存也能填() {
    // 「姓名」是新增的字段类型：客户端会自动填当前登录人
    let s = parse(&schema(
        r#"[{"key":"who","label":"登记人","type":"name","required":true},
            {"key":"day","label":"发生日期","type":"date"}]"#,
    ));
    assert!(validate_schema(&s).is_ok(), "姓名类型不该被当成非法类型");

    let ok = serde_json::from_str(r#"{"who":"吴凯波","day":"2026-10-29"}"#).unwrap();
    assert!(validate_data(&s, &ok).is_empty());

    // 必填时还是要填
    let missing = serde_json::from_str(r#"{"day":"2026-10-29"}"#).unwrap();
    let errs = validate_data(&s, &missing);
    assert_eq!(errs.get("who").map(|s| s.as_str()), Some("必填"));

    // 超长要挡住（默认 30 字）
    let long_name = "啊".repeat(31);
    let long = serde_json::from_str(&format!(r#"{{"who":"{long_name}"}}"#)).unwrap();
    let errs = validate_data(&s, &long);
    assert!(errs.contains_key("who"), "姓名超过 30 字要挡住");
}

#[test]
fn 不认识的字段类型存不进去() {
    let s = parse(&schema(r#"[{"key":"a","label":"A","type":"phone"}]"#));
    assert!(validate_schema(&s).is_err(), "未知类型必须拒掉");
}

/* ------------------------------------------------------------ 6. 导出列 */

#[test]
fn 导出列取多版本并集且缺列留空() {
    let v1 = schema(r#"[{"key":"a","label":"A","type":"text"},{"key":"old","label":"老字段","type":"text"}]"#);
    let v2 = schema(r#"[{"key":"a","label":"A","type":"text"},{"key":"b","label":"B","type":"text"}]"#);

    let rows = vec![
        db::FormSubmission {
            template_name: String::new(),
            id: 2,
            submission_id: "s2".into(),
            template_id: 1,
            template_version: 2,
            template_snapshot: v2.clone(),
            user_id: 1,
            user_name: "李四".into(),
            data_json: r#"{"a":"A2","b":"B2"}"#.into(),
            client_time: 1_700_000_200,
            created_at: 1_700_000_300,
        },
        db::FormSubmission {
            template_name: String::new(),
            id: 1,
            submission_id: "s1".into(),
            template_id: 1,
            template_version: 1,
            template_snapshot: v1.clone(),
            user_id: 1,
            user_name: "张三".into(),
            data_json: r#"{"a":"A1","old":"O1"}"#.into(),
            client_time: 1_700_000_000,
            created_at: 1_700_000_100,
        },
    ];

    let table = xlsx::build_table(&rows, 480);
    // 列 = 两次出现的并集（按首次出现顺序）+ 三个系统列
    assert_eq!(
        table.headers,
        vec!["A", "B", "老字段", "提交人", "填写时间", "接收时间"]
    );

    // v2 那条没有 old 字段 → 留空，不能把 B 填到"老字段"列里
    assert_eq!(table.rows[0][..4].to_vec(), vec!["A2", "B2", "", "李四"]);
    // v1 那条没有 b 字段
    assert_eq!(table.rows[1][..4].to_vec(), vec!["A1", "", "O1", "张三"]);
    // 两个时间列都按 TZ_OFFSET=480 渲染成北京时间
    assert!(table.rows[0][4].starts_with("2023-11-15"), "{:?}", table.rows[0]);
    assert_eq!(table.rows[0].len(), table.headers.len());
    assert_eq!(table.rows[1].len(), table.headers.len());
}

#[test]
fn 导出的字节是合法_xlsx() {
    let snap = schema(r#"[{"key":"a","label":"A","type":"number"}]"#);
    let rows = vec![db::FormSubmission {
            template_name: String::new(),
        id: 1,
        submission_id: "s1".into(),
        template_id: 1,
        template_version: 1,
        template_snapshot: snap,
        user_id: 1,
        user_name: "张三".into(),
        data_json: r#"{"a":12}"#.into(),
        client_time: 1_700_000_000,
        created_at: 1_700_000_100,
    }];
    let table = xlsx::build_table(&rows, 480);
    let bytes = xlsx::write_xlsx(&table, "补发申请").unwrap();
    // xlsx 就是个 zip
    assert_eq!(&bytes[..2], b"PK");
    assert!(bytes.len() > 1000);
    // sheet 名里不能出现 xlsx 禁用的字符
    assert!(xlsx::write_xlsx(&table, "a[b]c:d*e?f/g\\h").is_ok());
}

/* ============================ 4. 删模板（软删除）——不影响已生成的表格 */

#[test]
fn 删模板后历史登记和名字都还在() {
    let conn = fresh();
    let v1 = schema(r#"[{"key":"a","label":"A","type":"text"}]"#);
    db::form_template_save(&conn, None, "客户需求表", &v1, None, 0).unwrap();
    db::form_submit_insert(
        &conn,
        &db::NewSubmission {
            template_name: "测试模板",
            submission_id: "sub-0001",
            template_id: 1,
            template_version: 1,
            template_snapshot: &v1,
            user_id: 1,
            user_name: "张三",
            data_json: r#"{"a":"NO1"}"#,
            client_time: 1_700_000_000,
            client_id: "",
        },
    )
    .unwrap();

    assert!(db::form_template_delete(&conn, 1).unwrap(), "删除应该成功");

    // 登记一条不少
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM form_submissions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1, "删模板不能动已经填过的登记");

    // 模板还在库里（软删除），历史记录还认得它的名字
    let all = db::form_templates_all(&conn).unwrap();
    assert_eq!(all.len(), 1);
    assert!(all[0].deleted_at > 0, "应该打上删除时间");
    assert!(all[0].name.starts_with("客户需求表"), "名字要认得出来：{}", all[0].name);

    // 但列表里已经没有了
    assert!(db::form_templates(&conn, true).unwrap().is_empty(), "列表里不该再出现");
    assert!(db::form_templates(&conn, false).unwrap().is_empty(), "客户端更不该看到");

    // 删过的模板不能再收新登记
    assert!(db::form_template_deleted(&conn, 1).unwrap());
}

#[test]
fn 删模板后能用回原来那个名字() {
    let conn = fresh();
    let v1 = schema(r#"[{"key":"a","label":"A","type":"text"}]"#);
    db::form_template_save(&conn, None, "客户需求表", &v1, None, 0).unwrap();
    db::form_template_delete(&conn, 1).unwrap();
    // 名字腾出来了：可以重新建一个同名模板（UNIQUE 约束不能挡着）
    let r = db::form_template_save(&conn, None, "客户需求表", &v1, None, 0).unwrap();
    assert!(matches!(r, db::TemplateSave::Saved { id: 2, version: 1 }));
}

/* ============================ 5. 改一条登记（预览里编辑） */

#[test]
fn 改登记只动内容不动字段定义() {
    let conn = fresh();
    let v1 = schema(r#"[{"key":"a","label":"A","type":"text"}]"#);
    db::form_template_save(&conn, None, "表", &v1, None, 0).unwrap();
    db::form_submit_insert(
        &conn,
        &db::NewSubmission {
            template_name: "测试模板",
            submission_id: "sub-0001",
            template_id: 1,
            template_version: 1,
            template_snapshot: &v1,
            user_id: 1,
            user_name: "张三",
            data_json: r#"{"a":"旧值"}"#,
            client_time: 1_700_000_000,
            client_id: "",
        },
    )
    .unwrap();

    let id: i64 = conn
        .query_row("SELECT id FROM form_submissions", [], |r| r.get(0))
        .unwrap();
    assert!(db::form_submission_update(&conn, id, r#"{"a":"新值"}"#).unwrap());

    let sub = db::form_submission_by_id(&conn, id).unwrap().unwrap();
    assert_eq!(sub.data_json, r#"{"a":"新值"}"#);
    assert_eq!(sub.template_snapshot, v1, "字段定义（快照）不能动，改了列就串了");
    assert_eq!(sub.user_name, "张三", "提交人不能改");
}

/* ============================ 6. 管理端人员与口令 */

#[test]
fn 管理端口令只存哈希() {
    let conn = fresh();
    let hash = crate::forms::token_hash("abc123456");
    assert_ne!(hash, "abc123456", "库里不能是明文");
    assert_eq!(hash.len(), 64, "sha256 的十六进制是 64 个字");

    db::admin_user_create(&conn, "李会计", &hash).unwrap();
    let found = db::admin_user_by_hash(&conn, &hash).unwrap().unwrap();
    assert_eq!(found.name, "李会计");
    assert!(found.enabled);
    // 换一个口令就找不到了
    assert!(db::admin_user_by_hash(&conn, &crate::forms::token_hash("别的")).unwrap().is_none());
}

#[test]
fn 停用管理端人员后他的口令登不进来() {
    let conn = fresh();
    let hash = crate::forms::token_hash("abc123456");
    db::admin_user_create(&conn, "李会计", &hash).unwrap();
    assert!(db::admin_user_toggle(&conn, 1, false).unwrap());
    let u = db::admin_users(&conn).unwrap();
    assert!(!u[0].enabled);
}

#[test]
fn 改管理端口令后老口令失效() {
    let conn = fresh();
    let old = crate::forms::token_hash("abc123456");
    db::admin_user_create(&conn, "李会计", &old).unwrap();
    let new = crate::forms::token_hash("xyz987654");
    db::admin_user_update(&conn, 1, "李会计", Some(&new)).unwrap();

    assert!(db::admin_user_by_hash(&conn, &old).unwrap().is_none(), "老口令要失效");
    assert!(db::admin_user_by_hash(&conn, &new).unwrap().is_some(), "新口令要能用");
}

/* ============================ 7. 查询参数空串（筛选表单） */

#[test]
fn 空串查询参数当成没填() {
    // 下拉框"全部"选项的值就是空串，筛选表单每次都会带上 template=&user=
    let uri: axum::http::Uri =
        "/admin?tab=register&sub=preview&template=&user=&from=&to=".parse().unwrap();
    let q = axum::extract::Query::<crate::admin::PageQuery>::try_from_uri(&uri).unwrap();
    assert_eq!(q.template, None, "空串不能变成 400");
    assert_eq!(q.user, None);
    assert_eq!(q.tab(), "register");
    assert_eq!(q.sub("register"), "preview");
}

#[test]
fn 查询参数里的整数_空串和乱填都当没填() {
    // 筛选表单每次提交都带着 template=&user=（"全部"选项的值就是空串）。
    // 这两个参数以前按 Option<i64> 直接解析，空串会报
    // "cannot parse integer from empty string" → 整页 400，看起来像"点了没反应"。
    let uri: axum::http::Uri =
        "/admin?tab=register&sub=preview&template=&user=7&from=&to=".parse().unwrap();
    let q = axum::extract::Query::<crate::admin::PageQuery>::try_from_uri(&uri).unwrap();
    assert_eq!(q.template, None, "空串当成没填");
    assert_eq!(q.user, Some(7), "正常数字要能读出来");
    assert_eq!(q.from.as_deref(), Some(""), "字符串参数不受影响");

    // 乱填也不能 400，当成没填
    let uri2: axum::http::Uri = "/admin?template=abc".parse().unwrap();
    let q2 = axum::extract::Query::<crate::admin::PageQuery>::try_from_uri(&uri2).unwrap();
    assert_eq!(q2.template, None);
}

/* --------------------------------------- v0.5：模板的网页版编辑器 + 永久删除 */

#[test]
fn 网页版编辑器_解析出字段并按位置排序() {
    let mut form = std::collections::HashMap::new();
    form.insert("id".to_string(), "1".to_string());
    form.insert("name".to_string(), "  补发申请  ".to_string());
    // 三行，故意把顺序打乱：位置 2 / 1 / 3
    form.insert("f0_pos".to_string(), "2".to_string());
    form.insert("f0_key".to_string(), "b".to_string());
    form.insert("f0_label".to_string(), "乙".to_string());
    form.insert("f0_type".to_string(), "text".to_string());

    form.insert("f1_pos".to_string(), "1".to_string());
    form.insert("f1_key".to_string(), "a".to_string());
    form.insert("f1_label".to_string(), "甲".to_string());
    form.insert("f1_type".to_string(), "select".to_string());
    form.insert("f1_required".to_string(), "1".to_string());
    form.insert("f1_options".to_string(), "一，二、三".to_string());

    form.insert("f2_pos".to_string(), "3".to_string());
    form.insert("f2_key".to_string(), "c".to_string());
    form.insert("f2_label".to_string(), "丙".to_string());
    form.insert("f2_type".to_string(), "number".to_string());

    let (name, fields) = crate::forms::parse_template_form(&form);
    assert_eq!(name, "补发申请", "名字要去掉首尾空格");
    assert_eq!(fields.len(), 3);
    assert_eq!(
        fields.iter().map(|f| f.key.as_str()).collect::<Vec<_>>(),
        vec!["a", "b", "c"],
        "必须按「位置」排序，不能按行号"
    );
    assert!(fields[0].required, "必填勾上了要认出来");
    assert_eq!(
        fields[0].options.as_deref(),
        Some(["一".to_string(), "二".to_string(), "三".to_string()].as_slice()),
        "下拉选项要按中英文逗号都切"
    );
    assert!(fields[1].options.is_none(), "非下拉字段不该带选项");
}

#[test]
fn 网页版编辑器_删掉的行和空行都不算() {
    let mut form = std::collections::HashMap::new();
    form.insert("name".to_string(), "表".to_string());
    // 这一行勾了「删」
    form.insert("f0_key".to_string(), "a".to_string());
    form.insert("f0_label".to_string(), "甲".to_string());
    form.insert("f0_type".to_string(), "text".to_string());
    form.insert("f0_del".to_string(), "1".to_string());
    // 这一行标识是空的（没 JS 时「加一个字段」留下的空行）
    form.insert("f1_key".to_string(), "".to_string());
    form.insert("f1_label".to_string(), "还没填".to_string());
    form.insert("f1_type".to_string(), "text".to_string());
    // 这一行是正常的
    form.insert("f2_key".to_string(), "b".to_string());
    form.insert("f2_label".to_string(), "乙".to_string());
    form.insert("f2_type".to_string(), "text".to_string());

    let (_, fields) = crate::forms::parse_template_form(&form);
    assert_eq!(fields.len(), 1, "只该留下没被删、标识也填了的那一行");
    assert_eq!(fields[0].key, "b");
}

#[test]
fn 提交时把模板名冗余存进登记() {
    let conn = fresh();
    let v1 = schema(r#"[{"key":"a","label":"A","type":"text"}]"#);
    db::form_template_save(&conn, None, "客户需求表", &v1, None, 0).unwrap();
    db::form_submit_insert(
        &conn,
        &db::NewSubmission {
            template_name: "客户需求表",
            submission_id: "sub-9001",
            template_id: 1,
            template_version: 1,
            template_snapshot: &v1,
            user_id: 1,
            user_name: "张三",
            data_json: r#"{"a":"1"}"#,
            client_time: 1_700_000_000,
            client_id: "",
        },
    )
    .unwrap();

    let name: String = conn
        .query_row(
            "SELECT template_name FROM form_submissions WHERE submission_id = 'sub-9001'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(name, "客户需求表", "模板名要冗余存一份，永久删除后还认得出来");
}

#[test]
fn 永久删除后模板和填写记录都真删除() {
    let conn = fresh();
    let v1 = schema(r#"[{"key":"a","label":"A","type":"text"}]"#);
    db::form_template_save(&conn, None, "客户需求表", &v1, None, 0).unwrap();
    // 老数据：template_name 是空的（这一列是后加的）
    db::form_submit_insert(
        &conn,
        &db::NewSubmission {
            template_name: "",
            submission_id: "sub-9101",
            template_id: 1,
            template_version: 1,
            template_snapshot: &v1,
            user_id: 1,
            user_name: "张三",
            data_json: r#"{"a":"1"}"#,
            client_time: 1_700_000_000,
            client_id: "",
        },
    )
    .unwrap();

    // 先软删
    assert!(db::form_template_delete(&conn, 1).unwrap());
    // 再永久删除
    match db::form_template_purge(&conn, 1).unwrap() {
        db::PurgeResult::Purged { submissions } => assert_eq!(submissions, 1),
        other => panic!("应该是删成功，实际 {other:?}"),
    }

    // 模板行真的没了
    assert!(db::form_templates_all(&conn).unwrap().is_empty(), "模板行应该被真删");
    assert!(
        db::form_template_by_id(&conn, 1).unwrap().is_none(),
        "再查也查不到这个模板"
    );

    // 填写记录也必须一并删除
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM form_submissions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 0, "永久删除模板必须同时删除其填写记录");
}

#[test]
fn 没删过的模板不能永久删除() {
    let conn = fresh();
    let v1 = schema(r#"[{"key":"a","label":"A","type":"text"}]"#);
    db::form_template_save(&conn, None, "客户需求表", &v1, None, 0).unwrap();

    match db::form_template_purge(&conn, 1).unwrap() {
        db::PurgeResult::NotDeleted => {}
        other => panic!("没软删的模板不该被永久删除，实际 {other:?}"),
    }
    // 模板还在
    assert_eq!(db::form_templates_all(&conn).unwrap().len(), 1);
}

#[test]
fn 永久删除会清掉字段定义的历史版本() {
    let conn = fresh();
    let v1 = schema(r#"[{"key":"a","label":"A","type":"text"}]"#);
    let v2 = schema(r#"[{"key":"a","label":"A","type":"text"},{"key":"b","label":"B","type":"text"}]"#);
    db::form_template_save(&conn, None, "客户需求表", &v1, None, 0).unwrap();
    db::form_template_save(&conn, Some(1), "客户需求表", &v2, Some(1), 0).unwrap();

    let n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM form_template_versions WHERE template_id = 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 2, "应该存了两个版本的字段定义");

    db::form_template_delete(&conn, 1).unwrap();
    db::form_template_purge(&conn, 1).unwrap();

    let n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM form_template_versions WHERE template_id = 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 0, "永久删除要把字段定义的历史版本也清掉");
}

/* --------------------------- v0.6：表单预览的网格（改登记数据，不是改字段） */

use serde_json::{Map, Value};

/// 造一条登记，返回它的行 id
fn insert_sub(conn: &Connection, snapshot: &str, data_json: &str) -> i64 {
    db::form_submit_insert(
        conn,
        &db::NewSubmission {
            template_name: "测试模板",
            submission_id: "sid-grid",
            template_id: 1,
            template_version: 1,
            template_snapshot: snapshot,
            user_id: 1,
            user_name: "张三",
            data_json,
            client_time: 1_700_000_000,
            client_id: "pc",
        },
    )
    .unwrap();
    conn.query_row(
        "SELECT id FROM form_submissions WHERE submission_id = 'sid-grid'",
        [],
        |r| r.get(0),
    )
    .unwrap()
}

#[test]
fn 网格的列_当前模板字段在前_旧字段附在后面并标旧() {
    let cur = parse(&schema(
        r#"[{"key":"a","label":"甲","type":"text"},{"key":"b","label":"乙","type":"select","options":["x","y"]}]"#,
    ));
    // 这条登记是旧版本提交的：多一个 c，少一个 b
    let data: Map<String, Value> = serde_json::from_str(r#"{"a":"1","c":"3"}"#).unwrap();
    let cols = crate::forms::grid_columns(Some(&cur), &[(Some(cur.clone()), data)]);
    assert_eq!(
        cols.iter().map(|c| c.key.as_str()).collect::<Vec<_>>(),
        vec!["a", "b", "c"],
        "列必须是「当前模板字段 + 多出来的旧字段」，不能按登记的列"
    );
    assert!(!cols[0].legacy && !cols[1].legacy);
    assert!(cols[2].legacy, "c 是当前模板没有的字段，得标成旧列（只读）");
    assert_eq!(
        cols[1].options,
        vec!["x".to_string(), "y".to_string()],
        "下拉列要带上选项，前端才知道渲染成什么控件"
    );
}

#[test]
fn 网格改登记_只改传进来的字段_没提到的和旧字段都原样保留() {
    let conn = fresh();
    let snap = schema(r#"[{"key":"a","label":"甲","type":"text"},{"key":"b","label":"乙","type":"text"}]"#);
    let id = insert_sub(&conn, &snap, r#"{"a":"原值","b":"乙值","old":"旧数据"}"#);

    let mut v = std::collections::BTreeMap::new();
    v.insert("a".to_string(), " 新值 ".to_string());
    crate::forms::apply_submission_edit(&conn, id, &v).unwrap();

    let row = db::form_submission_by_id(&conn, id).unwrap().unwrap();
    let data: Value = serde_json::from_str(&row.data_json).unwrap();
    assert_eq!(data["a"], "新值", "要存上，而且首尾空格得去掉");
    assert_eq!(data["b"], "乙值", "没提到的字段不能被清空");
    assert_eq!(data["old"], "旧数据", "旧字段要原样留着，不然数据就丢了");
    assert_eq!(
        row.template_snapshot, snap,
        "只改 data_json，快照一个字节都不能动"
    );
}

#[test]
fn 网格改登记_清空必填会被拒_而且库里的东西一点没改() {
    let conn = fresh();
    let snap = schema(r#"[{"key":"a","label":"甲","type":"text","required":true}]"#);
    let id = insert_sub(&conn, &snap, r#"{"a":"原值"}"#);

    let mut v = std::collections::BTreeMap::new();
    v.insert("a".to_string(), "".to_string());
    let r = crate::forms::apply_submission_edit(&conn, id, &v);
    assert!(r.is_err(), "必填被清空必须拒掉");
    assert!(
        r.unwrap_err().contains("甲"),
        "报错要带上字段的中文名，不然不知道是哪一栏"
    );

    let row = db::form_submission_by_id(&conn, id).unwrap().unwrap();
    let data: Value = serde_json::from_str(&row.data_json).unwrap();
    assert_eq!(data["a"], "原值", "校验不过就不能改库");
}

#[test]
fn 网格改登记_改一条不影响同模板的其它行() {
    let conn = fresh();
    let snap = schema(r#"[{"key":"a","label":"甲","type":"text"}]"#);
    let id1 = insert_sub(&conn, &snap, r#"{"a":"第一行"}"#);
    // 换个 submission_id 再插一条（同一模板）
    db::form_submit_insert(
        &conn,
        &db::NewSubmission {
            template_name: "测试模板",
            submission_id: "sid-grid-2",
            template_id: 1,
            template_version: 1,
            template_snapshot: &snap,
            user_id: 1,
            user_name: "李四",
            data_json: r#"{"a":"第二行"}"#,
            client_time: 1_700_000_100,
            client_id: "pc",
        },
    )
    .unwrap();

    let mut v = std::collections::BTreeMap::new();
    v.insert("a".to_string(), "改过的第一行".to_string());
    crate::forms::apply_submission_edit(&conn, id1, &v).unwrap();

    let other: String = conn
        .query_row(
            "SELECT data_json FROM form_submissions WHERE submission_id = 'sid-grid-2'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let data: Value = serde_json::from_str(&other).unwrap();
    assert_eq!(data["a"], "第二行", "改一行不能把别的行带上");
}

/* ---------------------------- v1.6：物流快递字段 + 按快递分工作表 */

fn express_schema(extra: &str) -> String {
    schema(&format!(
        r#"[{{"key":"exp","label":"物流","type":"express"{extra}}}]"#
    ))
}

#[test]
fn 物流字段_必填时单号和快递公司都得有() {
    let s = parse(&express_schema(r#","required":true"#));
    let none = serde_json::Map::new();
    let errs = validate_data(&s, &none);
    assert!(errs.contains_key("exp"), "必填的物流字段不能空着");

    let mut only_number = serde_json::Map::new();
    only_number.insert("exp".into(), serde_json::json!({"tracking_number":"SF123","carrier":""}));
    let errs = validate_data(&s, &only_number);
    assert!(errs.contains_key("exp"), "填了单号没选快递公司，一样不算填完");

    let mut ok = serde_json::Map::new();
    ok.insert("exp".into(), serde_json::json!({"tracking_number":"SF123","carrier":"顺丰"}));
    assert!(validate_data(&s, &ok).is_empty(), "两个都填了就该过");
}

#[test]
fn 物流字段_单号和快递公司都有长度上限() {
    let s = parse(&express_schema(""));
    let mut too_long = serde_json::Map::new();
    too_long.insert(
        "exp".into(),
        serde_json::json!({"tracking_number":"x".repeat(61),"carrier":"顺丰"}),
    );
    assert!(validate_data(&s, &too_long).contains_key("exp"), "单号超过 60 字要拦");

    let mut carrier_long = serde_json::Map::new();
    carrier_long.insert(
        "exp".into(),
        serde_json::json!({"tracking_number":"SF1","carrier":"快".repeat(31)}),
    );
    assert!(validate_data(&s, &carrier_long).contains_key("exp"), "快递公司超过 30 字要拦");
}

#[test]
fn 按快递分表_只能勾在物流字段上且最多一个() {
    // 勾在 text 字段上 → 拒
    let s = schema(
        r#"[{"key":"a","label":"甲","type":"text","groupByCarrier":true}]"#,
    );
    let err = validate_schema(&parse(&s)).unwrap_err();
    assert!(err.contains("物流快递"), "只有物流字段能按快递分组，实际：{err}");

    // 两个物流字段都勾 → 拒
    let s = schema(
        r#"[{"key":"e1","label":"物流1","type":"express","groupByCarrier":true},
            {"key":"e2","label":"物流2","type":"express","groupByCarrier":true}]"#,
    );
    let err = validate_schema(&parse(&s)).unwrap_err();
    assert!(err.contains("最多"), "一个模板最多一个分组字段，实际：{err}");

    // 一个物流字段勾 → 过
    let s = express_schema(r#","groupByCarrier":true"#);
    assert!(validate_schema(&parse(&s)).is_ok());
}

#[test]
fn 网格改物流字段_只读_不会被改动() {
    let conn = fresh();
    let snap = express_schema("");
    let id = insert_sub(&conn, &snap, r#"{"exp":[{"tracking_number":"旧单号","carrier":"圆通速递"}]}"#);

    let mut v = std::collections::BTreeMap::new();
    v.insert("exp".to_string(), "SF999 | 顺丰快递".to_string());
    crate::forms::apply_submission_edit(&conn, id, &v).unwrap();

    let row = db::form_submission_by_id(&conn, id).unwrap().unwrap();
    let data: Value = serde_json::from_str(&row.data_json).unwrap();
    // 网格里快递字段只读：即使传了值也不改，原来的数组原样保留
    assert_eq!(data["exp"][0]["tracking_number"], "旧单号");
    assert_eq!(data["exp"][0]["carrier"], "圆通速递");
}

#[test]
fn 多工作表导出_重名的表自动加后缀() {
    let t = crate::xlsx::Table {
        headers: vec!["A".into()],
        keys: vec!["a".into()],
        rows: vec![vec!["1".into()]],
        merges: vec![],
        force_text: vec![],
    };
    // 同一个名字给三次：第一张叫「顺丰」，后面两张要变成「顺丰-2」「顺丰-3」
    let sheets = [(&t, "顺丰".to_string()), (&t, "顺丰".to_string()), (&t, "顺丰".to_string())];
    let bytes = crate::xlsx::write_xlsx_sheets(&sheets).unwrap();
    assert_eq!(&bytes[0..2], b"PK", "应该是一个合法 xlsx");

    // 换成真正会撞名的长名字也要能存下去
    let long = "很长的快递公司名称".repeat(3); // 超过 31 字
    let sheets = [(&t, long.clone()), (&t, long.clone())];
    assert!(crate::xlsx::write_xlsx_sheets(&sheets).is_ok(), "长名字撞名不能报错");
}

/* ---------------------------- 2026-10-31 后续：回收站还原 + 空密码免密 */

#[test]
fn 回收站还原_名字和原来的启用状态都恢复() {
    let conn = fresh();
    let v1 = schema(r#"[{"key":"a","label":"A","type":"text"}]"#);
    db::form_template_save(&conn, None, "客户需求表", &v1, None, 0).unwrap();
    // 先停用，再删除 —— 还原后应该还是停用状态
    db::form_template_toggle(&conn, 1, false).unwrap();
    assert!(db::form_template_delete(&conn, 1).unwrap());

    let t = db::form_templates_all(&conn).unwrap().remove(0);
    assert_eq!(t.name, "客户需求表（已删除）", "软删除要改名腾出 UNIQUE");
    assert_eq!(t.deleted_name, "客户需求表", "原名要存进 deleted_name");

    match db::form_template_restore(&conn, 1).unwrap() {
        db::RestoreResult::Restored => {}
        other => panic!("应该能还原，实际 {other:?}"),
    }
    let t = db::form_templates_all(&conn).unwrap().remove(0);
    assert_eq!(t.name, "客户需求表", "还原要把原名还回去");
    assert_eq!(t.deleted_at, 0);
    assert!(!t.is_active, "删除前是停用的，还原后还得是停用");
    assert_eq!(t.deleted_name, "");
}

#[test]
fn 回收站还原_原名字被占了就不还原而且库不动() {
    let conn = fresh();
    let v1 = schema(r#"[{"key":"a","label":"A","type":"text"}]"#);
    db::form_template_save(&conn, None, "客户需求表", &v1, None, 0).unwrap();
    db::form_template_delete(&conn, 1).unwrap();
    // 用原名再建一个（软删除已经腾出了 UNIQUE）
    db::form_template_save(&conn, None, "客户需求表", &v1, None, 0).unwrap();

    match db::form_template_restore(&conn, 1).unwrap() {
        db::RestoreResult::NameTaken => {}
        other => panic!("重名应该拒绝，实际 {other:?}"),
    }
    let t = db::form_template_by_id(&conn, 1).unwrap().unwrap();
    assert_eq!(t.name, "客户需求表（已删除）", "被拒时不能改库");
    assert!(t.deleted_at > 0);
}

#[test]
fn 回收站还原_没删过的模板不能被还原() {
    let conn = fresh();
    let v1 = schema(r#"[{"key":"a","label":"A","type":"text"}]"#);
    db::form_template_save(&conn, None, "客户需求表", &v1, None, 0).unwrap();
    match db::form_template_restore(&conn, 1).unwrap() {
        db::RestoreResult::NotDeleted => {}
        other => panic!("没删过的应该返回 NotDeleted，实际 {other:?}"),
    }
}

#[test]
fn 空密码账号_只有空密码能登进来() {
    // 空哈希 = 免密登录；但只要传了非空密码，就不能过
    assert!(crate::forms::password_matches("", ""), "空哈希 + 空密码 = 免密");
    assert!(!crate::forms::password_matches("123456", ""), "空哈希不接受任何非空密码");

    // 有哈希时必须比对
    let conn = fresh();
    db::form_user_save(&conn, None, "张三", None).unwrap();
    let empty: String = conn
        .query_row("SELECT password_hash FROM form_users WHERE name='张三'", [], |r| r.get(0))
        .unwrap();
    assert!(empty.is_empty(), "不传密码创建出来的是空哈希");
}

#[test]
fn 空密码存空串_一位密码也能正常哈希比对() {
    // 空密码不存哈希，存空串 —— 这是"免密登录"的开关
    assert_eq!(crate::forms::hash_password("").unwrap(), "");
    // 没有最小长度限制，一位也能设
    let h = crate::forms::hash_password("a").unwrap();
    assert!(h.starts_with("$argon2"), "应该是 argon2 编码串");
    assert!(crate::forms::password_matches("a", &h));
    assert!(!crate::forms::password_matches("b", &h));
}

#[test]
fn 模板原名_优先用删除时存下的那份() {
    let mut t = db::FormTemplate {
        id: 1,
        name: "客户需求表（已删除）".into(),
        version: 1,
        is_active: false,
        sort_order: 0,
        updated_at: 0,
        schema_json: "{}".into(),
        deleted_at: 123,
        deleted_name: "客户需求表".into(),
    };
    assert_eq!(crate::forms::template_original_name(&t), "客户需求表");

    // 老数据没存 deleted_name：从「（已删除）」后缀还原
    t.deleted_name = String::new();
    assert_eq!(crate::forms::template_original_name(&t), "客户需求表");

    // 连后缀都没有（理论上不该出现）：原样返回，宁可不删也不猜
    t.name = "客户需求表".into();
    assert_eq!(crate::forms::template_original_name(&t), "客户需求表");
}

/* ============================================================ 商品库 */

fn fresh_products() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    db::init_forms(&conn).unwrap();
    db::init_products(&conn).unwrap();
    conn
}

fn tmpdir(tag: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!("bks-test-{tag}-{}", db::now()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn add_product(conn: &Connection, name: &str) -> i64 {
    db::product_save(
        conn,
        None,
        &db::NewProduct {
            name,
            category: "园艺",
            class_name: "园艺肥料",
            forms_json: r#"["液体"]"#,
            funcs_json: r#"["绿叶肥"]"#,
            usage_text: "稀释 1000 倍灌根",
            image: "",
            listed_at: "2026-11-04",
        },
    )
    .unwrap()
}

fn add_sku(conn: &Connection, pid: i64, barcode: &str, cents: i64) -> i64 {
    db::sku_save(
        conn,
        None,
        &db::NewSku {
            product_id: pid,
            spec_value: 500.0,
            spec_unit: "ml",
            spec_pack: "瓶",
            barcode69: barcode,
            price_cents: cents,
            promo: "买2送1",
            sort_order: 0,
        },
    )
    .unwrap()
}

#[test]
fn 码表出厂就带着脑图里的剂型和功能() {
    let conn = fresh_products();
    let dict = db::product_dict(&conn).unwrap();
    let forms: Vec<&str> = dict.iter().filter(|d| d.kind == "form").map(|d| d.value.as_str()).collect();
    let funcs: Vec<&str> = dict.iter().filter(|d| d.kind == "function").map(|d| d.value.as_str()).collect();
    // 悬浮剂和悬乳剂是两个东西，别合并
    assert!(forms.contains(&"悬浮剂") && forms.contains(&"悬乳剂"));
    assert!(forms.contains(&"可湿性粉剂") && forms.contains(&"水分散粒剂"));
    assert!(funcs.contains(&"杀虫剂") && funcs.contains(&"植物生长调节剂"));
    assert_eq!(dict.iter().filter(|d| d.kind == "category").count(), 2);
    assert_eq!(dict.iter().filter(|d| d.kind == "class").count(), 4);
}

#[test]
fn 码表可以加可以删也可以重排() {
    let conn = fresh_products();
    db::dict_add(&conn, "form", "微乳剂").unwrap();
    let dict = db::product_dict(&conn).unwrap();
    let item = dict.iter().find(|d| d.kind == "form" && d.value == "微乳剂").unwrap();

    // 加到末尾
    let last = dict.iter().filter(|d| d.kind == "form").next_back().unwrap();
    assert_eq!(item.sort_order, last.sort_order);

    // 把「微乳剂」拖到第一个
    let mut ids: Vec<i64> = dict.iter().filter(|d| d.kind == "form").map(|d| d.id).collect();
    ids.retain(|id| *id != item.id);
    ids.insert(0, item.id);
    db::dict_reorder(&conn, "form", &ids).unwrap();

    let dict = db::product_dict(&conn).unwrap();
    let first = dict.iter().find(|d| d.kind == "form").unwrap();
    assert_eq!(first.value, "微乳剂");

    db::dict_delete(&conn, item.id).unwrap();
    let dict = db::product_dict(&conn).unwrap();
    assert!(!dict.iter().any(|d| d.value == "微乳剂"));
}

#[test]
fn 商品和规格存得住_69码不能重复() {
    let conn = fresh_products();
    let pid = add_product(&conn, "花卉营养液");
    let sku_id = add_sku(&conn, pid, "6931234567890", 3990);

    let p = db::product_by_id(&conn, pid).unwrap().unwrap();
    assert_eq!(p.name, "花卉营养液");
    assert_eq!(p.forms_json, r#"["液体"]"#);

    let skus = db::skus_of(&conn, pid).unwrap();
    assert_eq!(skus.len(), 1);
    assert_eq!(skus[0].price_cents, 3990);
    assert_eq!(skus[0].spec_pack, "瓶");

    // 同一个 69 码换到别的规格上要被挡
    assert!(db::sku_barcode_taken(&conn, "6931234567890", 0).unwrap());
    // 自己重存不算冲突
    assert!(!db::sku_barcode_taken(&conn, "6931234567890", sku_id).unwrap());
}

#[test]
fn 增量同步只回新改动_删除也有流水() {
    let conn = fresh_products();
    let pid = add_product(&conn, "颗粒缓释肥");
    let rev0 = db::product_rev(&conn).unwrap();
    assert!(rev0 > 0);

    // 新商品在 since=rev0 之后
    let pid2 = add_product(&conn, "杀虫剂");
    let sync = db::product_sync(&conn, rev0).unwrap();
    assert_eq!(sync.products.len(), 1, "只该回刚加的那个");
    assert_eq!(sync.products[0].id, pid2);
    assert!(sync.skus.is_empty());

    // 加规格：商品本身也会跟着刷新（客户端按商品聚合）
    let sku_id = add_sku(&conn, pid2, "6930000000001", 1990);
    let sync = db::product_sync(&conn, rev0).unwrap();
    assert_eq!(sync.skus.len(), 1);
    assert!(sync.products.iter().any(|p| p.id == pid2));

    // 删除：商品和规格都留流水
    let rev1 = db::product_rev(&conn).unwrap();
    db::product_delete(&conn, pid2).unwrap();
    let sync = db::product_sync(&conn, rev1).unwrap();
    assert!(sync.products.is_empty());
    assert!(sync.deleted.iter().any(|(k, id)| k == "product" && *id == pid2));
    assert!(sync.deleted.iter().any(|(k, id)| k == "sku" && *id == sku_id));

    // 全量（since=0）能拿到还活着的商品
    let all = db::product_sync(&conn, 0).unwrap();
    assert_eq!(all.products.len(), 1);
    assert_eq!(all.products[0].id, pid);
}

#[test]
fn 售价从元转成分_格式不对要挡住() {
    use crate::products::price_to_cents;
    assert_eq!(price_to_cents("39"), Some(3900));
    assert_eq!(price_to_cents("39.9"), Some(3990));
    assert_eq!(price_to_cents("39.99"), Some(3999));
    assert_eq!(price_to_cents("0.05"), Some(5));
    assert_eq!(price_to_cents(""), Some(0));
    assert_eq!(price_to_cents("1,299"), Some(129900));
    assert_eq!(price_to_cents("39.999"), None);
    assert_eq!(price_to_cents("abc"), None);
    assert_eq!(price_to_cents("-5"), None);
}

/* ------------------------------------------------------------ 图片 */

fn png_of(w: u32, h: u32, rgb: [u8; 3]) -> Vec<u8> {
    use image::ImageEncoder;
    let mut img = image::RgbImage::new(w, h);
    for p in img.pixels_mut() {
        *p = image::Rgb(rgb);
    }
    let mut buf = Vec::new();
    image::codecs::png::PngEncoder::new(&mut buf)
        .write_image(img.as_raw(), w, h, image::ExtendedColorType::Rgb8)
        .unwrap();
    buf
}

#[test]
fn 竖长图会被补成800正方形_不是裁切() {
    let src = png_of(400, 900, [200, 30, 30]);
    let out = crate::products::normalize_image(&src).unwrap();
    let img = image::load_from_memory(&out).unwrap().to_rgb8();
    assert_eq!(img.dimensions(), (800, 800));

    // 左上角是补的白边
    assert_eq!(img.get_pixel(0, 0).0, [255, 255, 255]);
    // 中间还是原图（没被裁掉）
    let mid = img.get_pixel(400, 400).0;
    assert!(mid[0] > 180 && mid[1] < 60, "中间应该是红色原图，实际 {mid:?}");
    // 400×900 等比缩到长边 800 → 355×800，左右各留 ~222 白边
    let left = img.get_pixel(60, 400).0;
    assert_eq!(left, [255, 255, 255]);
}

#[test]
fn 缩略图是160正方形() {
    let src = png_of(1000, 600, [10, 120, 60]);
    let big = crate::products::normalize_image(&src).unwrap();
    let thumb = crate::products::make_thumb(&big).unwrap();
    let img = image::load_from_memory(&thumb).unwrap().to_rgb8();
    assert_eq!(img.dimensions(), (160, 160));
    assert!(thumb.len() < big.len(), "缩略图必须比原图小");
}

#[test]
fn 同一张图重复存只占一份_哈希相同() {
    let dir = tmpdir("img");
    let src = png_of(300, 300, [30, 30, 200]);
    let a = crate::products::store_image_in(&dir, &src).unwrap();
    let b = crate::products::store_image_in(&dir, &src).unwrap();
    assert_eq!(a, b);
    let files = std::fs::read_dir(&dir).unwrap().count();
    assert_eq!(files, 2, "只有大图 + 缩略图各一份");
    let _ = std::fs::remove_dir_all(&dir);
}

/* ------------------------------------------------------------ 备份 / 恢复 */

#[test]
fn 备份包含数据库_密钥_图片和清单() {
    let dir = tmpdir("backup");
    let db_path = dir.join("updates.db");
    let conn = db::open(&db_path).unwrap();
    let pid = add_product(&conn, "备份测试商品");
    let img_dir = dir.join("products").join("img");
    std::fs::create_dir_all(&img_dir).unwrap();
    let hash = crate::products::store_image_in(&img_dir, &png_of(500, 500, [1, 2, 3])).unwrap();
    db::product_set_images(&conn, pid, &[hash.clone()]).unwrap();
    std::fs::write(dir.join("secret.key"), b"fake-secret").unwrap();

    let (zip_path, size) = crate::backup::build_backup_at(&conn, &dir, "unit-test").unwrap();
    assert!(size > 0);

    let file = std::fs::File::open(&zip_path).unwrap();
    let mut archive = zip::ZipArchive::new(file).unwrap();
    let names: Vec<String> = (0..archive.len())
        .map(|i| archive.by_index(i).unwrap().name().to_string())
        .collect();
    assert!(names.iter().any(|n| n == "updates.db"));
    assert!(names.iter().any(|n| n == "secret.key"));
    assert!(names.iter().any(|n| n == "manifest.json"));
    assert_eq!(
        names.iter().filter(|n| n.starts_with("img/")).count(),
        2,
        "大图 + 缩略图"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn 恢复能把数据整套装回去_并且先留一份现在的() {
    let src_dir = tmpdir("restore-src");
    let conn = db::open(&src_dir.join("updates.db")).unwrap();
    add_product(&conn, "恢复前的商品");
    let (zip_path, _) = crate::backup::build_backup_at(&conn, &src_dir, "unit-test").unwrap();
    let bytes = std::fs::read(&zip_path).unwrap();
    drop(conn);

    // 目标：一个全新的数据目录（模拟服务器重装）
    let dst_dir = tmpdir("restore-dst");
    let staged = crate::backup::stage_restore(&dst_dir, &bytes).unwrap();
    assert!(staged >= 2, "至少是 db + manifest");
    assert!(dst_dir.join("restore.pending").exists());

    crate::backup::apply_pending(&dst_dir).unwrap();
    assert!(!dst_dir.join("restore.pending").exists(), "标记要清掉");

    let conn = db::open(&dst_dir.join("updates.db")).unwrap();
    let all = db::products(&conn, false).unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].name, "恢复前的商品");
    drop(conn);

    // 再恢复一次：这次目标目录里已有数据，应该先另存一份 pre-restore
    let conn = db::open(&dst_dir.join("updates.db")).unwrap();
    add_product(&conn, "后来加的");
    let (zip2, _) = crate::backup::build_backup_at(&conn, &dst_dir, "second").unwrap();
    let bytes2 = std::fs::read(&zip2).unwrap();
    drop(conn);
    crate::backup::stage_restore(&dst_dir, &bytes2).unwrap();
    crate::backup::apply_pending(&dst_dir).unwrap();
    let backups: Vec<_> = std::fs::read_dir(dst_dir.join("backups"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    assert!(
        backups.iter().any(|n| n.starts_with("pre-restore-")),
        "恢复前要把现状留一份，实际：{backups:?}"
    );

    let _ = std::fs::remove_dir_all(&src_dir);
    let _ = std::fs::remove_dir_all(&dst_dir);
}

#[test]
fn 不是本系统备份的_zip_会被拒() {
    let dir = tmpdir("restore-bad");
    let mut buf = Vec::new();
    {
        let mut w = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        w.start_file("hello.txt", zip::write::SimpleFileOptions::default()).unwrap();
        std::io::Write::write_all(&mut w, b"not a backup").unwrap();
        w.finish().unwrap();
    }
    let err = crate::backup::stage_restore(&dir, &buf).unwrap_err();
    assert!(err.contains("updates.db"), "错误要说清楚缺什么：{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

/* ------------------------------------------------------------ 一次保存商品 + 规格 */

fn form_of(pairs: &[(&str, &str)]) -> std::collections::HashMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn new_product(name: &str) -> db::NewProduct<'_> {
    db::NewProduct {
        name,
        category: "园艺",
        class_name: "园艺肥料",
        forms_json: r#"["液体"]"#,
        funcs_json: r#"["绿叶肥"]"#,
        usage_text: "稀释 1000 倍",
        image: "",
        listed_at: "",
    }
}

#[test]
fn 表单里按序号解析规格行() {
    let form = form_of(&[
        ("sku_0_id", ""),
        ("sku_0_value", "500"),
        ("sku_0_unit", "ml"),
        ("sku_0_pack", "瓶"),
        ("sku_0_barcode", "6930000000001"),
        ("sku_0_price", "39.9"),
        ("sku_1_id", "7"),
        ("sku_1_barcode", "6930000000002"),
        ("sku_1_del", "1"),
        ("name", "不该被当成规格"),
    ]);
    let rows = crate::products::parse_sku_rows(&form);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].barcode, "6930000000001");
    assert_eq!(rows[0].value, 500.0);
    assert_eq!(rows[0].unit, "ml");
    assert!(rows[0].id.is_none());
    assert_eq!(rows[1].id, Some(7));
    assert!(rows[1].remove);
}

#[test]
fn 保存商品时规格一起落库_空行忽略() {
    let conn = fresh_products();
    let form = form_of(&[
        ("sku_0_value", "500"),
        ("sku_0_unit", "ml"),
        ("sku_0_pack", "瓶"),
        ("sku_0_barcode", "6930000000001"),
        ("sku_0_price", "39.9"),
        ("sku_0_promo", "买2送1"),
        ("sku_1_value", "1"),
        ("sku_1_unit", "L"),
        ("sku_1_pack", "瓶"),
        ("sku_1_barcode", "6930000000002"),
        ("sku_1_price", "69.9"),
        // 第三条是完全空的行（界面上永远留一条）
        ("sku_2_value", ""),
        ("sku_2_barcode", ""),
        ("sku_2_price", ""),
    ]);
    let rows = crate::products::parse_sku_rows(&form);
    let pid = crate::products::save_product_with_skus(&conn, None, &new_product("营养液"), &rows).unwrap();

    let skus = db::skus_of(&conn, pid).unwrap();
    assert_eq!(skus.len(), 2, "空行不该被当成规格");
    assert_eq!(skus[0].barcode69, "6930000000001");
    assert_eq!(skus[0].price_cents, 3990);
    assert_eq!(skus[0].promo, "买2送1");
    assert_eq!(skus[1].spec_value, 1.0);
    assert_eq!(skus[1].spec_unit, "L");
}

#[test]
fn 同一次提交里_69码重复要整体拒掉并且不留商品() {
    let conn = fresh_products();
    let form = form_of(&[
        ("sku_0_barcode", "6930000000001"),
        ("sku_0_price", "10"),
        ("sku_1_barcode", "6930000000001"),
        ("sku_1_price", "20"),
    ]);
    let rows = crate::products::parse_sku_rows(&form);
    let err = crate::products::save_product_with_skus(&conn, None, &new_product("重复码"), &rows)
        .unwrap_err();
    assert!(err.contains("填了两次"), "错误要说清楚：{err}");
    assert!(db::products(&conn, false).unwrap().is_empty(), "失败不能留下半个商品");
}

#[test]
fn 没填69码的规格也能保存() {
    let conn = fresh_products();
    let form = form_of(&[("sku_0_value", "500"), ("sku_0_price", "39.9")]);
    let rows = crate::products::parse_sku_rows(&form);
    let pid = crate::products::save_product_with_skus(&conn, None, &new_product("缺码"), &rows)
        .expect("1.7.8 起 69 码选填，不该被拒");
    let skus = db::skus_of(&conn, pid).unwrap();
    assert_eq!(skus.len(), 1);
    assert_eq!(skus[0].barcode69, "");
    assert_eq!(skus[0].price_cents, 3990);
}

#[test]
fn 空码规格价格填错也要说清是第几条() {
    let conn = fresh_products();
    let form = form_of(&[
        ("sku_0_value", "500"),
        ("sku_0_price", "abc"),
        ("sku_1_value", "1"),
        ("sku_1_price", "20"),
    ]);
    let rows = crate::products::parse_sku_rows(&form);
    let err = crate::products::save_product_with_skus(&conn, None, &new_product("坏价格"), &rows)
        .unwrap_err();
    assert!(err.contains("第 1 条规格"), "没码时也要能定位到行：{err}");
    assert!(err.contains("未填 69 码"), "{err}");
    assert!(err.contains("售价格式不对"), "{err}");
}

#[test]
fn 六九码被别的商品占了要拒() {
    let conn = fresh_products();
    let other = add_product(&conn, "别的商品");
    add_sku(&conn, other, "6930000000009", 1990);

    let form = form_of(&[("sku_0_barcode", "6930000000009"), ("sku_0_price", "19.9")]);
    let rows = crate::products::parse_sku_rows(&form);
    let err = crate::products::save_product_with_skus(&conn, None, &new_product("抢码"), &rows)
        .unwrap_err();
    assert!(err.contains("已被别的规格占用"), "{err}");
}

#[test]
fn 改商品时勾了删的规格会被删掉_其余原样更新() {
    let conn = fresh_products();
    let pid = add_product(&conn, "缓释肥");
    let keep = add_sku(&conn, pid, "6930000000001", 2500);
    let gone = add_sku(&conn, pid, "6930000000002", 4500);

    let form = form_of(&[
        ("sku_0_id", &keep.to_string()),
        ("sku_0_value", "500"),
        ("sku_0_unit", "g"),
        ("sku_0_pack", "袋"),
        ("sku_0_barcode", "6930000000001"),
        ("sku_0_price", "26"),
        ("sku_1_id", &gone.to_string()),
        ("sku_1_barcode", "6930000000002"),
        ("sku_1_del", "1"),
        ("sku_2_barcode", "6930000000003"),
        ("sku_2_price", "60"),
    ]);
    let rows = crate::products::parse_sku_rows(&form);
    crate::products::save_product_with_skus(&conn, Some(pid), &new_product("缓释肥"), &rows).unwrap();

    let skus = db::skus_of(&conn, pid).unwrap();
    let codes: Vec<&str> = skus.iter().map(|s| s.barcode69.as_str()).collect();
    assert_eq!(codes, vec!["6930000000001", "6930000000003"]);
    assert_eq!(skus[0].price_cents, 2600, "改价要生效");
    assert!(!codes.contains(&"6930000000002"), "勾了删的要没了");
}

/* ------------------------------------------------------------ 上架时间 */

#[test]
fn 上架时间只收标准日期_空表示不填() {
    use crate::products::clean_date;
    assert_eq!(clean_date("2026-11-04"), Some("2026-11-04".to_string()));
    assert_eq!(clean_date("  2026-11-04 "), Some("2026-11-04".to_string()));
    assert_eq!(clean_date(""), Some(String::new()));
    assert_eq!(clean_date("   "), Some(String::new()));
    // 斜杠、单位数月份、中文日期一律拒 —— 存进去 Excel 里排序会乱
    assert_eq!(clean_date("2026/11/04"), None);
    assert_eq!(clean_date("2026-1-4"), None);
    assert_eq!(clean_date("2026年11月4日"), None);
    assert_eq!(clean_date("abc"), None);
}

#[test]
fn 老库没有上架时间列_启动时会自动补上() {
    let conn = Connection::open_in_memory().unwrap();
    // 模拟旧版建的 products 表（没有 listed_at）
    conn.execute_batch(
        "CREATE TABLE products (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT NOT NULL,
            category TEXT NOT NULL DEFAULT '',
            class_name TEXT NOT NULL DEFAULT '',
            forms_json TEXT NOT NULL DEFAULT '[]',
            funcs_json TEXT NOT NULL DEFAULT '[]',
            usage_text TEXT NOT NULL DEFAULT '',
            image TEXT NOT NULL DEFAULT '',
            hit_count INTEGER NOT NULL DEFAULT 0,
            last_hit_at INTEGER NOT NULL DEFAULT 0,
            enabled INTEGER NOT NULL DEFAULT 1,
            rev INTEGER NOT NULL DEFAULT 0,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL
        );
        INSERT INTO products(name, created_at, updated_at) VALUES('老商品', 0, 0);",
    )
    .unwrap();

    db::init_products(&conn).unwrap();

    let cols: Vec<String> = {
        let mut stmt = conn.prepare("PRAGMA table_info(products)").unwrap();
        let rows = stmt.query_map([], |r| r.get::<_, String>(1)).unwrap();
        rows.collect::<rusqlite::Result<_>>().unwrap()
    };
    assert!(cols.iter().any(|c| c == "listed_at"), "列没补上：{cols:?}");

    // 老数据的这一列是空串，不是 NULL —— 前端直接当字符串用
    let p = db::product_by_name(&conn, "老商品").unwrap().unwrap();
    assert_eq!(p.listed_at, "");
}

#[test]
fn 上架时间存得住_改商品也不会丢() {
    let conn = fresh_products();
    let pid = db::product_save(
        &conn,
        None,
        &db::NewProduct {
            name: "带日期的商品",
            category: "园艺",
            class_name: "园艺肥料",
            forms_json: "[]",
            funcs_json: "[]",
            usage_text: "",
            image: "",
            listed_at: "2026-03-15",
        },
    )
    .unwrap();

    let p = db::product_by_id(&conn, pid).unwrap().unwrap();
    assert_eq!(p.listed_at, "2026-03-15");

    // 只改名字（模拟编辑保存），日期要原样带回去
    db::product_save(
        &conn,
        Some(pid),
        &db::NewProduct {
            name: "带日期的商品（改名）",
            category: "园艺",
            class_name: "园艺肥料",
            forms_json: "[]",
            funcs_json: "[]",
            usage_text: "",
            image: "",
            listed_at: "2026-03-15",
        },
    )
    .unwrap();
    let p = db::product_by_id(&conn, pid).unwrap().unwrap();
    assert_eq!(p.name, "带日期的商品（改名）");
    assert_eq!(p.listed_at, "2026-03-15");
}

/* ------------------------------------------------------------ 多图（1.7.7） */

fn img(c: char) -> String {
    c.to_string().repeat(16)
}

#[test]
fn 图片列表只留合法hash_去重_最多5张() {
    use crate::products::{images_to_json, normalize_images, parse_images, valid_hash};
    assert!(valid_hash("0123456789abcdef"));
    assert!(!valid_hash("0123456789abcde"), "少一位不行");
    assert!(!valid_hash("../secret"), "路径穿越不行");

    let list = normalize_images([
        img('a'), "../evil".to_string(), img('a'), img('b'),
        img('c'), img('d'), img('e'), img('f'),
    ]);
    assert_eq!(list.len(), crate::products::MAX_IMAGES);
    assert_eq!(list[0], img('a'), "第一张是主图");
    assert!(!list.contains(&img('f')), "第 6 张不收");
    assert_eq!(parse_images(&images_to_json(&list)), list);
    assert_eq!(parse_images("不是 JSON"), Vec::<String>::new(), "坏数据不能让页面崩");
}

#[test]
fn 老库没有多图列_启动时补上并回填单张主图() {
    let conn = Connection::open_in_memory().unwrap();
    // 模拟 1.7.6 的表：只有 image，没有 images_json
    conn.execute_batch(
        "CREATE TABLE products (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT NOT NULL,
            category TEXT NOT NULL DEFAULT '',
            class_name TEXT NOT NULL DEFAULT '',
            forms_json TEXT NOT NULL DEFAULT '[]',
            funcs_json TEXT NOT NULL DEFAULT '[]',
            usage_text TEXT NOT NULL DEFAULT '',
            image TEXT NOT NULL DEFAULT '',
            listed_at TEXT NOT NULL DEFAULT '',
            hit_count INTEGER NOT NULL DEFAULT 0,
            last_hit_at INTEGER NOT NULL DEFAULT 0,
            enabled INTEGER NOT NULL DEFAULT 1,
            rev INTEGER NOT NULL DEFAULT 0,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL
        );
        INSERT INTO products(name, image, created_at, updated_at)
            VALUES('老商品', 'aaaaaaaaaaaaaaaa', 0, 0);",
    )
    .unwrap();

    crate::db::init_products(&conn).unwrap();
    let p = crate::db::product_by_name(&conn, "老商品").unwrap().unwrap();
    assert_eq!(
        crate::products::parse_images(&p.images_json),
        vec![img('a')],
        "老商品原来的那张图要变成数组里的唯一一张，不能丢"
    );
}

#[test]
fn 设多图时主图与增量版本一起更新() {
    let conn = fresh_products();
    let pid = crate::db::product_save(&conn, None, &new_product("多图商品")).unwrap();
    let rev0 = crate::db::product_rev(&conn).unwrap();
    let imgs = vec![img('a'), img('b'), img('c')];
    crate::db::product_set_images(&conn, pid, &imgs).unwrap();

    let p = crate::db::product_by_id(&conn, pid).unwrap().unwrap();
    assert_eq!(crate::products::parse_images(&p.images_json), imgs);
    assert_eq!(p.image, imgs[0], "老客户端读的 image 必须等于第一张");
    assert!(
        crate::db::product_rev(&conn).unwrap() > rev0,
        "改图要推进 rev，客户端才拉得到增量"
    );

    // 设主图：把第三张挪到开头，老客户端看到的 image 也要跟着变
    let mut reordered = imgs.clone();
    let third = reordered.remove(2);
    reordered.insert(0, third.clone());
    crate::db::product_set_images(&conn, pid, &reordered).unwrap();
    let p2 = crate::db::product_by_id(&conn, pid).unwrap().unwrap();
    assert_eq!(p2.image, third);

    // 删光：两列都要回到空
    crate::db::product_set_images(&conn, pid, &[]).unwrap();
    let p3 = crate::db::product_by_id(&conn, pid).unwrap().unwrap();
    assert_eq!(p3.image, "");
    assert_eq!(p3.images_json, "[]");
}

#[test]
fn 用图清单要包含副图_不然清理会误删() {
    let conn = fresh_products();
    let pid = crate::db::product_save(&conn, None, &new_product("多图商品")).unwrap();
    let imgs = vec![img('a'), img('b'), img('c')];
    crate::db::product_set_images(&conn, pid, &imgs).unwrap();
    assert_eq!(crate::db::product_images_used(&conn).unwrap(), imgs);
}

/* ============================================================ 69 码选填（1.7.8） */

#[test]
fn 空69码可以有多条_非空码仍然全局唯一() {
    let conn = fresh_products();
    let pid = add_product(&conn, "没条码的商品");
    let a = add_sku(&conn, pid, "", 1990);
    let b = add_sku(&conn, pid, "", 2990);
    assert_ne!(a, b, "列级 UNIQUE 已经去掉，两条空码不该撞约束");

    let skus = db::skus_of(&conn, pid).unwrap();
    assert_eq!(skus.len(), 2);
    assert!(skus.iter().all(|s| s.barcode69.is_empty()));

    // 空码不参与占用判断，也不能拿空串查出「那一条」
    assert!(!db::sku_barcode_taken(&conn, "", 0).unwrap());
    assert!(db::sku_by_barcode(&conn, "").unwrap().is_none());

    let c = add_sku(&conn, pid, "6930000000077", 3990);
    assert!(db::sku_barcode_taken(&conn, "6930000000077", 0).unwrap());
    assert!(!db::sku_barcode_taken(&conn, "6930000000077", c).unwrap());
    let dup = db::sku_save(
        &conn,
        None,
        &db::NewSku {
            product_id: pid,
            spec_value: 1.0,
            spec_unit: "L",
            spec_pack: "瓶",
            barcode69: "6930000000077",
            price_cents: 100,
            promo: "",
            sort_order: 0,
        },
    );
    assert!(dup.is_err(), "非空码的部分唯一索引要挡住重复");
}

#[test]
fn 表单里多条空码规格能一起保存() {
    let conn = fresh_products();
    let form = form_of(&[
        ("sku_0_value", "500"),
        ("sku_0_unit", "ml"),
        ("sku_0_price", "19.9"),
        ("sku_1_value", "1000"),
        ("sku_1_unit", "ml"),
        ("sku_1_price", "29.9"),
        ("sku_2_barcode", "6930000000088"),
        ("sku_2_price", "39.9"),
    ]);
    let rows = crate::products::parse_sku_rows(&form);
    let pid = crate::products::save_product_with_skus(
        &conn,
        None,
        &new_product("混合规格"),
        &rows,
    )
    .unwrap();
    let skus = db::skus_of(&conn, pid).unwrap();
    assert_eq!(skus.len(), 3);
    assert_eq!(skus.iter().filter(|s| s.barcode69.is_empty()).count(), 2);
    assert_eq!(skus[2].barcode69, "6930000000088");
}

#[test]
fn 空码规格按三元组认人_导出再导入不会越滚越多() {
    let conn = fresh_products();
    let pid = add_product(&conn, "空码商品");
    let id1 = add_sku(&conn, pid, "", 1990);
    let id2 = db::sku_save(
        &conn,
        None,
        &db::NewSku {
            product_id: pid,
            spec_value: 1000.0,
            spec_unit: "ml",
            spec_pack: "瓶",
            barcode69: "",
            price_cents: 2990,
            promo: "",
            sort_order: 1,
        },
    )
    .unwrap();

    // add_sku 的 500ml / ml / 瓶 是第一条
    let hit1 = db::sku_find_blank_by_spec(&conn, pid, 500.0, "ml", "瓶")
        .unwrap()
        .unwrap();
    assert_eq!(hit1.id, id1);
    let hit2 = db::sku_find_blank_by_spec(&conn, pid, 1000.0, "ml", "瓶")
        .unwrap()
        .unwrap();
    assert_eq!(hit2.id, id2);
    assert!(db::sku_find_blank_by_spec(&conn, pid, 2000.0, "ml", "瓶")
        .unwrap()
        .is_none());
    // 不能串到别的商品
    let other = add_product(&conn, "别的商品");
    assert!(db::sku_find_blank_by_spec(&conn, other, 500.0, "ml", "瓶")
        .unwrap()
        .is_none());

    // 模拟导入：找到已有行后覆盖更新，条数不变
    db::sku_save(
        &conn,
        Some(id1),
        &db::NewSku {
            product_id: pid,
            spec_value: 500.0,
            spec_unit: "ml",
            spec_pack: "瓶",
            barcode69: "",
            price_cents: 2590,
            promo: "",
            sort_order: 0,
        },
    )
    .unwrap();
    let skus = db::skus_of(&conn, pid).unwrap();
    assert_eq!(skus.len(), 2, "导入不能重复建空码规格");
    assert_eq!(skus[0].price_cents, 2590);
}

/// 老库的 `barcode69 TEXT NOT NULL UNIQUE` 是列级约束，SQLite 不能 DROP CONSTRAINT，
/// 只能整表重建。这条测试直接造一张 1.7.7 的老表，走一遍 init_products 验证迁移。
#[test]
fn 老库列级UNIQUE会自动迁移_数据不丢且能存多条空码() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE product_skus (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            product_id  INTEGER NOT NULL,
            spec_value  REAL NOT NULL DEFAULT 0,
            spec_unit   TEXT NOT NULL DEFAULT '',
            spec_pack   TEXT NOT NULL DEFAULT '',
            barcode69   TEXT NOT NULL UNIQUE,
            price_cents INTEGER NOT NULL DEFAULT 0,
            promo       TEXT NOT NULL DEFAULT '',
            sort_order  INTEGER NOT NULL DEFAULT 0,
            enabled     INTEGER NOT NULL DEFAULT 1,
            rev         INTEGER NOT NULL DEFAULT 0,
            created_at  INTEGER NOT NULL,
            updated_at  INTEGER NOT NULL
        );
        INSERT INTO product_skus
            (id, product_id, spec_value, spec_unit, spec_pack, barcode69,
             price_cents, promo, sort_order, enabled, rev, created_at, updated_at)
        VALUES (7, 1, 500, 'ml', '瓶', '6930000000001', 3990, '买2送1', 0, 1, 3, 100, 100);
        CREATE INDEX idx_skus_pid ON product_skus(product_id);
        CREATE INDEX idx_skus_rev ON product_skus(rev);",
    )
    .unwrap();

    db::init_products(&conn).unwrap();

    let sql: String = conn
        .query_row(
            "SELECT COALESCE(sql, '') FROM sqlite_master
             WHERE type = 'table' AND name = 'product_skus'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(!sql.contains("UNIQUE"), "迁移后不该再有列级 UNIQUE：{sql}");
    let idx: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type = 'index' AND name = 'idx_skus_barcode'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(idx, 1, "非空码的部分唯一索引要建起来");

    let (id, barcode, cents, promo, rev): (i64, String, i64, String, i64) = conn
        .query_row(
            "SELECT id, barcode69, price_cents, promo, rev FROM product_skus WHERE id = 7",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .unwrap();
    assert_eq!(
        (id, barcode.as_str(), cents, promo.as_str(), rev),
        (7, "6930000000001", 3990, "买2送1", 3),
        "老数据要连 id / rev 一起原样搬过去"
    );

    let pid = add_product(&conn, "迁移后的商品");
    let a = add_sku(&conn, pid, "", 100);
    let b = add_sku(&conn, pid, "", 200);
    assert_ne!(a, b);
    assert!(a > 7 && b > 7, "AUTOINCREMENT 不能从 1 重新开始撞老 id");
    let dup = db::sku_save(
        &conn,
        None,
        &db::NewSku {
            product_id: pid,
            spec_value: 1.0,
            spec_unit: "L",
            spec_pack: "瓶",
            barcode69: "6930000000001",
            price_cents: 100,
            promo: "",
            sort_order: 0,
        },
    );
    assert!(dup.is_err(), "老码的唯一性在迁移后仍然有效");

    // 再初始化一次：不能重复迁移，数据也不能丢
    db::init_products(&conn).unwrap();
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM product_skus", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 3);
}
