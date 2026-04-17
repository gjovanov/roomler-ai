//! `roomler-ai-remote-control` — TeamViewer-style remote desktop subsystem for Roomler AI.
//!
//! This crate is signaling and state-management only. It never touches video
//! frames or input events; those flow over a direct WebRTC PeerConnection
//! between the controller browser and the native agent.
//!
//! Module map:
//!
//! - [`hub`]         — process-global registry of online agents and live sessions
//! - [`session`]     — session state machine (Pending → AwaitingConsent → Active → Closed)
//! - [`signaling`]   — `rc:*` WebSocket message types and dispatch
//! - [`consent`]     — controller-requested-control consent flow
//! - [`permissions`] — per-session capability bitfield (input, clipboard, files, audio)
//! - [`turn_creds`]  — short-lived TURN credentials (HMAC over coturn shared secret)
//! - [`audit`]       — write-side of the remote_audit collection
//! - [`models`]      — Mongo-backed entities (Agent, RemoteSession, RemoteAuditEvent)
//! - [`error`]       — unified error type
//!
//! This crate intentionally does NOT depend on the mediasoup crate. The SFU
//! bridge for N-watcher sessions lives in a sibling crate (`sfu`) and is
//! invoked via a trait object passed into [`hub::Hub::new`].

pub mod audit;
pub mod consent;
pub mod error;
pub mod hub;
pub mod models;
pub mod permissions;
pub mod serde_helpers;
pub mod session;
pub mod signaling;
pub mod turn_creds;

pub use error::{Error, Result};
pub use hub::Hub;
pub use models::{Agent, AgentStatus, RemoteSession, SessionPhase};
pub use permissions::Permissions;
pub use signaling::{ClientMsg, ServerMsg};
