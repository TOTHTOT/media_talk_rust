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

/// 一条保存的网络摄像头 (监控页数据源; 密码明文本机存储, 见设计文档风险节)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CameraRow {
    pub id: i64,
    pub name: String,
    pub uri: String,
    pub username: String,
    pub password: String,
    /// 流类型标签: "h264" / "h265" / "jpeg" / "" (未知)
    pub kind: String,
}

/// add_camera 的结果 (同 IP 合并策略, 见 add_camera 注释)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CameraSaveOutcome {
    /// 新设备, 已插入
    Inserted,
    /// 同 IP 已有记录, 新流画质更高 (视频流覆盖 jpeg/未知), 已更新
    Updated,
    /// 同 IP 已有同等或更高画质记录, 未重复添加
    Skipped,
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
            );
            CREATE TABLE IF NOT EXISTS cameras (
                id         INTEGER PRIMARY KEY AUTOINCREMENT,
                name       TEXT NOT NULL,
                uri        TEXT NOT NULL,
                username   TEXT NOT NULL DEFAULT '',
                password   TEXT NOT NULL DEFAULT '',
                kind       TEXT NOT NULL DEFAULT '',
                created_at TEXT NOT NULL DEFAULT (datetime('now', 'localtime'))
            );",
        )
        .context("建表失败")?;
        let store = Self { conn };
        store.migrate_cameras()?;
        Ok(store)
    }

    /// cameras 表老库迁移: 补 kind 列, 合并同 IP 重复行
    /// (ONVIF 搜索保存曾把同一设备的 h264/jpeg 两个 profile 各存一行)
    fn migrate_cameras(&self) -> Result<()> {
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

    /// 全部已保存摄像头, 按名称排序
    pub fn cameras(&self) -> Result<Vec<CameraRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, uri, username, password, kind FROM cameras
             ORDER BY name COLLATE NOCASE, id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(CameraRow {
                id: r.get(0)?,
                name: r.get(1)?,
                uri: r.get(2)?,
                username: r.get(3)?,
                password: r.get(4)?,
                kind: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// 保存摄像头, 同 IP (uri host) 合并: 已存在时画质更高才覆盖
    /// (kind_rank 视频流 > jpeg > 未知), 否则跳过避免一设备多行
    pub fn add_camera(
        &self,
        name: &str,
        uri: &str,
        username: &str,
        password: &str,
        kind: &str,
    ) -> Result<CameraSaveOutcome> {
        if let Some(host) = rtsp_host(uri) {
            let existing = self
                .cameras()?
                .into_iter()
                .find(|c| rtsp_host(&c.uri).as_deref() == Some(host.as_str()));
            if let Some(old) = existing {
                if kind_rank(kind) > kind_rank(&old.kind) {
                    self.conn.execute(
                        "UPDATE cameras SET name=?2, uri=?3, username=?4, password=?5, kind=?6
                         WHERE id=?1",
                        params![old.id, name, uri, username, password, kind],
                    )?;
                    return Ok(CameraSaveOutcome::Updated);
                }
                return Ok(CameraSaveOutcome::Skipped);
            }
        }
        self.conn.execute(
            "INSERT INTO cameras (name, uri, username, password, kind) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![name, uri, username, password, kind],
        )?;
        Ok(CameraSaveOutcome::Inserted)
    }

    pub fn delete_camera(&self, id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM cameras WHERE id = ?1", params![id])?;
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

/// 从 rtsp uri 提取 host (设备 IP): 剥 scheme / userinfo / 端口.
/// 解析失败 (非 rtsp 或畸形) 返回 None, 调用方按"无同设备"处理
fn rtsp_host(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("rtsp://")?;
    let after_auth = rest.rsplit('@').next()?;
    let host = after_auth
        .split([':', '/'])
        .next()
        .filter(|h| !h.is_empty())?;
    Some(host.to_string())
}

/// 流类型画质排序: 视频流 > jpeg 抓图 > 未知
fn kind_rank(kind: &str) -> u8 {
    match kind {
        "h264" | "h265" => 2,
        "jpeg" => 1,
        _ => 0,
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
    fn cameras_crud_roundtrip() {
        let s = mem_store();
        assert!(s.cameras().unwrap().is_empty());

        let r = s
            .add_camera(
                "大门",
                "rtsp://192.168.1.64:554/ch01",
                "admin",
                "123456",
                "h264",
            )
            .unwrap();
        assert_eq!(r, CameraSaveOutcome::Inserted);
        let r2 = s
            .add_camera("后院", "rtsp://192.168.1.65/11", "", "", "")
            .unwrap();
        assert_eq!(r2, CameraSaveOutcome::Inserted);
        let all = s.cameras().unwrap();
        assert_eq!(all.len(), 2);
        let c = all.iter().find(|c| c.name == "大门").unwrap();
        assert_eq!(
            (c.uri.as_str(), c.username.as_str(), c.kind.as_str()),
            ("rtsp://192.168.1.64:554/ch01", "admin", "h264")
        );

        s.delete_camera(all.iter().find(|c| c.name == "后院").unwrap().id)
            .unwrap();
        assert_eq!(s.cameras().unwrap().len(), 1);
    }

    #[test]
    fn cameras_same_host_merged() {
        let s = mem_store();
        // 先存 jpeg 抓图 profile, 再存 h264 视频 profile: 应覆盖更新
        s.add_camera("门", "rtsp://192.168.11.168:8558/ch01", "a", "b", "jpeg")
            .unwrap();
        let r = s
            .add_camera("门", "rtsp://192.168.11.168:8554/ch01", "a", "b", "h264")
            .unwrap();
        assert_eq!(r, CameraSaveOutcome::Updated);
        let all = s.cameras().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].kind, "h264");
        assert_eq!(all[0].uri, "rtsp://192.168.11.168:8554/ch01");

        // 反向: 已有视频流, 再存 jpeg/未知 应跳过
        let r = s
            .add_camera("门", "rtsp://192.168.11.168:8558/ch01", "a", "b", "jpeg")
            .unwrap();
        assert_eq!(r, CameraSaveOutcome::Skipped);
        assert_eq!(s.cameras().unwrap().len(), 1);

        // 不同 IP 不受影响; 同 IP 带凭据的 uri 也认同一台
        let r = s
            .add_camera("新", "rtsp://admin:pw@192.168.11.122:8554/ch01", "", "", "")
            .unwrap();
        assert_eq!(r, CameraSaveOutcome::Inserted);
        assert_eq!(s.cameras().unwrap().len(), 2);
    }

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

    #[test]
    fn rtsp_host_parse() {
        assert_eq!(
            rtsp_host("rtsp://192.168.1.64:554/ch01").as_deref(),
            Some("192.168.1.64")
        );
        assert_eq!(
            rtsp_host("rtsp://admin:pw@192.168.1.64/ch01").as_deref(),
            Some("192.168.1.64")
        );
        assert_eq!(rtsp_host("http://x/1"), None);
        assert_eq!(rtsp_host("rtsp:///ch01"), None);
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
