//! cameras 表 CRUD 与同 IP 合并策略 (rtsp_host / kind_rank 也服务 migrate)

use anyhow::Result;
use rusqlite::params;

use super::Store;

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

impl Store {
    /// 全部已保存摄像头, 按名称排序
    ///
    ///
    ///
    /// returns: Result<Vec<CameraRow>>
    ///
    /// # Examples
    ///
    /// ```
    ///
    /// ```
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
    ///
    ///
    ///
    /// # Arguments
    ///
    /// * `name`: 展示名称
    /// * `uri`: RTSP 流地址 (同 IP 判断取它的 host)
    /// * `username`: 拉流用户名, 无则空串
    /// * `password`: 拉流密码 (明文本机存储), 无则空串
    /// * `kind`: 流类型标签 "h264" / "h265" / "jpeg" / "" (未知)
    ///
    /// returns: Result<CameraSaveOutcome>
    ///
    /// # Examples
    ///
    /// ```
    ///
    /// ```
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

    /// 按 id 删除摄像头; 删不存在不视为错误
    ///
    ///
    ///
    /// # Arguments
    ///
    /// * `id`: 摄像头自增 id
    ///
    /// returns: Result<()>
    ///
    /// # Examples
    ///
    /// ```
    ///
    /// ```
    pub fn delete_camera(&self, id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM cameras WHERE id = ?1", params![id])?;
        Ok(())
    }
}

/// 从 rtsp uri 提取 host (设备 IP): 剥 scheme / userinfo / 端口.
/// 解析失败 (非 rtsp 或畸形) 返回 None, 调用方按"无同设备"处理
pub(super) fn rtsp_host(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("rtsp://")?;
    let after_auth = rest.rsplit('@').next()?;
    let host = after_auth
        .split([':', '/'])
        .next()
        .filter(|h| !h.is_empty())?;
    Some(host.to_string())
}

/// 流类型画质排序: 视频流 > jpeg 抓图 > 未知
pub(super) fn kind_rank(kind: &str) -> u8 {
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
}
