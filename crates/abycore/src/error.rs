use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Stable failure categories for host decisions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ErrorKind {
    Configuration,
    Authentication,
    Quota,
    RateLimit,
    ContextLimitExceeded,
    InvalidRequest,
    Server,
    Transport,
    Timeout,
    Cancelled,
    Protocol,
    StreamClosed,
    BudgetExceeded,
    Session,
    NeedsResolution,
    EventHandler,
    SearchUnavailable,
}

/// A sanitized SDK error. Request bodies and credentials are never attached.
#[derive(Debug, thiserror::Error)]
#[error("{kind:?}: {message}")]
pub struct Error {
    pub kind: ErrorKind,
    pub message: String,
    pub status: Option<u16>,
    pub code: Option<String>,
    pub request_id: Option<String>,
    pub retry_after: Option<Duration>,
}

impl Error {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            status: None,
            code: None,
            request_id: None,
            retry_after: None,
        }
    }

    pub(crate) fn protocol(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Protocol, message)
    }
}

pub type Result<T> = std::result::Result<T, Error>;
