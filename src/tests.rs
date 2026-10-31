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
