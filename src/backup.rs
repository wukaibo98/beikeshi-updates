//! 全量备份 / 恢复 / 自动备份。
//!
//! 为什么不用「拷 updates.db 文件」：数据库开的是 WAL，最新数据可能还在 `-wal` 里，
//! 只拷主库会丢一大截（这个坑在手工备份时踩过）。这里统一用 `VACUUM INTO` 生成
//! 一致性快照，得到的是一个自包含、无 WAL 的干净副本。
//!
//! 恢复不能在运行中热替换 SQLite 文件（连接池握着句柄，NAS 上容易锁死），
//! 所以流程是：上传 → 校验 → 解到 `/data/restore/` → 打标记 → 进程退出 →
//! Docker 拉起 → 启动时在打开数据库**之前**完成替换。

use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};

use axum::body::Body;
use axum::extract::{Multipart, Path as AxumPath, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use tokio_util::io::ReaderStream;

use crate::AppState;

pub const RESTORE_MARKER: &str = "restore.pending";
pub const RESTORE_DIR: &str = "restore";
pub const BACKUP_DIR: &str = "backups";
/// 自动备份保留份数
const KEEP_AUTO: usize = 7;

fn backup_dir(data: &Path) -> PathBuf {
    data.join(BACKUP_DIR)
}

fn short_ts(ts: i64, tz_min: i32) -> String {
    let shifted = ts + (tz_min as i64) * 60;
    let days = shifted.div_euclid(86_400);
    let secs = shifted.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}{m:02}{d:02}-{:02}{:02}",
        secs / 3600,
        (secs % 3600) / 60
    )
}

fn pretty_ts(ts: i64, tz_min: i32) -> String {
    let shifted = ts + (tz_min as i64) * 60;
    let days = shifted.div_euclid(86_400);
    let secs = shifted.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}",
        secs / 3600,
        (secs % 3600) / 60
    )
}

/// 从 1970-01-01 起的天数 → 年月日（避免引入 chrono）
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as i64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn sha256_file(path: &Path) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    let mut file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn walk_files(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            walk_files(&path, out)?;
        } else {
            out.push(path);
        }
    }
    Ok(())
}

/// 生成一份备份，返回 zip 路径与大小。
pub fn build_backup(state: &AppState, tag: &str) -> Result<(PathBuf, u64), String> {
    let conn = state.conn.get().map_err(|e| format!("拿连接失败：{e}"))?;
    build_backup_at(&conn, &state.cfg.data_dir, tag)
}

/// 同上，但不依赖 AppState —— 测试和自检都走这个。
pub fn build_backup_at(
    conn: &rusqlite::Connection,
    data: &Path,
    tag: &str,
) -> Result<(PathBuf, u64), String> {
    let dir = backup_dir(data);
    std::fs::create_dir_all(&dir).map_err(|e| format!("建备份目录失败：{e}"))?;

    let tmp = dir.join(format!(".tmp-{tag}"));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).map_err(|e| format!("建临时目录失败：{e}"))?;

    // 1) 一致性快照（VACUUM INTO 会把 WAL 里的内容一起写进去）
    let snapshot = tmp.join("updates.db");
    conn.execute("VACUUM INTO ?1", rusqlite::params![snapshot.to_string_lossy()])
        .map_err(|e| format!("数据库快照失败：{e}"))?;

    // 2) 密钥（恢复后所有人的登录态还能继续用）
    let secret_src = data.join("secret.key");
    let mut files: Vec<(String, PathBuf)> = vec![("updates.db".into(), snapshot.clone())];
    if secret_src.exists() {
        let dst = tmp.join("secret.key");
        std::fs::copy(&secret_src, &dst).map_err(|e| format!("拷密钥失败：{e}"))?;
        files.push(("secret.key".into(), dst));
    }

    // 3) 商品图片
    let img_src = data.join("products").join("img");
    let mut images: Vec<PathBuf> = Vec::new();
    walk_files(&img_src, &mut images).map_err(|e| format!("扫图片失败：{e}"))?;
    for src in &images {
        let name = src.file_name().unwrap_or_default().to_string_lossy().to_string();
        let dst = tmp.join("img").join(&name);
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::copy(src, &dst).map_err(|e| format!("拷图片失败：{e}"))?;
        files.push((format!("img/{name}"), dst));
    }

    // 4) manifest（恢复时校验，防止传了半个包）
    let db_hash = sha256_file(&snapshot)?;
    let secret_hash = if secret_src.exists() {
        sha256_file(&tmp.join("secret.key")).unwrap_or_default()
    } else {
        String::new()
    };
    let manifest = serde_json::json!({
        "app": "bks-updates",
        "version": env!("CARGO_PKG_VERSION"),
        "created_at": crate::db::now(),
        "db_sha256": db_hash,
        "secret_sha256": secret_hash,
        "images": images.len(),
    });
    let manifest_path = tmp.join("manifest.json");
    std::fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    files.push(("manifest.json".into(), manifest_path));

    // 5) 打包
    let zip_path = dir.join(format!("{tag}.zip"));
    let _ = std::fs::remove_file(&zip_path);
    let file = std::fs::File::create(&zip_path).map_err(|e| format!("建 zip 失败：{e}"))?;
    let mut writer = zip::ZipWriter::new(file);
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .large_file(true);
    for (name, path) in &files {
        writer
            .start_file(name.clone(), opts)
            .map_err(|e| format!("写 zip 失败：{e}"))?;
        let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
        writer.write_all(&bytes).map_err(|e| e.to_string())?;
    }
    writer.finish().map_err(|e| format!("封 zip 失败：{e}"))?;
    let _ = std::fs::remove_dir_all(&tmp);
    let size = std::fs::metadata(&zip_path).map(|m| m.len()).unwrap_or(0);
    Ok((zip_path, size))
}

/// 只保留最近 N 份自动备份（手动备份不删）
pub fn prune_auto(data: &Path) -> Result<(), String> {
    let dir = backup_dir(data);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Ok(());
    };
    let mut autos: Vec<(i64, PathBuf)> = entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            let name = path.file_name()?.to_string_lossy().to_string();
            if !name.starts_with("auto-") || !name.ends_with(".zip") {
                return None;
            }
            let mtime = e.metadata().ok()?.modified().ok()?;
            let ts = mtime
                .duration_since(std::time::UNIX_EPOCH)
                .ok()
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            Some((ts, path))
        })
        .collect();
    autos.sort_by_key(|(ts, _)| *ts);
    while autos.len() > KEEP_AUTO {
        let (_, path) = autos.remove(0);
        let _ = std::fs::remove_file(path);
    }
    Ok(())
}

pub struct BackupEntry {
    pub name: String,
    pub size: u64,
    pub at: i64,
}

pub fn list_backups(data: &Path) -> Vec<BackupEntry> {
    let Ok(entries) = std::fs::read_dir(backup_dir(data)) else {
        return Vec::new();
    };
    let mut out: Vec<BackupEntry> = entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            let name = path.file_name()?.to_string_lossy().to_string();
            if !name.ends_with(".zip") || name.starts_with('.') {
                return None;
            }
            let meta = e.metadata().ok()?;
            let at = meta
                .modified()
                .ok()
                .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            Some(BackupEntry { name, size: meta.len(), at })
        })
        .collect();
    out.sort_by(|a, b| b.at.cmp(&a.at));
    out
}

/* ============================================================ 启动时应用 */

/// 启动时（**打开数据库之前**）检查有没有待恢复的备份。
pub fn apply_pending(data: &Path) -> Result<(), String> {
    let marker = data.join(RESTORE_MARKER);
    if !marker.exists() {
        return Ok(());
    }
    let staging = data.join(RESTORE_DIR);
    let staged_db = staging.join("updates.db");
    if !staged_db.exists() {
        let _ = std::fs::remove_file(&marker);
        return Err("待恢复的备份里没有数据库，已跳过".into());
    }

    // 把当前状态先收起来：恢复点错了还能手工找回来
    let keep = backup_dir(data).join(format!("pre-restore-{}", short_ts(crate::db::now(), 480)));
    let _ = std::fs::create_dir_all(&keep);
    for name in ["updates.db", "updates.db-wal", "updates.db-shm", "secret.key"] {
        let src = data.join(name);
        if src.exists() {
            let _ = std::fs::copy(&src, keep.join(name));
        }
    }

    // 旧的 WAL/SHM 必须删掉，否则会盖在刚恢复的主库上，把数据弄乱
    for name in ["updates.db-wal", "updates.db-shm"] {
        let _ = std::fs::remove_file(data.join(name));
    }
    std::fs::copy(&staged_db, data.join("updates.db"))
        .map_err(|e| format!("替换数据库失败：{e}"))?;

    let staged_secret = staging.join("secret.key");
    if staged_secret.exists() {
        std::fs::copy(&staged_secret, data.join("secret.key"))
            .map_err(|e| format!("替换密钥失败：{e}"))?;
    }

    let staged_img = staging.join("img");
    if staged_img.exists() {
        let dst = data.join("products").join("img");
        std::fs::create_dir_all(&dst).map_err(|e| e.to_string())?;
        let mut files = Vec::new();
        walk_files(&staged_img, &mut files).map_err(|e| e.to_string())?;
        for src in files {
            if let Some(name) = src.file_name() {
                let _ = std::fs::copy(&src, dst.join(name));
            }
        }
    }

    let _ = std::fs::remove_dir_all(&staging);
    let _ = std::fs::remove_file(&marker);
    Ok(())
}

/* ============================================================ HTTP */

fn back(ok: Option<&str>, err: Option<&str>) -> Response {
    let mut url = "/admin?tab=backup".to_string();
    if let Some(m) = ok {
        url.push_str(&format!("&ok={}", crate::admin::encode(m)));
    }
    if let Some(m) = err {
        url.push_str(&format!("&err={}", crate::admin::encode(m)));
    }
    Redirect::to(&url).into_response()
}

/// 一键下载全量备份
pub async fn download(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !crate::is_admin(&headers, &state) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let tag = format!("manual-{}", short_ts(crate::db::now(), state.cfg.tz_offset_min));
    let built = {
        let state = state.clone();
        let tag = tag.clone();
        tokio::task::spawn_blocking(move || build_backup(&state, &tag)).await
    };
    let (path, size) = match built {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("中断：{e}")).into_response(),
    };
    let Ok(file) = tokio::fs::File::open(&path).await else {
        return (StatusCode::INTERNAL_SERVER_ERROR, "备份文件读不到").into_response();
    };
    let filename = path.file_name().unwrap_or_default().to_string_lossy().to_string();
    (
        [
            (header::CONTENT_TYPE, "application/zip".to_string()),
            (header::CONTENT_LENGTH, size.to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{filename}\""),
            ),
        ],
        Body::from_stream(ReaderStream::new(file)),
    )
        .into_response()
}

/// 下载 backups/ 里已有的某一份
pub async fn download_file(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(name): AxumPath<String>,
) -> Response {
    if !crate::is_admin(&headers, &state) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if !name.ends_with(".zip") || !name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')) {
        return (StatusCode::BAD_REQUEST, "文件名不合法").into_response();
    }
    let path = backup_dir(&state.cfg.data_dir).join(&name);
    let Ok(file) = tokio::fs::File::open(&path).await else {
        return (StatusCode::NOT_FOUND, "没有这份备份").into_response();
    };
    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    (
        [
            (header::CONTENT_TYPE, "application/zip".to_string()),
            (header::CONTENT_LENGTH, size.to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{name}\""),
            ),
        ],
        Body::from_stream(ReaderStream::new(file)),
    )
        .into_response()
}

pub async fn delete_file(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::Form(form): axum::Form<std::collections::HashMap<String, String>>,
) -> Response {
    if !crate::is_admin(&headers, &state) {
        return back(None, Some("登录已过期"));
    }
    let name = form.get("name").cloned().unwrap_or_default();
    if !name.ends_with(".zip") || name.contains('/') {
        return back(None, Some("文件名不合法"));
    }
    let path = backup_dir(&state.cfg.data_dir).join(&name);
    match std::fs::remove_file(&path) {
        Ok(_) => back(Some("已删除这份备份"), None),
        Err(e) => back(None, Some(&format!("删除失败：{e}"))),
    }
}

/// 上传备份并进入「待重启」状态
pub async fn restore(
    State(state): State<AppState>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> Response {
    if !crate::is_admin(&headers, &state) {
        return back(None, Some("登录已过期"));
    }
    let mut zip_bytes: Option<Vec<u8>> = None;
    let mut confirm = String::new();
    while let Ok(Some(field)) = multipart.next_field().await {
        match field.name().unwrap_or("") {
            "confirm" => confirm = field.text().await.unwrap_or_default(),
            "file" => {
                if let Ok(b) = field.bytes().await {
                    zip_bytes = Some(b.to_vec());
                }
            }
            _ => {}
        }
    }
    if confirm.trim() != "RESTORE" {
        return back(None, Some("确认框要一字不差输入 RESTORE"));
    }
    let Some(bytes) = zip_bytes else {
        return back(None, Some("没有收到备份文件"));
    };

    let data = state.cfg.data_dir.clone();
    let result = tokio::task::spawn_blocking(move || stage_restore(&data, &bytes)).await;
    match result {
        Ok(Ok(count)) => back(
            Some(&format!("备份已解压就绪（{count} 个文件）。点下面的「重启并恢复」才会生效")),
            None,
        ),
        Ok(Err(e)) => back(None, Some(&e)),
        Err(e) => back(None, Some(&format!("中断：{e}"))),
    }
}

/// 把 zip 解到 /data/restore/，校验 manifest 与数据库哈希，最后打标记
/// （pub 是为了让测试能直接调，不用起 HTTP）
pub fn stage_restore(data: &Path, bytes: &[u8]) -> Result<usize, String> {
    let mut archive =
        zip::ZipArchive::new(Cursor::new(bytes)).map_err(|e| format!("这不是一个 zip：{e}"))?;

    let staging = data.join(RESTORE_DIR);
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).map_err(|e| format!("建暂存目录失败：{e}"))?;

    let mut count = 0usize;
    for i in 0..archive.len() {
        let mut file = archive.by_index(i).map_err(|e| format!("读 zip 失败：{e}"))?;
        if file.is_dir() {
            continue;
        }
        let raw = file.name().to_string();
        // 只允许白名单路径，挡掉 ../ 逃逸
        if raw.contains("..") || raw.starts_with('/') {
            continue;
        }
        let dst = staging.join(&raw);
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let mut out = std::fs::File::create(&dst).map_err(|e| format!("写文件失败：{e}"))?;
        std::io::copy(&mut file, &mut out).map_err(|e| format!("解压失败：{e}"))?;
        count += 1;
    }

    let manifest_path = staging.join("manifest.json");
    let db_path = staging.join("updates.db");
    if !db_path.exists() {
        return Err("这个包里没有 updates.db，不是本系统的备份".into());
    }
    if manifest_path.exists() {
        let text = std::fs::read_to_string(&manifest_path).map_err(|e| e.to_string())?;
        let manifest: serde_json::Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
        if let Some(expected) = manifest.get("db_sha256").and_then(|v| v.as_str()) {
            if !expected.is_empty() {
                let actual = sha256_file(&db_path)?;
                if actual != expected {
                    let _ = std::fs::remove_dir_all(&staging);
                    return Err("数据库校验不通过（备份可能传坏了），已中止".into());
                }
            }
        }
    }

    std::fs::write(data.join(RESTORE_MARKER), b"pending")
        .map_err(|e| format!("写标记失败：{e}"))?;
    Ok(count)
}

/// 让进程退出，由 Docker 的 restart 策略拉起 —— 恢复在启动时完成
pub async fn restart(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !crate::is_admin(&headers, &state) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let pending = state.cfg.data_dir.join(RESTORE_MARKER).exists();
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        std::process::exit(0);
    });
    let msg = if pending {
        "正在重启并恢复备份，约 10 秒后刷新这个页面"
    } else {
        "正在重启服务，约 10 秒后刷新"
    };
    back(Some(msg), None)
}

/// 每天 03:00 自动备份一次（按 `tz_offset_min` 算本地时间）。
/// 不用长 sleep 精确等待：每 10 分钟醒一次看「今天 3 点这一档做过没有」，
/// 这样机器休眠、时钟跳变都不会漏。
pub fn spawn_auto_backup(state: AppState) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(600)).await;
            let tz = state.cfg.tz_offset_min as i64;
            let local = crate::db::now() + tz * 60;
            let hour = local.rem_euclid(86_400) / 3600;
            if hour != 3 {
                continue;
            }
            let tag = format!("auto-{}", short_ts(crate::db::now(), tz as i32));
            let marker = backup_dir(&state.cfg.data_dir).join(format!("{tag}.zip"));
            if marker.exists() {
                continue;
            }
            let st = state.clone();
            let tag2 = tag.clone();
            let done = tokio::task::spawn_blocking(move || build_backup(&st, &tag2)).await;
            match done {
                Ok(Ok((path, size))) => {
                    println!("自动备份完成：{} ({} 字节)", path.display(), size);
                    let _ = prune_auto(&state.cfg.data_dir);
                }
                Ok(Err(e)) => eprintln!("自动备份失败：{e}"),
                Err(e) => eprintln!("自动备份线程中断：{e}"),
            }
        }
    });
}

/* ============================================================ 页面 */

fn human_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / 1024.0 / 1024.0)
    } else if bytes >= 1024 {
        format!("{:.0} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

pub fn pane(state: &AppState) -> String {
    let data = &state.cfg.data_dir;
    let entries = list_backups(data);
    let tz = state.cfg.tz_offset_min;
    let total: u64 = entries.iter().map(|e| e.size).sum();
    let auto_count = entries.iter().filter(|e| e.name.starts_with("auto-")).count();

    let rows = entries
        .iter()
        .map(|e| {
            format!(
                r##"<tr>
  <td>{name}</td>
  <td class="muted">{size}</td>
  <td class="muted">{at}</td>
  <td style="white-space:nowrap">
    <a class="ghost" href="/admin/backup/file/{name}">下载</a>
    <form method="post" action="/admin/backup/delete" style="display:inline" onsubmit="return confirm('删除这份备份？')">
      <input type="hidden" name="name" value="{name}">
      <button class="ghost" type="submit">删除</button>
    </form>
  </td>
</tr>"##,
                name = crate::escape(&e.name),
                size = human_size(e.size),
                at = pretty_ts(e.at, tz),
            )
        })
        .collect::<String>();
    let rows = if rows.is_empty() {
        r#"<tr><td colspan="4" class="muted">还没有备份文件</td></tr>"#.to_string()
    } else {
        rows
    };

    format!(
        r##"<div class="card">
  <h2>备份与恢复<span class="muted">数据库 + 密钥 + 商品图片，打成一个 zip</span></h2>
  <p class="muted" style="line-height:1.8">
    备份内容是<strong>全量</strong>：客服账号、表单模板与登记、商品库、图片、登录密钥。<br>
    恢复会把这些<strong>整体退回</strong>到备份那一刻 —— 恢复前的现状会先另存一份到 backups/pre-restore-*。
  </p>
  <div class="row">
    <a class="ghost" href="/admin/backup/download">立即备份并下载</a>
    <span class="muted">共 {n} 份（其中自动 {auto} 份）· 合计 {total}</span>
  </div>
</div>

<div class="card">
  <h2>从备份恢复<span class="muted">上传 zip → 核对 → 重启后生效</span></h2>
  <form method="post" action="/admin/backup/restore" enctype="multipart/form-data">
    <table>
      <tr><th style="width:110px">备份文件</th><td><input type="file" name="file" accept=".zip" required></td></tr>
      <tr><th>确认</th><td>
        <input name="confirm" placeholder="输入 RESTORE" style="max-width:200px" required>
        <span class="muted" style="margin-left:8px">恢复会覆盖现有数据，输入大写 RESTORE 才执行</span>
      </td></tr>
    </table>
    <div class="row" style="margin-top:12px"><button type="submit">上传并校验</button></div>
  </form>
  <form method="post" action="/admin/backup/restart" style="margin-top:12px">
    <button class="ghost" type="submit">重启服务（应用待恢复的备份 / 排障用）</button>
  </form>
</div>

<div class="card">
  <h2>备份文件<span class="muted">自动备份每天 03:00，保留最近 7 份</span></h2>
  <table>
    <tr><th>文件</th><th style="width:90px">大小</th><th style="width:150px">时间</th><th style="width:140px"></th></tr>
    {rows}
  </table>
  <p class="muted" style="margin-top:12px">
    提醒：备份和数据库在同一块盘上，整机故障会一起没。建议定期用「立即备份并下载」存一份到电脑或网盘。
  </p>
</div>"##,
        n = entries.len(),
        auto = auto_count,
        total = human_size(total),
        rows = rows,
    )
}
