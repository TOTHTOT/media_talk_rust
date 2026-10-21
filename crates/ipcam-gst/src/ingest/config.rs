//! Stream-source configuration (`GstStreamConfig`) and its validation.

use std::time::Duration;

use crate::GstStreamError;

/// 单路 RTSP 流配置参数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GstStreamConfig {
    /// RTSP 完整地址，如 `rtsp://192.168.1.10/stream`。
    /// URI 里可以带用户名密码（`rtspsrc` 支持）。
    pub uri: String,
    /// 显式凭据 (用户名, 密码)。如果设了，会覆盖 URI 里的 userinfo。
    pub credentials: Option<(String, String)>,
    /// Jitter buffer 深度（毫秒），越大越能抗网络抖动，但延迟增加。范围 [0, 5000]。
    pub latency_ms: u32,
    /// 音频输出到哪里：
    /// - `Disabled`：不放音，仅给浏览器（默认）
    /// - `Alsa { device }`：同时从本机扬声器播放
    pub audio_output: AudioOutput,
    /// 断流重连策略（指数退避）。
    pub reconnect: ReconnectPolicy,
    /// WebRTC 信令频道上广播的流名称，浏览器据此匹配要拉哪个流。
    /// 对应 webrtcsink 的 `meta,name=...` 属性。
    pub stream_name: String,
    /// WebRTC 信令服务器地址。
    pub signalling_host: String,
    /// WebRTC 信令服务器端口。
    pub signalling_port: u16,
    /// 是否创建 webrtcsink 转推支路。
    /// - `true`：RTSP → WebRTC 转推（网页观看，需要信令服务器）
    /// - `false`：仅本地 tap（GUI 监控页），不需要信令服务器
    /// 注意：`false` 且 `RawTaps.video` 为 `None` 时视频轨无消费者，`link_video` 会报 Link 错误。
    pub webrtc: bool,
    /// GUI tap 分支显示限宽。
    /// - `Some(w)`：解码后 videoscale 降采样到 w 以内（保持宽高比，capsfilter 锁 PAR=1/1）
    /// - `None`：不缩放
    /// 1080p@30fps 全尺寸 RGBA ≈ 165MB/s，ARM 板上必须限宽。
    pub tap_max_width: Option<u32>,
}

impl Default for GstStreamConfig {
    fn default() -> Self {
        Self {
            // Placeholder loopback URI so `Default` passes validation;
            // callers always replace it with a real camera address.
            uri: "rtsp://127.0.0.1:554/stream".into(),
            credentials: None,
            latency_ms: 200,
            audio_output: AudioOutput::default(),
            reconnect: ReconnectPolicy::default(),
            stream_name: "stream".into(),
            signalling_host: "127.0.0.1".into(),
            signalling_port: 8443,
            webrtc: true,
            tap_max_width: None,
        }
    }
}

/// 解码后音频的输出目标。浏览器始终能收到音频（通过 WebRTC 转推）；
/// 此字段仅控制是否同时从本机扬声器播放。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum AudioOutput {
    /// 不从本机扬声器播放，仅给浏览器（默认）。
    #[default]
    Disabled,
    /// 同时从指定 ALSA 设备播放。
    Alsa { device: String },
}

/// 指数退避重连策略。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconnectPolicy {
    /// 首次重连前的等待时间。
    pub initial_delay: Duration,
    /// 每次退避的上限。
    pub max_delay: Duration,
    /// 最大重试次数。`None` = 无限重试。
    pub max_attempts: Option<u32>,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            initial_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(30),
            max_attempts: None,
        }
    }
}

impl ReconnectPolicy {
    /// Exponential back-off: `initial_delay * 2^attempt`, capped at
    /// `max_delay`. `attempt` is zero-based (first retry uses the
    /// initial delay).
    pub fn delay_for(&self, attempt: u32) -> Duration {
        // Clamp the shift so huge attempt counts can't overflow the
        // multiplier before the max_delay cap kicks in.
        let factor = 1u32 << attempt.min(31);
        self.initial_delay
            .saturating_mul(factor)
            .min(self.max_delay)
    }

    /// Whether reconnect attempt number `attempt` (zero-based, the one
    /// about to start) is beyond the configured limit. `None` retries
    /// forever.
    pub fn exhausted(&self, attempt: u32) -> bool {
        self.max_attempts.is_some_and(|max| attempt >= max)
    }
}

/// Static validation of a stream config; performs no I/O.
pub fn validate(cfg: &GstStreamConfig) -> Result<(), GstStreamError> {
    if !cfg.uri.starts_with("rtsp://") {
        return Err(GstStreamError::InvalidConfig(format!(
            "uri must start with `rtsp://`, got {:?}",
            cfg.uri
        )));
    }
    if cfg.latency_ms > 5000 {
        return Err(GstStreamError::InvalidConfig(format!(
            "latency_ms {} out of range [0, 5000]",
            cfg.latency_ms
        )));
    }
    if cfg.reconnect.max_delay < cfg.reconnect.initial_delay {
        return Err(GstStreamError::InvalidConfig(format!(
            "reconnect max_delay ({:?}) < initial_delay ({:?})",
            cfg.reconnect.max_delay, cfg.reconnect.initial_delay
        )));
    }
    if cfg.stream_name.is_empty() {
        return Err(GstStreamError::InvalidConfig(
            "stream_name must not be empty".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_validates() {
        validate(&GstStreamConfig::default()).expect("default config must validate");
    }

    #[test]
    fn rejects_non_rtsp_uri() {
        let cfg = GstStreamConfig {
            uri: "http://192.168.1.64/stream".into(),
            ..Default::default()
        };
        let err = validate(&cfg).unwrap_err();
        assert!(matches!(err, GstStreamError::InvalidConfig(_)));
    }

    #[test]
    fn rejects_latency_above_5000() {
        let cfg = GstStreamConfig {
            latency_ms: 5001,
            ..Default::default()
        };
        let err = validate(&cfg).unwrap_err();
        assert!(matches!(err, GstStreamError::InvalidConfig(_)));
        // boundary value passes
        let cfg = GstStreamConfig {
            latency_ms: 5000,
            ..Default::default()
        };
        validate(&cfg).expect("latency_ms = 5000 is in range");
    }

    #[test]
    fn rejects_max_delay_below_initial() {
        let cfg = GstStreamConfig {
            reconnect: ReconnectPolicy {
                initial_delay: Duration::from_secs(10),
                max_delay: Duration::from_secs(5),
                max_attempts: None,
            },
            ..Default::default()
        };
        let err = validate(&cfg).unwrap_err();
        assert!(matches!(err, GstStreamError::InvalidConfig(_)));
    }

    #[test]
    fn backoff_doubles_and_caps_at_max_delay() {
        let policy = ReconnectPolicy::default();
        let seq: Vec<Duration> = (0..7).map(|a| policy.delay_for(a)).collect();
        assert_eq!(
            seq,
            vec![
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(4),
                Duration::from_secs(8),
                Duration::from_secs(16),
                Duration::from_secs(30),
                Duration::from_secs(30),
            ]
        );
    }

    #[test]
    fn backoff_huge_attempt_does_not_overflow() {
        let policy = ReconnectPolicy::default();
        assert_eq!(policy.delay_for(u32::MAX), Duration::from_secs(30));
    }

    #[test]
    fn exhausted_respects_max_attempts_boundary() {
        let policy = ReconnectPolicy {
            max_attempts: Some(3),
            ..Default::default()
        };
        // attempts 0, 1, 2 may start; attempt 3 is the cut-off
        assert!(!policy.exhausted(0));
        assert!(!policy.exhausted(2));
        assert!(policy.exhausted(3));
        assert!(policy.exhausted(100));
        // None = retry forever
        assert!(!ReconnectPolicy::default().exhausted(u32::MAX));
    }
}
