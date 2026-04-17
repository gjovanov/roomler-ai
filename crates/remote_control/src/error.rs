use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("agent {0} not online")]
    AgentOffline(String),

    #[error("agent {0} not found")]
    AgentNotFound(String),

    #[error("session {0} not found")]
    SessionNotFound(String),

    #[error("session {0} in wrong phase: {1}")]
    BadPhase(String, &'static str),

    #[error("consent denied by user")]
    ConsentDenied,

    #[error("consent timed out")]
    ConsentTimeout,

    #[error("permission denied: {0}")]
    PermissionDenied(&'static str),

    #[error("invalid signaling message: {0}")]
    BadMessage(&'static str),

    #[error("agent capacity exceeded")]
    AgentBusy,

    #[error("ws send failed")]
    SendFailed,

    #[error(transparent)]
    Mongo(#[from] mongodb::error::Error),

    #[error(transparent)]
    Bson(#[from] bson::ser::Error),

    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
