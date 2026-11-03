//! SQLite 存取层。
//!
//! 表结构跟原 Python 版逐字兼容（`client` 列还在），所以 NAS 上那份现成的
//! updates.db 可以直接拿来用 —— 迁移只加列，不动数据。
//!
//! **现在只有一条版本线**：旧 Python 版已停发，新的上传、发布、清单都固定用
//! `CLIENT`。`client` 列留着只为给历史记录打「旧版」标记，以及对旧库做兼容。

use std::path::Path;

use rusqlite::{params, Connection, OptionalExtension, Row};

/// 唯一在发布的客户端标识。清单里的 `client` 字段也是它。
pub const CLIENT: &str = "web";

#[derive(Debug, Clone)]
pub struct Release {
    pub id: i64,
    pub version: String,
    pub filename: String,
    pub size: i64,
    pub sha256: String,
    pub notes: String,
    pub created_at: i64,
    pub downloads: i64,
    pub is_current: bool,
    pub package: String,
    pub client: String,
}

impl Release {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            version: row.get("version")?,
            filename: row.get("filename")?,
            size: row.get("size")?,
            sha256: row.get("sha256")?,
            notes: row.get::<_, Option<String>>("notes")?.unwrap_or_default(),
            created_at: row.get("created_at")?,
            downloads: row.get::<_, Option<i64>>("downloads")?.unwrap_or(0),
            is_current: row.get::<_, Option<i64>>("is_current")?.unwrap_or(0) == 1,
            package: row
                .get::<_, Option<String>>("package")?
                .unwrap_or_else(|| "portable".into()),
            client: row
                .get::<_, Option<String>>("client")?
                .unwrap_or_else(|| "python".into()),
        })
    }
}

pub fn open(db_path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open(db_path)?;
    // WAL：读的时候不挡写（20 个客服同时检查更新也不会互相等）
    let _ = conn.pragma_update(None, "journal_mode", "WAL");
    let _ = conn.pragma_update(None, "synchronous", "NORMAL");
    init(&conn)?;
    Ok(conn)
}

fn init(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS releases (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            version TEXT NOT NULL,
            filename TEXT NOT NULL,
            size INTEGER NOT NULL,
            sha256 TEXT NOT NULL,
            notes TEXT DEFAULT '',
            created_at INTEGER NOT NULL,
            downloads INTEGER DEFAULT 0,
            is_current INTEGER DEFAULT 0,
            package TEXT DEFAULT 'portable',
            client TEXT DEFAULT 'python'
        );
        CREATE TABLE IF NOT EXISTS checks (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            count INTEGER DEFAULT 0,
            last_at INTEGER DEFAULT 0
        );
        INSERT OR IGNORE INTO checks (id, count, last_at) VALUES (1, 0, 0);",
    )?;

    // 老库可能缺列：ALTER 失败就说明已经有了，忽略即可
    for sql in [
        "ALTER TABLE releases ADD COLUMN package TEXT DEFAULT 'portable'",
        "ALTER TABLE releases ADD COLUMN client TEXT DEFAULT 'python'",
    ] {
        let _ = conn.execute(sql, []);
    }

    init_forms(conn)?;
    init_products(conn)?;
    Ok(())
}

/// 当前对外发布的版本（只有一条线）。
pub fn current(conn: &Connection) -> rusqlite::Result<Option<Release>> {
    conn.query_row(
        "SELECT * FROM releases WHERE is_current = 1 AND client = ? ORDER BY id DESC LIMIT 1",
        params![CLIENT],
        Release::from_row,
    )
    .optional()
}

pub fn list_all(conn: &Connection) -> rusqlite::Result<Vec<Release>> {
    let mut stmt = conn.prepare("SELECT * FROM releases ORDER BY id DESC")?;
    let rows = stmt.query_map([], Release::from_row)?;
    rows.collect()
}

pub fn find(conn: &Connection, id: i64) -> rusqlite::Result<Option<Release>> {
    conn.query_row("SELECT * FROM releases WHERE id = ?", params![id], Release::from_row)
        .optional()
}

/// 插入新版本，并把本线的旧版本取消"当前"标记。
///
/// 条件里仍然带 `client = CLIENT`：库里还留着已停发的 Python 版历史记录，
/// 不加这个条件会把它们的历史标记一起清掉（图上看不出来，回滚时会踩）。
pub fn insert_release(
    conn: &Connection,
    version: &str,
    filename: &str,
    size: i64,
    sha256: &str,
    notes: &str,
    package: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE releases SET is_current = 0 WHERE client = ?",
        params![CLIENT],
    )?;
    conn.execute(
        "INSERT INTO releases (version, filename, size, sha256, notes, created_at, downloads, is_current, package, client)
         VALUES (?, ?, ?, ?, ?, ?, 0, 1, ?, ?)",
        params![version, filename, size, sha256, notes, now(), package, CLIENT],
    )?;
    Ok(())
}

/// 发布/回滚的结果。分成「成功」「没这条」「这条是已停发的旧版」三种：
/// 旧版记录不能被切回当前，否则会把新版顶下线、而新版又只能从 web 线取，
/// 结果就是所有人收不到更新。
pub enum PublishResult {
    Published(Release),
    NotFound,
    Legacy(Release),
}

pub fn publish(conn: &Connection, id: i64) -> rusqlite::Result<PublishResult> {
    let Some(rel) = find(conn, id)? else {
        return Ok(PublishResult::NotFound);
    };
    if rel.client != CLIENT {
        return Ok(PublishResult::Legacy(rel));
    }
    conn.execute(
        "UPDATE releases SET is_current = 0 WHERE client = ?",
        params![CLIENT],
    )?;
    conn.execute("UPDATE releases SET is_current = 1 WHERE id = ?", params![id])?;
    Ok(PublishResult::Published(rel))
}

/// 删除一条记录；如果删的正好是"当前版本"，把本线最新的一条提上来。
pub fn delete(conn: &Connection, id: i64) -> rusqlite::Result<Option<Release>> {
    let Some(rel) = find(conn, id)? else { return Ok(None) };
    conn.execute("DELETE FROM releases WHERE id = ?", params![id])?;
    if rel.is_current {
        let fallback: Option<i64> = conn
            .query_row(
                "SELECT id FROM releases WHERE client = ? ORDER BY id DESC LIMIT 1",
                params![CLIENT],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(next) = fallback {
            conn.execute("UPDATE releases SET is_current = 1 WHERE id = ?", params![next])?;
        }
    }
    Ok(Some(rel))
}

pub fn bump_downloads(conn: &Connection, id: i64) {
    let _ = conn.execute(
        "UPDATE releases SET downloads = downloads + 1 WHERE id = ?",
        params![id],
    );
}

pub fn bump_checks(conn: &Connection) {
    let _ = conn.execute(
        "UPDATE checks SET count = count + 1, last_at = ? WHERE id = 1",
        params![now()],
    );
}

pub fn checks_info(conn: &Connection) -> (i64, i64) {
    conn.query_row("SELECT count, last_at FROM checks WHERE id = 1", [], |r| {
        Ok((r.get::<_, i64>(0).unwrap_or(0), r.get::<_, i64>(1).unwrap_or(0)))
    })
    .unwrap_or((0, 0))
}

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/* ==================================================== 表单收集（v0.2 起） */
//
// 三张业务表 + 一张版本表：
//   form_users              姓名白名单（登录只做"在不在名单里"）
//   form_templates          模板头（name / 当前版本 / 是否启用 / 排序）
//   form_template_versions  每个版本的字段定义（快照从这里取，模板不留历史会串列）
//   form_submissions        提交记录（submission_id 唯一 → 幂等）

#[derive(Debug, Clone)]
pub struct FormUser {
    pub id: i64,
    pub name: String,
    pub enabled: bool,
    pub has_password: bool,
    pub auth_version: i64,
}

#[derive(Debug, Clone)]
pub struct FormLoginUser {
    pub id: i64,
    pub name: String,
    pub password_hash: String,
    pub auth_version: i64,
}

#[derive(Debug, Clone)]
pub struct FormTemplate {
    pub id: i64,
    pub name: String,
    pub version: i64,
    pub is_active: bool,
    pub sort_order: i64,
    pub updated_at: i64,
    pub schema_json: String,
    /// 软删除时间（0 = 没删）。删了之后模板不再出现在列表里，
    /// 但这一行还在 —— 历史提交靠它认模板名，删干净了老记录就只剩个 id。
    pub deleted_at: i64,
    /// 原模板名称，供回收站展示与还原。
    pub deleted_name: String,
}

#[derive(Debug, Clone)]
pub struct FormSubmission {
    /// 目前页面没用到，留着是为了将来"删掉某一条"和排查时能对得上
    #[allow(dead_code)]
    pub id: i64,
    pub submission_id: String,
    pub template_id: i64,
    pub template_version: i64,
    pub template_snapshot: String,
    /// 历史兼容列：旧版本永久删除后保留下来的登记可能依靠此列显示表名。
    pub template_name: String,
    /// 同上：筛选用的是 filter.user_id，这里只是把整行读出来
    #[allow(dead_code)]
    pub user_id: i64,
    pub user_name: String,
    pub data_json: String,
    pub client_time: i64,
    pub created_at: i64,
}

pub(crate) fn init_forms(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "        CREATE TABLE IF NOT EXISTS admin_users (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            name       TEXT NOT NULL UNIQUE,
            token_hash TEXT NOT NULL,
            enabled    INTEGER NOT NULL DEFAULT 1,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS form_users (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            name       TEXT NOT NULL UNIQUE,
            password_hash TEXT NOT NULL DEFAULT '',
            auth_version INTEGER NOT NULL DEFAULT 1,
            enabled    INTEGER NOT NULL DEFAULT 1,
            created_at INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS form_templates (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            name       TEXT NOT NULL UNIQUE,
            version    INTEGER NOT NULL DEFAULT 1,
            is_active  INTEGER NOT NULL DEFAULT 1,
            sort_order INTEGER NOT NULL DEFAULT 0,
            updated_at INTEGER NOT NULL,
            deleted_at INTEGER NOT NULL DEFAULT 0,
            deleted_name TEXT NOT NULL DEFAULT '',
            deleted_is_active INTEGER NOT NULL DEFAULT 1
        );
        CREATE TABLE IF NOT EXISTS form_template_versions (
            template_id INTEGER NOT NULL,
            version     INTEGER NOT NULL,
            schema_json TEXT NOT NULL,
            created_at  INTEGER NOT NULL,
            PRIMARY KEY (template_id, version)
        );
        CREATE TABLE IF NOT EXISTS form_submissions (
            id                INTEGER PRIMARY KEY AUTOINCREMENT,
            submission_id     TEXT NOT NULL UNIQUE,
            template_id       INTEGER NOT NULL,
            template_version  INTEGER NOT NULL,
            template_snapshot TEXT NOT NULL,
            template_name     TEXT NOT NULL DEFAULT '',
            user_id           INTEGER NOT NULL,
            user_name         TEXT NOT NULL,
            data_json         TEXT NOT NULL,
            client_time       INTEGER NOT NULL,
            created_at        INTEGER NOT NULL,
            client_id         TEXT NOT NULL DEFAULT ''
        );
        CREATE INDEX IF NOT EXISTS idx_sub_tpl  ON form_submissions(template_id, client_time);
        CREATE INDEX IF NOT EXISTS idx_sub_user ON form_submissions(user_id, client_time);
        CREATE TABLE IF NOT EXISTS activity (
            id        INTEGER PRIMARY KEY AUTOINCREMENT,
            kind      TEXT NOT NULL,
            who       TEXT NOT NULL DEFAULT '',
            ip        TEXT NOT NULL DEFAULT '',
            client_id TEXT NOT NULL DEFAULT '',
            detail    TEXT NOT NULL DEFAULT '',
            at        INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS form_sessions (
            user_id INTEGER NOT NULL,
            client_id TEXT NOT NULL,
            ip TEXT NOT NULL DEFAULT '',
            last_seen INTEGER NOT NULL,
            PRIMARY KEY(user_id, client_id)
        );
        CREATE INDEX IF NOT EXISTS idx_activity_at ON activity(at);",
    )?;

    // 老库补列：ALTER 失败说明已经有了，忽略即可
    for sql in [
        // 模板软删除标记。软删除保留原数据并支持还原；永久删除另行清理关联数据。
        "ALTER TABLE form_templates ADD COLUMN deleted_at INTEGER NOT NULL DEFAULT 0",
        "ALTER TABLE form_templates ADD COLUMN deleted_name TEXT NOT NULL DEFAULT ''",
        "ALTER TABLE form_templates ADD COLUMN deleted_is_active INTEGER NOT NULL DEFAULT 1",
        // 历史兼容列：旧版本永久删除后仍保留的提交记录靠此列显示模板名。
        "ALTER TABLE form_submissions ADD COLUMN template_name TEXT NOT NULL DEFAULT ''",
        // Password login for client accounts. Existing allowlist entries need an admin-set password.
        "ALTER TABLE form_users ADD COLUMN password_hash TEXT NOT NULL DEFAULT ''",
        "ALTER TABLE form_users ADD COLUMN auth_version INTEGER NOT NULL DEFAULT 1",
    ] {
        let _ = conn.execute(sql, []);
    }
    Ok(())
}

/* ---------------------------------------------------------- 用户 */

pub fn form_users(conn: &Connection) -> rusqlite::Result<Vec<FormUser>> {
    let mut stmt = conn.prepare("SELECT id, name, enabled, password_hash, auth_version FROM form_users ORDER BY id")?;
    let rows = stmt.query_map([], |r| {
        Ok(FormUser {
            id: r.get(0)?,
            name: r.get(1)?,
            enabled: r.get::<_, i64>(2)? == 1,
            has_password: !r.get::<_, String>(3)?.is_empty(),
            auth_version: r.get(4)?,
        })
    })?;
    rows.collect()
}

/// 登录用：只看启用的。
#[cfg(test)]
pub fn form_user_by_name(conn: &Connection, name: &str) -> rusqlite::Result<Option<FormUser>> {
    conn.query_row(
        "SELECT id, name, enabled, password_hash, auth_version FROM form_users WHERE name = ? AND enabled = 1",
        params![name],
        |r| Ok(FormUser {
            id: r.get(0)?, name: r.get(1)?, enabled: true,
            has_password: !r.get::<_, String>(3)?.is_empty(), auth_version: r.get(4)?,
        }),
    )
    .optional()
}

pub fn form_user_find(conn: &Connection, id: i64) -> rusqlite::Result<Option<FormUser>> {
    conn.query_row("SELECT id, name, enabled, password_hash, auth_version FROM form_users WHERE id = ?", params![id], |r| {
        Ok(FormUser {
            id: r.get(0)?, name: r.get(1)?, enabled: r.get::<_, i64>(2)? == 1,
            has_password: !r.get::<_, String>(3)?.is_empty(), auth_version: r.get(4)?,
        })
    })
    .optional()
}

#[derive(Debug, Clone)]
pub struct OnlineSession { pub user_id: i64, pub client_id: String, pub ip: String, pub last_seen: i64 }

pub fn touch_form_session(conn: &Connection, user_id: i64, client_id: &str, ip: &str) {
    let _ = conn.execute("INSERT INTO form_sessions(user_id, client_id, ip, last_seen) VALUES(?, ?, ?, ?) ON CONFLICT(user_id, client_id) DO UPDATE SET ip=excluded.ip, last_seen=excluded.last_seen", params![user_id, client_id, ip, now()]);
}

pub fn online_sessions(conn: &Connection) -> Vec<OnlineSession> {
    let mut stmt = match conn.prepare("SELECT user_id, client_id, ip, last_seen FROM form_sessions ORDER BY user_id, client_id") { Ok(s) => s, Err(_) => return Vec::new() };
    stmt.query_map([], |r| Ok(OnlineSession { user_id:r.get(0)?, client_id:r.get(1)?, ip:r.get(2)?, last_seen:r.get(3)? })).and_then(|rows| rows.collect()).unwrap_or_default()
}

/// 新增或改名。名字唯一冲突时返回 Err（调用方翻成人话）。
pub fn form_user_save(conn: &Connection, id: Option<i64>, name: &str, password_hash: Option<&str>) -> rusqlite::Result<i64> {
    match id {
        Some(id) => {
            conn.execute("UPDATE form_users SET name = ?, auth_version = auth_version + 1 WHERE id = ?", params![name, id])?;
            Ok(id)
        }
        None => {
            conn.execute(
                "INSERT INTO form_users (name, password_hash, enabled, created_at) VALUES (?, ?, 1, ?)",
                params![name, password_hash.unwrap_or_default(), now()],
            )?;
            Ok(conn.last_insert_rowid())
        }
    }
}

pub fn form_user_login_by_name(conn: &Connection, name: &str) -> rusqlite::Result<Option<FormLoginUser>> {
    conn.query_row(
        "SELECT id, name, password_hash, auth_version FROM form_users WHERE name = ? AND enabled = 1",
        params![name],
        |r| Ok(FormLoginUser {
            id: r.get(0)?, name: r.get(1)?, password_hash: r.get(2)?, auth_version: r.get(3)?,
        }),
    ).optional()
}

pub fn form_user_set_password(conn: &Connection, id: i64, password_hash: &str) -> rusqlite::Result<bool> {
    let changed = conn.execute(
        "UPDATE form_users SET password_hash = ?, auth_version = auth_version + 1 WHERE id = ?",
        params![password_hash, id],
    )?;
    Ok(changed == 1)
}

pub fn form_user_toggle(conn: &Connection, id: i64, enabled: bool) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE form_users SET enabled = ?, auth_version = auth_version + 1 WHERE id = ?",
        params![if enabled { 1 } else { 0 }, id],
    )?;
    Ok(())
}

/* ---------------------------------------------------------- 模板 */

#[derive(Debug, Clone, PartialEq)]
pub enum TemplateSave {
    Saved { id: i64, version: i64 },
    /// 乐观锁：客户端看到的版本比库里旧
    Conflict { current: i64 },
    NotFound,
    NameTaken,
}

/// 模板列表。**永远不返回已删除的** —— 删掉的模板不该再出现在任何列表里。
pub fn form_templates(conn: &Connection, include_inactive: bool) -> rusqlite::Result<Vec<FormTemplate>> {
    let sql = if include_inactive {
        "SELECT t.id, t.name, t.version, t.is_active, t.sort_order, t.updated_at, v.schema_json, t.deleted_name
           FROM form_templates t
           JOIN form_template_versions v ON v.template_id = t.id AND v.version = t.version
          WHERE t.deleted_at = 0
          ORDER BY t.sort_order, t.id"
    } else {
        "SELECT t.id, t.name, t.version, t.is_active, t.sort_order, t.updated_at, v.schema_json, t.deleted_name
           FROM form_templates t
           JOIN form_template_versions v ON v.template_id = t.id AND v.version = t.version
          WHERE t.is_active = 1 AND t.deleted_at = 0
          ORDER BY t.sort_order, t.id"
    };
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map([], |r| {
        Ok(FormTemplate {
            id: r.get(0)?,
            name: r.get(1)?,
            version: r.get(2)?,
            is_active: r.get::<_, i64>(3)? == 1,
            sort_order: r.get(4)?,
            updated_at: r.get(5)?,
            schema_json: r.get(6)?,
            deleted_at: 0,
            deleted_name: r.get(7)?,
        })
    })?;
    rows.collect()
}

/// 全部模板，含回收站中的模板。管理页用它展示回收站，历史登记也用它解析模板名。
pub fn form_templates_all(conn: &Connection) -> rusqlite::Result<Vec<FormTemplate>> {
    let mut stmt = conn.prepare(
        "SELECT t.id, t.name, t.version, t.is_active, t.sort_order, t.updated_at, v.schema_json, t.deleted_at, t.deleted_name
           FROM form_templates t
           JOIN form_template_versions v ON v.template_id = t.id AND v.version = t.version
          ORDER BY t.sort_order, t.id",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(FormTemplate {
            id: r.get(0)?,
            name: r.get(1)?,
            version: r.get(2)?,
            is_active: r.get::<_, i64>(3)? == 1,
            sort_order: r.get(4)?,
            updated_at: r.get(5)?,
            schema_json: r.get(6)?,
            deleted_at: r.get(7)?,
            deleted_name: r.get(8)?,
        })
    })?;
    rows.collect()
}

pub fn form_template_by_id(conn: &Connection, id: i64) -> rusqlite::Result<Option<FormTemplate>> {
    conn.query_row(
        "SELECT t.id, t.name, t.version, t.is_active, t.sort_order, t.updated_at, v.schema_json, t.deleted_at, t.deleted_name
           FROM form_templates t
           JOIN form_template_versions v ON v.template_id = t.id AND v.version = t.version
          WHERE t.id = ?",
        params![id],
        |r| {
            Ok(FormTemplate {
                id: r.get(0)?,
                name: r.get(1)?,
                version: r.get(2)?,
                is_active: r.get::<_, i64>(3)? == 1,
                sort_order: r.get(4)?,
                updated_at: r.get(5)?,
                schema_json: r.get(6)?,
                deleted_at: r.get(7)?,
                deleted_name: r.get(8)?,
            })
        },
    )
    .optional()
}

/// 取某个版本的字段定义 —— 提交时用它做"冻结快照"，客户端离线几天也不会串列。
pub fn form_template_schema(
    conn: &Connection,
    template_id: i64,
    version: i64,
) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT schema_json FROM form_template_versions WHERE template_id = ? AND version = ?",
        params![template_id, version],
        |r| r.get(0),
    )
    .optional()
}

/// 保存模板：新建 version=1；更新则 version+1 并写入版本表。
///
/// `base_version` 是乐观锁：`None` 表示新建。不匹配返回 Conflict，且**不改库**。
pub fn form_template_save(
    conn: &Connection,
    id: Option<i64>,
    name: &str,
    schema_json: &str,
    base_version: Option<i64>,
    sort_order: i64,
) -> rusqlite::Result<TemplateSave> {
    let tx = conn.unchecked_transaction()?;

    let (id, version) = match id {
        None => {
            let taken: bool = tx
                .query_row("SELECT 1 FROM form_templates WHERE name = ?", params![name], |_| Ok(true))
                .optional()?
                .unwrap_or(false);
            if taken {
                return Ok(TemplateSave::NameTaken);
            }
            tx.execute(
                "INSERT INTO form_templates (name, version, is_active, sort_order, updated_at)
                 VALUES (?, 1, 1, ?, ?)",
                params![name, sort_order, now()],
            )?;
            (tx.last_insert_rowid(), 1i64)
        }
        Some(id) => {
            let current: Option<i64> = tx
                .query_row("SELECT version FROM form_templates WHERE id = ?", params![id], |r| r.get(0))
                .optional()?;
            let Some(current) = current else { return Ok(TemplateSave::NotFound) };

            if base_version != Some(current) {
                return Ok(TemplateSave::Conflict { current });
            }
            let taken: bool = tx
                .query_row(
                    "SELECT 1 FROM form_templates WHERE name = ? AND id <> ?",
                    params![name, id],
                    |_| Ok(true),
                )
                .optional()?
                .unwrap_or(false);
            if taken {
                return Ok(TemplateSave::NameTaken);
            }
            let next = current + 1;
            tx.execute(
                "UPDATE form_templates SET name = ?, version = ?, sort_order = ?, updated_at = ?
                  WHERE id = ?",
                params![name, next, sort_order, now(), id],
            )?;
            (id, next)
        }
    };

    tx.execute(
        "INSERT INTO form_template_versions (template_id, version, schema_json, created_at)
         VALUES (?, ?, ?, ?)",
        params![id, version, schema_json, now()],
    )?;
    tx.commit()?;
    Ok(TemplateSave::Saved { id, version })
}

pub fn form_template_toggle(conn: &Connection, id: i64, is_active: bool) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE form_templates SET is_active = ?, updated_at = ? WHERE id = ?",
        params![if is_active { 1 } else { 0 }, now(), id],
    )?;
    Ok(())
}

/// 移入回收站：保留模板和登记，保存原名及启用状态供还原使用。
pub fn form_template_delete(conn: &Connection, id: i64) -> rusqlite::Result<bool> {
    let changed = conn.execute(
        "UPDATE form_templates
            SET deleted_name = name, deleted_is_active = is_active,
                deleted_at = ?, is_active = 0, updated_at = ?,
                name = substr(name, 1, 24) || '（已删除）'
          WHERE id = ? AND deleted_at = 0",
        params![now(), now(), id],
    )?;
    Ok(changed == 1)
}

#[derive(Debug)]
pub enum RestoreResult { Restored, NotFound, NotDeleted, NameTaken }

/// 从回收站还原模板，不覆盖已经存在的同名模板。
pub fn form_template_restore(conn: &Connection, id: i64) -> rusqlite::Result<RestoreResult> {
    let tx = conn.unchecked_transaction()?;
    let row: Option<(String, String, i64, bool)> = tx.query_row(
        "SELECT name, deleted_name, deleted_at, deleted_is_active FROM form_templates WHERE id = ?",
        params![id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get::<_, i64>(3)? != 0))
    ).optional()?;
    let Some((current_name, saved_name, deleted_at, was_active)) = row else { return Ok(RestoreResult::NotFound) };
    if deleted_at <= 0 { return Ok(RestoreResult::NotDeleted) }
    let original_name = if !saved_name.is_empty() { saved_name } else {
        current_name.strip_suffix("（已删除）").unwrap_or(&current_name).to_string()
    };
    let taken: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM form_templates WHERE name = ? AND id <> ?)",
        params![original_name, id], |r| r.get(0)
    )?;
    if taken { return Ok(RestoreResult::NameTaken) }
    tx.execute("UPDATE form_templates SET name = ?, deleted_name = '', deleted_at = 0, is_active = ?, updated_at = ? WHERE id = ?",
        params![original_name, if was_active { 1 } else { 0 }, now(), id])?;
    tx.commit()?;
    Ok(RestoreResult::Restored)
}

/// 模板是否已删。提交时用：模板删了就不该再收新登记。
pub fn form_template_deleted(conn: &Connection, id: i64) -> rusqlite::Result<bool> {
    let deleted_at: Option<i64> = conn
        .query_row(
            "SELECT deleted_at FROM form_templates WHERE id = ?",
            params![id],
            |r| r.get(0),
        )
        .optional()?;
    Ok(deleted_at.unwrap_or(0) > 0)
}

/// **永久删除**一个已移入回收站的模板及其填写数据。
///
/// 只允许删 `deleted_at > 0` 的（没软删过的要走软删除那条路）。
///
/// 模板行、所有字段版本和填写记录在一个事务中删除。
#[derive(Debug)]
pub enum PurgeResult {
    /// 删掉了，连带删除的填写记录条数
    Purged { submissions: i64 },
    /// 没有这个模板
    NotFound,
    /// 还没软删过 —— 得先走「删除」
    NotDeleted,
}

pub fn form_template_purge(conn: &Connection, id: i64) -> rusqlite::Result<PurgeResult> {
    let tx = conn.unchecked_transaction()?;

    let row: Option<i64> = tx
        .query_row(
            "SELECT deleted_at FROM form_templates WHERE id = ?",
            params![id],
            |r| r.get(0),
        )
        .optional()?;
    let Some(deleted_at) = row else {
        return Ok(PurgeResult::NotFound);
    };
    if deleted_at <= 0 {
        return Ok(PurgeResult::NotDeleted);
    }

    let submissions = tx.execute("DELETE FROM form_submissions WHERE template_id = ?", params![id])? as i64;

    // 字段定义的各版本也跟着清掉
    tx.execute(
        "DELETE FROM form_template_versions WHERE template_id = ?",
        params![id],
    )?;
    tx.execute("DELETE FROM form_templates WHERE id = ?", params![id])?;

    tx.commit()?;
    Ok(PurgeResult::Purged { submissions })
}

/* ---------------------------------------------------------- 提交 */

pub struct NewSubmission<'a> {
    pub submission_id: &'a str,
    pub template_id: i64,
    pub template_version: i64,
    pub template_snapshot: &'a str,
    pub template_name: &'a str,
    pub user_id: i64,
    pub user_name: &'a str,
    pub data_json: &'a str,
    pub client_time: i64,
    pub client_id: &'a str,
}

/// 写入一条提交。返回 true = 新建，false = submission_id 已存在（重试/重复）。
pub fn form_submit_insert(conn: &Connection, s: &NewSubmission<'_>) -> rusqlite::Result<bool> {
    let changed = conn.execute(
        "INSERT INTO form_submissions
           (submission_id, template_id, template_version, template_snapshot, template_name,
            user_id, user_name, data_json, client_time, created_at, client_id)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(submission_id) DO NOTHING",
        params![
            s.submission_id,
            s.template_id,
            s.template_version,
            s.template_snapshot,
            s.template_name,
            s.user_id,
            s.user_name,
            s.data_json,
            s.client_time,
            now(),
            s.client_id
        ],
    )?;
    Ok(changed == 1)
}

#[derive(Default)]
pub struct SubmissionFilter {
    pub template_id: Option<i64>,
    pub user_id: Option<i64>,
    /// 按 client_time（客服填写时间）过滤，闭区间，UTC 秒
    pub from: Option<i64>,
    pub to: Option<i64>,
    pub limit: i64,
    pub offset: i64,
}

pub fn form_submissions(
    conn: &Connection,
    f: &SubmissionFilter,
) -> rusqlite::Result<Vec<FormSubmission>> {
    let mut sql = String::from(
        "SELECT id, submission_id, template_id, template_version, template_snapshot,
                template_name, user_id, user_name, data_json, client_time, created_at
           FROM form_submissions WHERE 1 = 1",
    );
    let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    if let Some(t) = f.template_id {
        sql.push_str(" AND template_id = ?");
        args.push(Box::new(t));
    }
    if let Some(u) = f.user_id {
        sql.push_str(" AND user_id = ?");
        args.push(Box::new(u));
    }
    if let Some(from) = f.from {
        sql.push_str(" AND client_time >= ?");
        args.push(Box::new(from));
    }
    if let Some(to) = f.to {
        sql.push_str(" AND client_time <= ?");
        args.push(Box::new(to));
    }
    sql.push_str(" ORDER BY client_time DESC, id DESC LIMIT ? OFFSET ?");
    args.push(Box::new(if f.limit <= 0 { 50 } else { f.limit }));
    args.push(Box::new(f.offset.max(0)));

    let mut stmt = conn.prepare(&sql)?;
    let refs: Vec<&dyn rusqlite::ToSql> = args.iter().map(|b| b.as_ref()).collect();
    let rows = stmt.query_map(refs.as_slice(), |r| {
        Ok(FormSubmission {
            id: r.get(0)?,
            submission_id: r.get(1)?,
            template_id: r.get(2)?,
            template_version: r.get(3)?,
            template_snapshot: r.get(4)?,
            template_name: r.get(5)?,
            user_id: r.get(6)?,
            user_name: r.get(7)?,
            data_json: r.get(8)?,
            client_time: r.get(9)?,
            created_at: r.get(10)?,
        })
    })?;
    rows.collect()
}

pub fn form_submissions_count(conn: &Connection, f: &SubmissionFilter) -> rusqlite::Result<i64> {
    let mut sql = String::from("SELECT COUNT(*) FROM form_submissions WHERE 1 = 1");
    let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    if let Some(t) = f.template_id {
        sql.push_str(" AND template_id = ?");
        args.push(Box::new(t));
    }
    if let Some(u) = f.user_id {
        sql.push_str(" AND user_id = ?");
        args.push(Box::new(u));
    }
    if let Some(from) = f.from {
        sql.push_str(" AND client_time >= ?");
        args.push(Box::new(from));
    }
    if let Some(to) = f.to {
        sql.push_str(" AND client_time <= ?");
        args.push(Box::new(to));
    }
    let refs: Vec<&dyn rusqlite::ToSql> = args.iter().map(|b| b.as_ref()).collect();
    conn.query_row(&sql, refs.as_slice(), |r| r.get(0))
}

/// 每个模板各有多少条登记（模板管理页上显示"已有 N 条登记"）。
pub fn form_submissions_count_by_template(conn: &Connection) -> rusqlite::Result<Vec<(i64, i64)>> {
    let mut stmt = conn.prepare(
        "SELECT template_id, COUNT(*) FROM form_submissions GROUP BY template_id",
    )?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    rows.collect()
}

/* ------------------------------------------------------ 单条提交（预览/编辑用） */
/// 取一条提交。管理页"表单预览"点进去看到的就是它。
pub fn form_submission_by_id(
    conn: &Connection,
    id: i64,
) -> rusqlite::Result<Option<FormSubmission>> {
    conn.query_row(
        "SELECT id, submission_id, template_id, template_version, template_snapshot,
                template_name, user_id, user_name, data_json, client_time, created_at
           FROM form_submissions WHERE id = ?",
        params![id],
        |r| {
            Ok(FormSubmission {
                id: r.get(0)?,
                submission_id: r.get(1)?,
                template_id: r.get(2)?,
                template_version: r.get(3)?,
                template_snapshot: r.get(4)?,
                template_name: r.get(5)?,
                user_id: r.get(6)?,
                user_name: r.get(7)?,
                data_json: r.get(8)?,
                client_time: r.get(9)?,
                created_at: r.get(10)?,
            })
        },
    )
    .optional()
}

/// 改一条提交的内容（管理页预览时手改）。
///
/// 只改 `data_json`：**字段定义（`template_snapshot`）不动** —— 表格的列是提交那一刻
/// 的模板，改了列就会跟同模板的其他行对不上。
pub fn form_submission_update(
    conn: &Connection,
    id: i64,
    data_json: &str,
) -> rusqlite::Result<bool> {
    let changed = conn.execute(
        "UPDATE form_submissions SET data_json = ? WHERE id = ?",
        params![data_json, id],
    )?;
    Ok(changed == 1)
}

/* ---------------------------------------------------- 管理端人员（admin_users） */

#[derive(Debug, Clone)]
pub struct AdminUser {
    pub id: i64,
    pub name: String,
    /// 口令的 sha256 十六进制。**不存明文**：库文件被人拷走也猜不出口令。
    /// 页面永远不显示它（也显示不出来）。
    #[allow(dead_code)]
    pub token_hash: String,
    pub enabled: bool,
    #[allow(dead_code)]
    pub created_at: i64,
    pub updated_at: i64,
}

pub fn admin_users(conn: &Connection) -> rusqlite::Result<Vec<AdminUser>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, token_hash, enabled, created_at, updated_at FROM admin_users ORDER BY id",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(AdminUser {
            id: r.get(0)?,
            name: r.get(1)?,
            token_hash: r.get(2)?,
            enabled: r.get::<_, i64>(3)? == 1,
            created_at: r.get(4)?,
            updated_at: r.get(5)?,
        })
    })?;
    rows.collect()
}

/// 按口令哈希找人。登录时用 —— 哈希一样就是这个人。
pub fn admin_user_by_hash(
    conn: &Connection,
    token_hash: &str,
) -> rusqlite::Result<Option<AdminUser>> {
    conn.query_row(
        "SELECT id, name, token_hash, enabled, created_at, updated_at
           FROM admin_users WHERE token_hash = ?",
        params![token_hash],
        |r| {
            Ok(AdminUser {
                id: r.get(0)?,
                name: r.get(1)?,
                token_hash: r.get(2)?,
                enabled: r.get::<_, i64>(3)? == 1,
                created_at: r.get(4)?,
                updated_at: r.get(5)?,
            })
        },
    )
    .optional()
}

/// 新建管理端人员。口令先在调用方算成哈希再传进来。
pub fn admin_user_create(
    conn: &Connection,
    name: &str,
    token_hash: &str,
) -> rusqlite::Result<i64> {
    conn.execute(
        "INSERT INTO admin_users (name, token_hash, enabled, created_at, updated_at)
         VALUES (?, ?, 1, ?, ?)",
        params![name, token_hash, now(), now()],
    )?;
    Ok(conn.last_insert_rowid())
}

/// 改口令。`token_hash` 传空串表示"不改口令，只改名"。
pub fn admin_user_update(
    conn: &Connection,
    id: i64,
    name: &str,
    token_hash: Option<&str>,
) -> rusqlite::Result<bool> {
    let changed = match token_hash {
        Some(h) => conn.execute(
            "UPDATE admin_users SET name = ?, token_hash = ?, updated_at = ? WHERE id = ?",
            params![name, h, now(), id],
        )?,
        None => conn.execute(
            "UPDATE admin_users SET name = ?, updated_at = ? WHERE id = ?",
            params![name, now(), id],
        )?,
    };
    Ok(changed == 1)
}

pub fn admin_user_toggle(conn: &Connection, id: i64, enabled: bool) -> rusqlite::Result<bool> {
    let changed = conn.execute(
        "UPDATE admin_users SET enabled = ?, updated_at = ? WHERE id = ?",
        params![if enabled { 1 } else { 0 }, now(), id],
    )?;
    Ok(changed == 1)
}

pub fn admin_user_delete(conn: &Connection, id: i64) -> rusqlite::Result<bool> {
    let changed = conn.execute("DELETE FROM admin_users WHERE id = ?", params![id])?;
    Ok(changed == 1)
}

/* ==================================================== 最近活动（概览页用） */
/// 谁在连服务器。不是"在线状态"——没有心跳，只能记"最后一次请求是什么时候、
/// 从哪个 IP、干了什么"。概览页据此显示"最近活动"，15 分钟内有动作的标在线。
/// 记一条。`who` 为空表示还没登录（只在检查更新）。
pub fn record_activity(
    conn: &Connection,
    kind: &str,
    who: &str,
    ip: &str,
    client_id: &str,
    detail: &str,
) {
    let _ = conn.execute(
        "INSERT INTO activity (kind, who, ip, client_id, detail, at) VALUES (?, ?, ?, ?, ?, ?)",
        params![kind, who, ip, client_id, detail, now()],
    );
    // 顺手清掉 7 天前的：这张表只服务于"最近谁在连"，留历史没意义还会一直长
    let _ = conn.execute("DELETE FROM activity WHERE at < ?", params![now() - 7 * 86400]);
}

/// 概览页顶部那几个数字。
#[derive(Default)]
pub struct FormStats {
    pub templates: i64,
    pub users: i64,
    pub submissions: i64,
    pub submissions_today: i64,
}

pub fn form_stats(conn: &Connection) -> FormStats {
    let count = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap_or(0) };
    let day_start = (now() / 86400) * 86400 - 8 * 3600; // 按北京时间算"今天"
    FormStats {
        templates: count("SELECT COUNT(*) FROM form_templates WHERE is_active = 1"),
        users: count("SELECT COUNT(*) FROM form_users WHERE enabled = 1"),
        submissions: count("SELECT COUNT(*) FROM form_submissions"),
        submissions_today: conn
            .query_row(
                "SELECT COUNT(*) FROM form_submissions WHERE client_time >= ?",
                params![day_start],
                |r| r.get(0),
            )
            .unwrap_or(0),
    }
}

/// 删人。历史提交**不动** —— `user_name` 是冗余存的，删了人记录还认得出是谁填的。
pub fn form_user_delete(conn: &Connection, id: i64) -> rusqlite::Result<bool> {
    let changed = conn.execute("DELETE FROM form_users WHERE id = ?", params![id])?;
    Ok(changed == 1)
}

/* ============================================================ 商品库 */
//
// 两层结构：products（商品，挂图片/话术/分类）+ product_skus（规格，挂 69 码/价格/促销）。
// 一个 69 码只属于一条规格，全局唯一 —— 出库和对账都靠它认货。
//
// 增量同步靠一个全局 rev 计数器（app_meta.product_rev）：任何写操作把计数器 +1 并写进
// 对应行的 rev，客户端带上本地 rev 来问「比我新的」，服务端只回这些行。
// 删除不能靠「查不到」表达，所以另记一张 product_graveyard（kind + ref_id + rev）。

/// 商品与规格的全局版本号存在这张表里（只有一行）
const PRODUCT_REV_KEY: &str = "product_rev";

#[derive(Debug, Clone)]
pub struct ProductRow {
    pub id: i64,
    pub name: String,
    pub category: String,
    pub class_name: String,
    pub forms_json: String,
    pub funcs_json: String,
    pub usage_text: String,
    pub image: String,
    pub hit_count: i64,
    pub last_hit_at: i64,
    pub enabled: bool,
    /// 版本号只用来做增量查询（SQL 里比较），Rust 侧不需要读
    #[allow(dead_code)]
    pub rev: i64,
}

#[derive(Debug, Clone)]
pub struct SkuRow {
    pub id: i64,
    pub product_id: i64,
    pub spec_value: f64,
    pub spec_unit: String,
    pub spec_pack: String,
    pub barcode69: String,
    pub price_cents: i64,
    pub promo: String,
    pub sort_order: i64,
    pub enabled: bool,
    #[allow(dead_code)]
    pub rev: i64,
}

#[derive(Debug, Clone)]
pub struct DictRow {
    pub id: i64,
    pub kind: String,
    pub value: String,
    /// 排序在 SQL 里生效，结构体里留一份便于排查
    #[allow(dead_code)]
    pub sort_order: i64,
    pub enabled: bool,
}

/// 出厂码表。只在 product_dict 为空时灌一次，之后完全由管理页维护。
const SEED_DICT: &[(&str, &[&str])] = &[
    ("category", &["园艺", "农业"]),
    ("class", &["园艺肥料", "园艺农药", "农业肥料", "农业农药"]),
    (
        "form",
        &[
            "液体", "颗粒", "晶体", "粉末", "乳油", "水剂", "悬浮剂", "水乳剂", "悬乳剂",
            "可湿性粉剂", "水分散粒剂", "颗粒剂", "粉剂",
        ],
    ),
    (
        "function",
        &[
            "开花肥", "绿叶肥", "生长肥", "生根肥", "调土肥", "功能肥", "复合肥", "单质肥",
            "杀虫剂", "杀菌剂", "除草剂", "杀螨剂", "杀线虫剂", "杀鼠剂", "杀软体动物剂",
            "植物生长调节剂",
        ],
    ),
    ("unit", &["ml", "L", "g", "kg"]),
    ("pack", &["瓶", "袋", "桶", "包", "支"]),
];

pub(crate) fn init_products(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS products (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            name        TEXT NOT NULL,
            category    TEXT NOT NULL DEFAULT '',
            class_name  TEXT NOT NULL DEFAULT '',
            forms_json  TEXT NOT NULL DEFAULT '[]',
            funcs_json  TEXT NOT NULL DEFAULT '[]',
            usage_text  TEXT NOT NULL DEFAULT '',
            image       TEXT NOT NULL DEFAULT '',
            hit_count   INTEGER NOT NULL DEFAULT 0,
            last_hit_at INTEGER NOT NULL DEFAULT 0,
            enabled     INTEGER NOT NULL DEFAULT 1,
            rev         INTEGER NOT NULL DEFAULT 0,
            created_at  INTEGER NOT NULL,
            updated_at  INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_products_rev ON products(rev);
        CREATE TABLE IF NOT EXISTS product_skus (
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
        CREATE INDEX IF NOT EXISTS idx_skus_pid ON product_skus(product_id);
        CREATE INDEX IF NOT EXISTS idx_skus_rev ON product_skus(rev);
        CREATE TABLE IF NOT EXISTS product_dict (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            kind       TEXT NOT NULL,
            value      TEXT NOT NULL,
            sort_order INTEGER NOT NULL DEFAULT 0,
            enabled    INTEGER NOT NULL DEFAULT 1,
            UNIQUE(kind, value)
        );
        CREATE TABLE IF NOT EXISTS product_graveyard (
            kind   TEXT NOT NULL,
            ref_id INTEGER NOT NULL,
            rev    INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_grave_rev ON product_graveyard(rev);
        CREATE TABLE IF NOT EXISTS app_meta (
            key   TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );",
    )?;

    let seeded: i64 = conn.query_row("SELECT COUNT(*) FROM product_dict", [], |r| r.get(0))?;
    if seeded == 0 {
        for (kind, values) in SEED_DICT {
            for (i, value) in values.iter().enumerate() {
                conn.execute(
                    "INSERT OR IGNORE INTO product_dict(kind, value, sort_order) VALUES(?1, ?2, ?3)",
                    params![kind, value, i as i64],
                )?;
            }
        }
    }
    Ok(())
}

/// 当前商品库版本（客户端拿它判断要不要更新）
pub fn product_rev(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row(
        "SELECT CAST(value AS INTEGER) FROM app_meta WHERE key = ?1",
        params![PRODUCT_REV_KEY],
        |r| r.get(0),
    )
    .optional()
    .map(|v| v.unwrap_or(0))
}

/// 取下一个版本号。`ON CONFLICT DO UPDATE` 在 SQLite 里是原子的，
/// 并发写不会拿到同一个号。
fn product_next_rev(conn: &Connection) -> rusqlite::Result<i64> {
    conn.execute(
        "INSERT INTO app_meta(key, value) VALUES(?1, '1')
         ON CONFLICT(key) DO UPDATE SET value = CAST(value AS INTEGER) + 1",
        params![PRODUCT_REV_KEY],
    )?;
    product_rev(conn)
}

fn product_from_row(row: &Row<'_>) -> rusqlite::Result<ProductRow> {
    Ok(ProductRow {
        id: row.get("id")?,
        name: row.get("name")?,
        category: row.get("category")?,
        class_name: row.get("class_name")?,
        forms_json: row.get("forms_json")?,
        funcs_json: row.get("funcs_json")?,
        usage_text: row.get("usage_text")?,
        image: row.get("image")?,
        hit_count: row.get("hit_count")?,
        last_hit_at: row.get("last_hit_at")?,
        enabled: row.get::<_, i64>("enabled")? == 1,
        rev: row.get("rev")?,
    })
}

fn sku_from_row(row: &Row<'_>) -> rusqlite::Result<SkuRow> {
    Ok(SkuRow {
        id: row.get("id")?,
        product_id: row.get("product_id")?,
        spec_value: row.get("spec_value")?,
        spec_unit: row.get("spec_unit")?,
        spec_pack: row.get("spec_pack")?,
        barcode69: row.get("barcode69")?,
        price_cents: row.get("price_cents")?,
        promo: row.get("promo")?,
        sort_order: row.get("sort_order")?,
        enabled: row.get::<_, i64>("enabled")? == 1,
        rev: row.get("rev")?,
    })
}

const PRODUCT_COLS: &str = "id, name, category, class_name, forms_json, funcs_json, \
     usage_text, image, hit_count, last_hit_at, enabled, rev";
const SKU_COLS: &str = "id, product_id, spec_value, spec_unit, spec_pack, barcode69, \
     price_cents, promo, sort_order, enabled, rev";

pub fn products(conn: &Connection, only_enabled: bool) -> rusqlite::Result<Vec<ProductRow>> {
    let sql = format!(
        "SELECT {PRODUCT_COLS} FROM products {} ORDER BY id",
        if only_enabled { "WHERE enabled = 1" } else { "" }
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], product_from_row)?;
    rows.collect()
}

pub fn product_by_id(conn: &Connection, id: i64) -> rusqlite::Result<Option<ProductRow>> {
    conn.query_row(
        &format!("SELECT {PRODUCT_COLS} FROM products WHERE id = ?1"),
        params![id],
        product_from_row,
    )
    .optional()
}

pub fn skus_of(conn: &Connection, product_id: i64) -> rusqlite::Result<Vec<SkuRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {SKU_COLS} FROM product_skus WHERE product_id = ?1 ORDER BY sort_order, id"
    ))?;
    let rows = stmt.query_map(params![product_id], sku_from_row)?;
    rows.collect()
}

pub struct NewProduct<'a> {
    pub name: &'a str,
    pub category: &'a str,
    pub class_name: &'a str,
    pub forms_json: &'a str,
    pub funcs_json: &'a str,
    pub usage_text: &'a str,
    pub image: &'a str,
}

pub fn product_save(conn: &Connection, id: Option<i64>, p: &NewProduct<'_>) -> rusqlite::Result<i64> {
    let now = now();
    let rev = product_next_rev(conn)?;
    match id {
        Some(id) => {
            conn.execute(
                "UPDATE products SET name=?2, category=?3, class_name=?4, forms_json=?5,
                     funcs_json=?6, usage_text=?7, image=?8, rev=?9, updated_at=?10 WHERE id=?1",
                params![id, p.name, p.category, p.class_name, p.forms_json, p.funcs_json,
                        p.usage_text, p.image, rev, now],
            )?;
            Ok(id)
        }
        None => {
            conn.execute(
                "INSERT INTO products(name, category, class_name, forms_json, funcs_json,
                     usage_text, image, rev, created_at, updated_at)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9)",
                params![p.name, p.category, p.class_name, p.forms_json, p.funcs_json,
                        p.usage_text, p.image, rev, now],
            )?;
            Ok(conn.last_insert_rowid())
        }
    }
}

/// 只改图片（上传/替换/清空都走这里），不动其它字段
pub fn product_set_image(conn: &Connection, id: i64, image: &str) -> rusqlite::Result<()> {
    let rev = product_next_rev(conn)?;
    conn.execute(
        "UPDATE products SET image = ?2, rev = ?3, updated_at = ?4 WHERE id = ?1",
        params![id, image, rev, now()],
    )?;
    Ok(())
}

pub fn product_toggle(conn: &Connection, id: i64, enabled: bool) -> rusqlite::Result<()> {
    let rev = product_next_rev(conn)?;
    conn.execute(
        "UPDATE products SET enabled = ?2, rev = ?3, updated_at = ?4 WHERE id = ?1",
        params![id, if enabled { 1 } else { 0 }, rev, now()],
    )?;
    Ok(())
}

/// 删商品：连规格一起删，并在流水里记两条删除（客户端据此从本地缓存里摘掉）
pub fn product_delete(conn: &Connection, id: i64) -> rusqlite::Result<()> {
    let rev = product_next_rev(conn)?;
    let sku_ids: Vec<i64> = {
        let mut stmt = conn.prepare("SELECT id FROM product_skus WHERE product_id = ?1")?;
        let rows = stmt.query_map(params![id], |r| r.get(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    for sku_id in sku_ids {
        conn.execute(
            "INSERT INTO product_graveyard(kind, ref_id, rev) VALUES('sku', ?1, ?2)",
            params![sku_id, rev],
        )?;
    }
    conn.execute("DELETE FROM product_skus WHERE product_id = ?1", params![id])?;
    conn.execute("DELETE FROM products WHERE id = ?1", params![id])?;
    conn.execute(
        "INSERT INTO product_graveyard(kind, ref_id, rev) VALUES('product', ?1, ?2)",
        params![id, rev],
    )?;
    Ok(())
}

pub struct NewSku<'a> {
    pub product_id: i64,
    pub spec_value: f64,
    pub spec_unit: &'a str,
    pub spec_pack: &'a str,
    pub barcode69: &'a str,
    pub price_cents: i64,
    pub promo: &'a str,
    pub sort_order: i64,
}

/// 69 码是否被**别的**规格占用（同一条自己重存不算冲突）
pub fn sku_barcode_taken(conn: &Connection, barcode: &str, except_sku: i64) -> rusqlite::Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM product_skus WHERE barcode69 = ?1 AND id <> ?2",
        params![barcode, except_sku],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

pub fn sku_save(conn: &Connection, id: Option<i64>, s: &NewSku<'_>) -> rusqlite::Result<i64> {
    let now = now();
    let rev = product_next_rev(conn)?;
    // 商品自己的 rev 也要动：客户端按商品聚合，规格变了商品就得重新下发
    conn.execute(
        "UPDATE products SET rev = ?2, updated_at = ?3 WHERE id = ?1",
        params![s.product_id, rev, now],
    )?;
    match id {
        Some(id) => {
            conn.execute(
                "UPDATE product_skus SET product_id=?2, spec_value=?3, spec_unit=?4,
                     spec_pack=?5, barcode69=?6, price_cents=?7, promo=?8, sort_order=?9,
                     rev=?10, updated_at=?11 WHERE id=?1",
                params![id, s.product_id, s.spec_value, s.spec_unit, s.spec_pack,
                        s.barcode69, s.price_cents, s.promo, s.sort_order, rev, now],
            )?;
            Ok(id)
        }
        None => {
            conn.execute(
                "INSERT INTO product_skus(product_id, spec_value, spec_unit, spec_pack,
                     barcode69, price_cents, promo, sort_order, rev, created_at, updated_at)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10)",
                params![s.product_id, s.spec_value, s.spec_unit, s.spec_pack, s.barcode69,
                        s.price_cents, s.promo, s.sort_order, rev, now],
            )?;
            Ok(conn.last_insert_rowid())
        }
    }
}

pub fn sku_delete(conn: &Connection, id: i64, product_id: i64) -> rusqlite::Result<()> {
    let rev = product_next_rev(conn)?;
    conn.execute("DELETE FROM product_skus WHERE id = ?1", params![id])?;
    conn.execute(
        "INSERT INTO product_graveyard(kind, ref_id, rev) VALUES('sku', ?1, ?2)",
        params![id, rev],
    )?;
    conn.execute(
        "UPDATE products SET rev = ?2, updated_at = ?3 WHERE id = ?1",
        params![product_id, rev, now()],
    )?;
    Ok(())
}

/// 客户端点开商品时上报一次热度。同一商品重复上报只累计次数，
/// 时间戳取最新 —— 客户端用它做「30 天半衰」排序。
pub fn product_hit(conn: &Connection, ids: &[i64]) -> rusqlite::Result<()> {
    let now = now();
    for id in ids {
        conn.execute(
            "UPDATE products SET hit_count = hit_count + 1, last_hit_at = ?2 WHERE id = ?1",
            params![id, now],
        )?;
    }
    Ok(())
}

pub fn product_dict(conn: &Connection) -> rusqlite::Result<Vec<DictRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, kind, value, sort_order, enabled FROM product_dict ORDER BY kind, sort_order, id",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(DictRow {
            id: row.get(0)?,
            kind: row.get(1)?,
            value: row.get(2)?,
            sort_order: row.get(3)?,
            enabled: row.get::<_, i64>(4)? == 1,
        })
    })?;
    rows.collect()
}

pub fn dict_add(conn: &Connection, kind: &str, value: &str) -> rusqlite::Result<()> {
    let next: i64 = conn.query_row(
        "SELECT COALESCE(MAX(sort_order), 0) + 1 FROM product_dict WHERE kind = ?1",
        params![kind],
        |r| r.get(0),
    )?;
    conn.execute(
        "INSERT OR IGNORE INTO product_dict(kind, value, sort_order) VALUES(?1, ?2, ?3)",
        params![kind, value, next],
    )?;
    Ok(())
}

pub fn dict_delete(conn: &Connection, id: i64) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM product_dict WHERE id = ?1", params![id])?;
    Ok(())
}

/// 拖拽排序：整组按前端给的新顺序重排（下标即顺序）
pub fn dict_reorder(conn: &Connection, kind: &str, ids: &[i64]) -> rusqlite::Result<()> {
    for (i, id) in ids.iter().enumerate() {
        conn.execute(
            "UPDATE product_dict SET sort_order = ?3 WHERE id = ?1 AND kind = ?2",
            params![id, kind, i as i64],
        )?;
    }
    Ok(())
}

pub struct ProductSync {
    pub rev: i64,
    pub products: Vec<ProductRow>,
    pub skus: Vec<SkuRow>,
    /// (kind, id)：kind 是 "product" 或 "sku"
    pub deleted: Vec<(String, i64)>,
}

pub fn product_sync(conn: &Connection, since: i64) -> rusqlite::Result<ProductSync> {
    let rev = product_rev(conn)?;
    let mut pstmt = conn.prepare(&format!(
        "SELECT {PRODUCT_COLS} FROM products WHERE rev > ?1 ORDER BY id"
    ))?;
    let products = pstmt
        .query_map(params![since], product_from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut sstmt = conn.prepare(&format!(
        "SELECT {SKU_COLS} FROM product_skus WHERE rev > ?1 ORDER BY product_id, sort_order, id"
    ))?;
    let skus = sstmt
        .query_map(params![since], sku_from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut gstmt =
        conn.prepare("SELECT kind, ref_id FROM product_graveyard WHERE rev > ?1 ORDER BY rev")?;
    let deleted = gstmt
        .query_map(params![since], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    Ok(ProductSync { rev, products, skus, deleted })
}

/// 商品库里引用了哪些图片（留给之后的「清理孤立图片」，现在还没接页面）
#[allow(dead_code)]
pub fn product_images_used(conn: &Connection) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT image FROM products WHERE image <> ''")?;
    let rows = stmt.query_map([], |r| r.get(0))?;
    rows.collect()
}

pub fn product_by_name(conn: &Connection, name: &str) -> rusqlite::Result<Option<ProductRow>> {
    conn.query_row(
        &format!("SELECT {PRODUCT_COLS} FROM products WHERE name = ?1"),
        params![name],
        product_from_row,
    )
    .optional()
}

pub fn sku_by_barcode(conn: &Connection, barcode: &str) -> rusqlite::Result<Option<SkuRow>> {
    conn.query_row(
        &format!("SELECT {SKU_COLS} FROM product_skus WHERE barcode69 = ?1"),
        params![barcode],
        sku_from_row,
    )
    .optional()
}
