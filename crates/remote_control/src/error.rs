// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
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

    /// FR-115 P3 — the organization already runs as many remote sessions at
    /// once as its plan allows (Free: one, Pro: one per seat). Refused at
    /// the START of a session, never by ending a live one. The hub has no
    /// names, so it hands over the ids of one session holding a slot (hex);
    /// the API layer turns them into "who, on which device" so the person
    /// asking can end that session or upgrade.
    #[error("your organization's plan runs {cap} remote session(s) at a time, and one is open now")]
    OrgSessionLimit {
        cap: u32,
        holder_user: String,
        holder_agent: String,
    },

    #[error("ws send failed")]
    SendFailed,

    /// Fleet RPC — the device runs an agent that predates `rc:rpc.exec`.
    /// Surfaced as `412 Precondition Failed`, never as a hang: a pre-feature
    /// agent drops the unknown tag in its debug branch, so pushing anyway
    /// would leave the caller waiting out its whole deadline for silence.
    #[error("device {0} runs an agent without Fleet-RPC support")]
    ExecUnsupported(String),

    /// Fleet RPC — the device accepted the request but produced no result
    /// before the caller's deadline.
    #[error("no result from device {0} within the deadline")]
    ExecTimeout(String),

    // Server-only (P3e Phase 3): the driver error can only arise from the
    // `server`-gated audit/hub paths, and carrying the variant unconditionally
    // is what dragged mongodb into every agent-side binary.
    #[cfg(feature = "server")]
    #[error(transparent)]
    Mongo(#[from] mongodb::error::Error),

    #[error(transparent)]
    Bson(#[from] bson::ser::Error),

    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
