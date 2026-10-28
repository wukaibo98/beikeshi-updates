//! SQLite 存取层。
//!
//! 表结构跟原 Python 版逐字兼容（多了 `client` 列），所以 NAS 上那份现成的
//! updates.db 可以直接拿来用 —— 迁移只加列，不动数据。

use std::path::Path;

use rusqlite::{params, Connection, OptionalExtension, Row};

pub const CLIENTS: [&str; 2] = ["python", "web"];

pub fn label(client: &str) -> &'static str {
    match client {
        "web" => "网页版",
        _ => "Python 版",
    }
}

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
    Ok(())
}

/// 某个客户端当前发布的版本。
pub fn current(conn: &Connection, client: &str) -> rusqlite::Result<Option<Release>> {
    conn.query_row(
        "SELECT * FROM releases WHERE is_current = 1 AND client = ? ORDER BY id DESC LIMIT 1",
        params![client],
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

/// 插入新版本，并把**同一客户端**的旧版本取消"当前"标记。
///
/// 这里必须按 client 收窄：两条版本线互不干扰，发布网页版不该把
/// Python 版的当前版本顶掉。
pub fn insert_release(
    conn: &Connection,
    version: &str,
    filename: &str,
    size: i64,
    sha256: &str,
    notes: &str,
    package: &str,
    client: &str,
) -> rusqlite::Result<()> {
    conn.execute("UPDATE releases SET is_current = 0 WHERE client = ?", params![client])?;
    conn.execute(
        "INSERT INTO releases (version, filename, size, sha256, notes, created_at, downloads, is_current, package, client)
         VALUES (?, ?, ?, ?, ?, ?, 0, 1, ?, ?)",
        params![
            version,
            filename,
            size,
            sha256,
            notes,
            now(),
            package,
            client
        ],
    )?;
    Ok(())
}

pub fn publish(conn: &Connection, id: i64) -> rusqlite::Result<Option<Release>> {
    let Some(rel) = find(conn, id)? else { return Ok(None) };
    conn.execute(
        "UPDATE releases SET is_current = 0 WHERE client = ?",
        params![rel.client],
    )?;
    conn.execute("UPDATE releases SET is_current = 1 WHERE id = ?", params![id])?;
    Ok(Some(rel))
}

/// 删除一条记录；如果删的正好是"当前版本"，把同客户端最新的一条提上来。
pub fn delete(conn: &Connection, id: i64) -> rusqlite::Result<Option<Release>> {
    let Some(rel) = find(conn, id)? else { return Ok(None) };
    conn.execute("DELETE FROM releases WHERE id = ?", params![id])?;
    if rel.is_current {
        let fallback: Option<i64> = conn
            .query_row(
                "SELECT id FROM releases WHERE client = ? ORDER BY id DESC LIMIT 1",
                params![rel.client],
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
