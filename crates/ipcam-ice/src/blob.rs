//! ICE blob: libnice 原生 SDP 文本的薄包装.
//!
//! blob 在对端之间原样搬运 (WS JSON 或剪贴板), 进出库走
//! `nice_agent_generate_local_sdp` / `nice_agent_parse_remote_sdp`,
//! 本模块只做候选行解析供 UI 状态展示.

use std::net::IpAddr;

/// 对端搬运的 ICE 信令单元 (libnice SDP 文本).
#[derive(Debug, Clone)]
pub struct IceBlob(String);

impl IceBlob {
    pub(crate) fn new_unchecked(sdp: String) -> Self {
        Self(sdp)
    }

    /// 从对端搬运来的 SDP 文本构造 (WS payload / 手动导入路径).
    /// 不做解析校验 — 合法性在对端 blob 经 [`crate::IceAgent::add_remote`]
    /// 喂入时被 libnice 检查. `new_unchecked` 的公开版, 供 call-ui 等
    /// crate 的消费侧使用 (库内生产路径仍走 `new_unchecked`).
    pub fn from_sdp(sdp: String) -> Self {
        Self(sdp)
    }

    /// 原始 SDP 文本 (WS/剪贴板搬运载体).
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// 解析 `a=candidate:` 行 (RFC 5245 语法) 供状态展示; 坏行跳过不 panic.
    pub fn candidates(&self) -> Vec<CandidateLine> {
        self.0
            .lines()
            .filter_map(|line| CandidateLine::parse(line.trim()).ok())
            .collect()
    }
}

/// 一条解析后的候选 (仅展示用, 不参与 ICE 逻辑).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateLine {
    pub foundation: String,
    pub component: u32,
    pub priority: u32,
    pub address: IpAddr,
    pub port: u16,
    pub kind: CandKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandKind {
    Host,
    Srflx,
    Relay,
}

impl CandidateLine {
    /// 解析单行 `a=candidate:foundation component transport priority ip port typ kind`.
    ///
    /// # Arguments
    ///
    /// * `line`: SDP 属性行, 可带 `a=` 前缀.
    ///
    /// returns: Result<CandidateLine, BlobError>
    ///
    /// # Examples
    ///
    /// ```
    ///
    /// ```
    fn parse(line: &str) -> Result<Self, BlobError> {
        let line = line.strip_prefix("a=candidate:").unwrap_or(line);
        let f: Vec<&str> = line.split_whitespace().collect();
        // 最小字段: foundation comp transport priority addr port typ kind
        if f.len() < 8 {
            return Err(BlobError::BadCandidateLine(line.into()));
        }
        let parse_kind = |s: &str| match s {
            "host" => Ok(CandKind::Host),
            "srflx" => Ok(CandKind::Srflx),
            "relay" => Ok(CandKind::Relay),
            other => Err(BlobError::BadCandidateLine(format!("typ {other}"))),
        };
        Ok(Self {
            foundation: f[0].into(),
            component: f[1]
                .parse()
                .map_err(|_| BlobError::BadCandidateLine(line.into()))?,
            priority: f[3]
                .parse()
                .map_err(|_| BlobError::BadCandidateLine(line.into()))?,
            address: f[4]
                .parse()
                .map_err(|_| BlobError::BadCandidateLine(line.into()))?,
            port: f[5]
                .parse()
                .map_err(|_| BlobError::BadCandidateLine(line.into()))?,
            kind: parse_kind(f[7])?,
        })
    }
}

/// blob 文本解析错误 (展示路径, 非 ICE 控制路径).
#[derive(Debug, thiserror::Error)]
pub enum BlobError {
    #[error("bad candidate line: {0}")]
    BadCandidateLine(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "v=0\r\n\
        m=video 1 RTP/AVP 96\r\n\
        a=ice-ufrag:abcd\r\n\
        a=ice-pwd:efghEFGHefghEFGHefgh\r\n\
        a=candidate:1 1 UDP 2013266431 192.168.1.5 51234 typ host\r\n\
        a=candidate:2 1 UDP 2013266430 118.31.0.10 37891 typ srflx raddr 192.168.1.5 rport 51234\r\n";

    #[test]
    fn parses_candidate_lines() {
        let blob = IceBlob::new_unchecked(SAMPLE.into());
        let cands = blob.candidates();
        assert_eq!(cands.len(), 2);
        assert_eq!(cands[0].kind, CandKind::Host);
        assert_eq!(cands[0].address, "192.168.1.5".parse::<IpAddr>().unwrap());
        assert_eq!(cands[0].port, 51234);
        assert_eq!(cands[1].kind, CandKind::Srflx);
        assert_eq!(cands[1].foundation, "2");
    }

    #[test]
    fn ignores_unrelated_lines() {
        let blob = IceBlob::new_unchecked("v=0\r\na=ice-pwd:x\r\n".into());
        assert!(blob.candidates().is_empty());
    }

    #[test]
    fn skips_malformed_candidate_line() {
        // 缺 port 的行不产生条目也不 panic.
        let blob = IceBlob::new_unchecked("a=candidate:1 1 UDP 100 10.0.0.1 typ host\r\n".into());
        assert!(blob.candidates().is_empty());
    }
}
