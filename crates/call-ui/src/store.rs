//! SQLite 持久化: 通讯录 (后续通话记录/消息也落这里, 同库加表即可)
//!
//! 库文件在平台标准数据目录 (`directories`), Windows 为
//! `%APPDATA%/media_talk/media_talk.db`, Linux 为 `~/.local/share/media_talk/`.
//! 数据量小 (几十条联系人), 全部同步操作, 直接在 UI 线程跑.

use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use tracing::info;

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

pub struct Store {
    conn: Connection,
}

impl Store {
    /// 打开 (不存在则创建) 数据目录下的库, 并建表
    pub fn open() -> Result<Self> {
        let dir = directories::ProjectDirs::from("", "", "media_talk")
            .context("无法确定数据目录")?
            .data_dir()
            .to_path_buf();
        std::fs::create_dir_all(&dir).context("创建数据目录失败")?;
        let path = dir.join("media_talk.db");
        info!(path = %path.display(), "open store");
        Self::open_at(&path)
    }

    /// 指定路径打开 (测试可传 :memory:)
    pub fn open_at(path: &std::path::Path) -> Result<Self> {
        let conn = Connection::open(path).context("打开 SQLite 失败")?;
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
            );",
        )
        .context("建表失败")?;
        Ok(Self { conn })
    }

    /// 全部联系人, 按姓名排序
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

    pub fn add_contact(&self, name: &str, number: &str) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO contacts (name, number) VALUES (?1, ?2)",
            params![name, number],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn update_contact(&self, id: i64, name: &str, number: &str) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE contacts SET name = ?2, number = ?3 WHERE id = ?1",
            params![id, name, number],
        )?;
        anyhow::ensure!(n == 1, "联系人不存在: {id}");
        Ok(())
    }

    pub fn delete_contact(&self, id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM contacts WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// 记一条通话, 顺带做保留策略: 只留最近 200 条
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem_store() -> Store {
        Store::open_at(std::path::Path::new(":memory:")).unwrap()
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
