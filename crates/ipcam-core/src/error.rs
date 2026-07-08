use thiserror::Error;

pub type CoreResult<T> = Result<T, CoreError>;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("invalid uri: {0}")]
    InvalidUri(String),

    #[error("auth failed: {0}")]
    AuthFailed(String),

    #[error("codec unsupported: {0}")]
    CodecUnsupported(String),

    #[error("timeout after {0:?}")]
    Timeout(Option<std::time::Duration>),

    #[error("not implemented: {0}")]
    NotImplemented(&'static str),

    #[error("transport closed")]
    TransportClosed,

    #[error("decode error: {0}")]
    Decode(String),

    #[error("encode/mux error: {0}")]
    Mux(String),

    #[error("other: {0}")]
    Other(String),
}

impl CoreError {
    pub fn other<S: Into<String>>(s: S) -> Self {
        Self::Other(s.into())
    }
}
