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
    db::form_user_save(&conn, None, "张三").unwrap();

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
