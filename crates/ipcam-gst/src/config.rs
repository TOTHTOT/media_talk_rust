//! Stream-source configuration (`GstStreamConfig`) and its validation.

use std::time::Duration;

use crate::GstStreamError;

/// Configuration for one RTSP streaming session (the `StreamSource`
/// entity from the data model).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GstStreamConfig {
    /// Full RTSP URI; userinfo is allowed (`rtspsrc` supports it).
    pub uri: String,
    /// Explicit credentials; when set they override any URI userinfo.
    pub credentials: Option<(String, String)>,
    /// Jitter-buffer depth in milliseconds.
    pub latency_ms: u32,
    /// Optional decoded playback branch tee'd off the pipeline.
    pub audio_output: AudioOutput,
    /// Reconnect/back-off policy after a stream failure.
    pub reconnect: ReconnectPolicy,
    /// Producer name announced on the WebRTC signalling channel
    /// (`meta,name=...`); browser consumers match on this exact string.
    pub stream_name: String,
    /// Signalling server the stream's webrtcsink registers with
    /// (started once per process by [`crate::ensure_signalling_server`]).
    pub signalling_host: String,
    /// Signalling server port.
    pub signalling_port: u16,
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
        }
    }
}

/// Where decoded audio goes, besides the encoded-frame callback.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum AudioOutput {
    /// Only deliver encoded audio frames via the callback.
    #[default]
    Disabled,
    /// Tee off a decode-and-play branch to an ALSA device.
    Alsa { device: String },
}

/// Exponential back-off reconnect policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconnectPolicy {
    /// Delay before the first reconnect attempt.
    pub initial_delay: Duration,
    /// Upper bound for the per-attempt delay.
    pub max_delay: Duration,
    /// `None` = retry forever.
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
