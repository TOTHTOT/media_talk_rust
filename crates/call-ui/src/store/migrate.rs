//! 建表 (新库老库幂等) 与 cameras 表老库结构迁移

use anyhow::{Context, Result};
use rusqlite::Connection;
use tracing::info;

use super::Store;
use super::cameras::{CameraRow, kind_rank, rtsp_host};

/// 建表 (IF NOT EXISTS, 新库老库幂等): contacts / call_records / cameras / kv
///
///
///
/// # Arguments
///
/// * `conn`: 已打开的 SQLite 连接
///
/// returns: Result<()>
///
/// # Examples
///
/// ```
///
/// ```
pub(super) fn create_tables(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS contacts (
            id     INTEGER PRIMARY KEY AUTOINCREMENT,
            name   TEXT NOT NULL,
            number TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS call_records (
            id            INTEGER PRIMARY KEY AUTOINCREMENT,
            outgoing      INTEGER NOT NULL,
            missed        INTEGER NOT NULL,
            connected     INTEGER NOT NULL,
            peer          TEXT NOT NULL,
            time          TEXT NOT NULL,
            duration_secs INTEGER NOT NULL,
            created_at    TEXT NOT NULL DEFAULT (datetime('now', 'localtime'))
        );
        CREATE TABLE IF NOT EXISTS cameras (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            name       TEXT NOT NULL,
            uri        TEXT NOT NULL,
            username   TEXT NOT NULL DEFAULT '',
            password   TEXT NOT NULL DEFAULT '',
            kind       TEXT NOT NULL DEFAULT '',
            created_at TEXT NOT NULL DEFAULT (datetime('now', 'localtime'))
        );
        CREATE TABLE IF NOT EXISTS kv (
            key   TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );",
    )
    .context("建表失败")?;
    Ok(())
}

impl Store {
    /// cameras 表老库迁移: 补 kind 列, 合并同 IP 重复行
    /// (ONVIF 搜索保存曾把同一设备的 h264/jpeg 两个 profile 各存一行)
    ///
    ///
    ///
    /// returns: Result<()>
    ///
    /// # Examples
    ///
    /// ```
    ///
    /// ```
    pub(super) fn migrate_cameras(&self) -> Result<()> {
        let has_kind: bool = self
            .conn
            .prepare("PRAGMA table_info(cameras)")?
            .query_map([], |r| r.get::<_, String>(1))?
            .any(|c| c.is_ok_and(|name| name == "kind"));
        if !has_kind {
            self.conn.execute(
                "ALTER TABLE cameras ADD COLUMN kind TEXT NOT NULL DEFAULT ''",
                [],
            )?;
        }
        // 同 IP 重复行: 保留画质最高的 (kind_rank 高者), 并列保留 id 最小的
        let rows = self.cameras()?;
        let mut best: std::collections::HashMap<String, &CameraRow> =
            std::collections::HashMap::new();
        for row in &rows {
            let Some(host) = rtsp_host(&row.uri) else {
                continue;
            };
            let keep = match best.get(&host) {
                Some(old) => {
                    kind_rank(&row.kind) > kind_rank(&old.kind)
                        || (kind_rank(&row.kind) == kind_rank(&old.kind) && row.id < old.id)
                }
                None => true,
            };
            if keep {
                best.insert(host, row);
            }
        }
        for row in &rows {
            let Some(host) = rtsp_host(&row.uri) else {
                continue;
            };
            if best.get(&host).is_some_and(|b| b.id != row.id) {
                self.delete_camera(row.id)?;
                info!(id = row.id, uri = %row.uri, "merged duplicate camera row");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrate_old_cameras_table() {
        // 模拟 kind 列之前的老库: 手工建旧表 + 塞同 IP 重复行, 再 reopen
        let dir = std::env::temp_dir().join(format!("store-mig-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("old.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE cameras (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    name TEXT NOT NULL, uri TEXT NOT NULL,
                    username TEXT NOT NULL DEFAULT '', password TEXT NOT NULL DEFAULT '',
                    created_at TEXT NOT NULL DEFAULT (datetime('now', 'localtime'))
                );
                INSERT INTO cameras (name, uri) VALUES
                    ('门-jpeg', 'rtsp://192.168.11.168:8558/ch01'),
                    ('门-h264', 'rtsp://192.168.11.168:8554/ch01');",
            )
            .unwrap();
        }
        let s = Store::open_at(&path).unwrap();
        let all = s.cameras().unwrap();
        // 迁移后: kind 列存在 (默认可读), 同 IP 合并剩一行
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].kind, "");
        std::fs::remove_dir_all(&dir).ok();
    }
}
