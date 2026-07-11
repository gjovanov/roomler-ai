//! Tunnel-client **session driver** — the reusable engine behind the CLI's
//! `forward`/`socks5`/`mesh` and (P3b-2) the daemon's outbound tunnels.
//!
//! P3b-1 lands the shared **flow vocabulary** here — the per-flow
//! reply-correlation types + the open-timeout — so the UDP relay (`crate::udp`)
//! can live in `tunnel-core` alongside the transports it drives, and so both
//! the session orchestration (still in `roomler-tunnel::forward` today; folds
//! into this module in the next slice) and the UDP relay share ONE definition.
//! The session orchestration + the `TunnelSignaling` seam arrive in P3b-2.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use roomler_ai_remote_control::signaling::RejectKind;
use tokio::sync::{Mutex, oneshot};

/// Per-flow open round-trip cap: `TcpForwardRequest` / `UdpForwardRequest` →
/// `Accept` / `Reject`. Server-side ACL eval is local, but the request rides the
/// agent's dial timeout in the relay case. Shared by the TCP session driver and
/// the UDP relay (`crate::udp`).
pub const FLOW_OPEN_TIMEOUT: Duration = Duration::from_secs(10);

/// Reply registry: per-flow oneshot for the server's accept/reject. Shared
/// across the TCP session driver and the UDP relay so flow-ids stay a single
/// correlation space across TCP + UDP within a session.
pub type ReplyRegistry = Arc<Mutex<HashMap<u32, oneshot::Sender<ForwardReply>>>>;

/// Active-flow registry: which DC index a given flow is bound to, so the WS
/// dispatch can route inbound `TcpHalfClose` audit signals (no demux action —
/// the in-band marker handles the data-plane close).
pub type ActiveFlows = Arc<Mutex<HashMap<u32, u8>>>;

/// The server's per-flow decision, delivered to the waiting opener via the
/// [`ReplyRegistry`] oneshot.
#[derive(Debug)]
pub enum ForwardReply {
    Accept { dc_index: u8 },
    Reject { kind: RejectKind, reason: String },
}
