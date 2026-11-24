//! SQLite 持久化: 通讯录 (后续通话记录/消息也落这里, 同库加表即可)
//!
//! 库文件在平台标准数据目录 (`directories`), Windows 为
//! `%APPDATA%/media_talk/media_talk.db`, Linux 为 `~/.local/share/media_talk/`.
//! 数据量小 (几十条联系人), 全部同步操作, 直接在 UI 线程跑.
//!
//! 子模块 cameras=监控摄像头表 CRUD 与同 IP 合并策略; 本文件留 Store 句柄 /
//! 建表 / 通讯录 / 通话记录. 未发布正式版, 不写字段迁移: cameras 旧结构
//! (没有 kind 列) 直接把旧表改名备份, 新建空表.

mod cameras;

pub use cameras::{CameraRow, CameraSaveOutcome};

use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use tracing::info;

/// 当前四张表的建表语句; IF NOT EXISTS, 新库和已建库都可重复执行
const SCHEMA_SQL: &str = "
    CREATE TABLE IF NOT EXISTS contacts (
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
    );
";

/// 建表; 老 cameras 表没有 kind 列时, 改名备份后由 SCHEMA_SQL 建新表
fn ensure_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(SCHEMA_SQL).context("create tables")?;

    let has_kind = conn
        .prepare("PRAGMA table_info(cameras)")?
        .query_map([], |r| r.get::<_, String>(1))?
        .any(|c| c.is_ok_and(|name| name == "kind"));
    if has_kind {
        return Ok(());
    }

    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .context("read system time")?
        .as_millis();
    let legacy = format!("cameras_legacy_{stamp}");
    conn.execute(&format!("ALTER TABLE cameras RENAME TO {legacy}"), [])
        .with_context(|| format!("rename old cameras table to {legacy}"))?;
    conn.execute_batch(SCHEMA_SQL)
        .context("recreate cameras table")?;
    info!(legacy, "old cameras table renamed, new empty table created");
    Ok(())
}

/// 一条通讯录记录
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Contact {
    pub id: i64,
    pub name: String,
    pub number: String,
}

/// 一条通话记录 (与 app::CallRecord 同构, 放这里避免 store 反向依赖 app)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallRecordRow {
    pub outgoing: bool,
    pub missed: bool,
    pub connected: bool,
    pub peer: String,
    /// 结束时刻 "HH:MM" (沿用 UI 展示格式)
    pub time: String,
    pub duration_secs: i64,
}

/// SQLite 句柄: 全库共享一条连接, 各表 CRUD 分散在子模块的 impl 块里
pub struct Store {
    conn: Connection,
}

impl Store {
    /// 打开 (不存在则创建) 数据目录下的库, 并建表
    ///
    ///
    ///
    /// returns: Result<Self>
    ///
    /// # Examples
    ///
    /// ```
    ///
    /// ```
    pub fn open() -> Result<Self> {
        let dir = directories::ProjectDirs::from("", "", "media_talk")
            .context("resolve data directory")?
            .data_dir()
            .to_path_buf();
        std::fs::create_dir_all(&dir).context("create data directory")?;
        let path = dir.join("media_talk.db");
        info!(path = %path.display(), "open store");
        Self::open_at(&path)
    }

    /// 指定路径打开 (测试可传 :memory:)
    ///
    ///
    ///
    /// # Arguments
    ///
    /// * `path`: 库文件路径, 传 `:memory:` 用内存库
    ///
    /// returns: Result<Self>
    ///
    /// # Examples
    ///
    /// ```
    ///
    /// ```
    pub fn open_at(path: &std::path::Path) -> Result<Self> {
        let conn = Connection::open(path).context("open SQLite database")?;
        ensure_schema(&conn)?;
        Ok(Self { conn })
    }

    /// 全部联系人, 按姓名排序
    ///
    ///
    ///
    /// returns: Result<Vec<Contact>>
    ///
    /// # Examples
    ///
    /// ```
    ///
    /// ```
    pub fn contacts(&self) -> Result<Vec<Contact>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, name, number FROM contacts ORDER BY name COLLATE NOCASE, id")?;
        let rows = stmt.query_map([], |r| {
            Ok(Contact {
                id: r.get(0)?,
                name: r.get(1)?,
                number: r.get(2)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// 新增联系人, 返回自增 id
    ///
    ///
    ///
    /// # Arguments
    ///
    /// * `name`: 联系人姓名
    /// * `number`: 联系人号码 (SIP 账号)
    ///
    /// returns: Result<i64>
    ///
    /// # Examples
    ///
    /// ```
    ///
    /// ```
    pub fn add_contact(&self, name: &str, number: &str) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO contacts (name, number) VALUES (?1, ?2)",
            params![name, number],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// 按 id 更新联系人; id 不存在时报错
    ///
    ///
    ///
    /// # Arguments
    ///
    /// * `id`: 联系人自增 id
    /// * `name`: 新姓名
    /// * `number`: 新号码
    ///
    /// returns: Result<()>
    ///
    /// # Examples
    ///
    /// ```
    ///
    /// ```
    pub fn update_contact(&self, id: i64, name: &str, number: &str) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE contacts SET name = ?2, number = ?3 WHERE id = ?1",
            params![id, name, number],
        )?;
        anyhow::ensure!(n == 1, "contact not found: {id}");
        Ok(())
    }

    /// 按 id 删除联系人; 删不存在不视为错误
    ///
    ///
    ///
    /// # Arguments
    ///
    /// * `id`: 联系人自增 id
    ///
    /// returns: Result<()>
    ///
    /// # Examples
    ///
    /// ```
    ///
    /// ```
    pub fn delete_contact(&self, id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM contacts WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// 记一条通话, 顺带做保留策略: 只留最近 200 条
    ///
    ///
    ///
    /// # Arguments
    ///
    /// * `r`: 待写入的通话记录 (created_at 由表默认值生成)
    ///
    /// returns: Result<()>
    ///
    /// # Examples
    ///
    /// ```
    ///
    /// ```
    pub fn add_call_record(&self, r: &CallRecordRow) -> Result<()> {
        self.conn.execute(
            "INSERT INTO call_records (outgoing, missed, connected, peer, time, duration_secs)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                r.outgoing,
                r.missed,
                r.connected,
                r.peer,
                r.time,
                r.duration_secs
            ],
        )?;
        self.conn.execute(
            "DELETE FROM call_records WHERE id NOT IN
             (SELECT id FROM call_records ORDER BY id DESC LIMIT 200)",
            [],
        )?;
        Ok(())
    }

    /// 最近 limit 条通话记录, 新的在前
    ///
    ///
    ///
    /// # Arguments
    ///
    /// * `limit`: 最多返回条数
    ///
    /// returns: Result<Vec<CallRecordRow>>
    ///
    /// # Examples
    ///
    /// ```
    ///
    /// ```
    pub fn recent_call_records(&self, limit: usize) -> Result<Vec<CallRecordRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT outgoing, missed, connected, peer, time, duration_secs
             FROM call_records ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |r| {
            Ok(CallRecordRow {
                outgoing: r.get(0)?,
                missed: r.get(1)?,
                connected: r.get(2)?,
                peer: r.get(3)?,
                time: r.get(4)?,
                duration_secs: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// 读 kv 配置项; 不存在返回 None
    ///
    ///
    ///
    /// # Arguments
    ///
    /// * `key`: 配置键 (如 "weather_city")
    ///
    /// returns: Result<Option<String>> — None 表示未配置
    ///
    /// # Examples
    ///
    /// ```
    ///
    /// ```
    pub fn kv_get(&self, key: &str) -> Result<Option<String>> {
        let mut stmt = self.conn.prepare("SELECT value FROM kv WHERE key = ?1")?;
        let mut rows = stmt.query(params![key])?;
        if let Some(row) = rows.next()? {
            Ok(Some(row.get(0)?))
        } else {
            Ok(None)
        }
    }

    /// 写 kv 配置项 (upsert)
    ///
    ///
    ///
    /// # Arguments
    ///
    /// * `key`: 配置键
    /// * `value`: 配置值
    ///
    /// returns: Result<()>
    ///
    /// # Examples
    ///
    /// ```
    ///
    /// ```
    pub fn kv_set(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO kv (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = ?2",
            params![key, value],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem_store() -> Store {
        Store::open_at(std::path::Path::new(":memory:")).unwrap()
    }

    #[test]
    fn old_cameras_table_is_renamed_and_rebuilt() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("store-cam-{}-{stamp}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("old.db");
        {
            // 旧结构: cameras 没有 kind; kv 里的设置应保留
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE cameras (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    name TEXT NOT NULL, uri TEXT NOT NULL,
                    username TEXT NOT NULL DEFAULT '', password TEXT NOT NULL DEFAULT ''
                );
                INSERT INTO cameras (name, uri) VALUES ('old', 'rtsp://192.168.1.10/ch01');
                CREATE TABLE kv (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                INSERT INTO kv VALUES ('weather_city', 'Xiamen');",
            )
            .unwrap();
        }

        let s = Store::open_at(&path).unwrap();
        assert!(s.cameras().unwrap().is_empty());
        assert_eq!(s.kv_get("weather_city").unwrap().as_deref(), Some("Xiamen"));
        let legacy: String = s
            .conn
            .query_row(
                "SELECT name FROM sqlite_master
                 WHERE type = 'table' AND name LIKE 'cameras_legacy_%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let old_count: i64 = s
            .conn
            .query_row(&format!("SELECT COUNT(*) FROM {legacy}"), [], |r| r.get(0))
            .unwrap();
        assert_eq!(old_count, 1);
        drop(s);

        // 再开一次: schema 已就绪, 不重复改名
        let s = Store::open_at(&path).unwrap();
        let legacy_count: i64 = s
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type = 'table' AND name LIKE 'cameras_legacy_%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(legacy_count, 1);
        drop(s);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn kv_roundtrip() {
        let s = mem_store();
        assert_eq!(s.kv_get("weather_city").unwrap(), None);
        s.kv_set("weather_city", "深圳").unwrap();
        assert_eq!(s.kv_get("weather_city").unwrap().as_deref(), Some("深圳"));
        // upsert: 覆盖不报错
        s.kv_set("weather_city", "广州").unwrap();
        assert_eq!(s.kv_get("weather_city").unwrap().as_deref(), Some("广州"));
    }

    #[test]
    fn crud_roundtrip() {
        let s = mem_store();
        assert!(s.contacts().unwrap().is_empty());

        let id = s.add_contact("门口机", "1000").unwrap();
        let id2 = s.add_contact("管理中心", "9000").unwrap();
        let all = s.contacts().unwrap();
        assert_eq!(all.len(), 2);
        // 按姓名排序: "门口机" < "管理中心" (COLLATE NOCASE 对中文按字节序, 这里仅验证稳定)
        assert!(all.iter().any(|c| c.id == id && c.number == "1000"));
        assert!(all.iter().any(|c| c.id == id2 && c.name == "管理中心"));

        s.update_contact(id, "大门门口机", "1001").unwrap();
        let c = s
            .contacts()
            .unwrap()
            .into_iter()
            .find(|c| c.id == id)
            .unwrap();
        assert_eq!((c.name.as_str(), c.number.as_str()), ("大门门口机", "1001"));

        s.delete_contact(id2).unwrap();
        assert_eq!(s.contacts().unwrap().len(), 1);
    }

    #[test]
    fn update_missing_fails() {
        let s = mem_store();
        assert!(s.update_contact(42, "x", "1").is_err());
        assert!(s.delete_contact(42).is_ok()); // 删不存在不视为错误
    }

    #[test]
    fn call_records_roundtrip() {
        let s = mem_store();
        assert!(s.recent_call_records(20).unwrap().is_empty());
        let r = |peer: &str| CallRecordRow {
            outgoing: true,
            missed: false,
            connected: true,
            peer: peer.into(),
            time: "10:12".into(),
            duration_secs: 31,
        };
        s.add_call_record(&r("1000")).unwrap();
        s.add_call_record(&r("1001")).unwrap();
        let all = s.recent_call_records(20).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].peer, "1001"); // 新的在前
        assert_eq!(all[1].peer, "1000");
        assert_eq!(all[0].duration_secs, 31);
        assert!(all[0].connected && !all[0].missed);
        // limit 生效
        assert_eq!(s.recent_call_records(1).unwrap().len(), 1);
    }
}
