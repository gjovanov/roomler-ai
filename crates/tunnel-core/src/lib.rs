//! Roomler AI tunnel core.
//!
//! Shared building blocks for the `roomler-tunnel` CLI (controller side)
//! and the `roomler-agent` tunnel service (target side):
//!
//! * [`transport`] — pluggable data-plane (`webrtc-dc-v1` today;
//!   `wireguard-v1` in v0.5). Capability-negotiated at peer setup.
//! * [`auth`]      — `TunnelClient` / `TunnelEnrollment` JWT claim
//!   shapes. The actual issue/verify functions live in
//!   `crates/services/src/auth/mod.rs` next to the existing Agent /
//!   Enrollment / Access / Refresh pattern.
//! * [`policy`]    — ACL evaluation (`subject × target × destination`).
//!   Pure functions, table-driven tests.
//! * [`mux`]       — `flow_id` framing (4-byte LE prefix per DC message)
//!   so multiple TCP flows share a fixed DC pool without per-flow RTT
//!   setup. See plan §4.
//! * [`forward`]   — bidirectional TCP↔DC pump with event-driven
//!   backpressure (`bufferedAmountLowThreshold`), lifted from the
//!   rc.19 file-DC pattern.
//! * [`signaling`] — re-exports the `rc:tunnel.*` envelope variants
//!   that land in `remote_control::signaling` in T2 (today: stub).
//!
//! Multi-tenancy invariant: every code path that touches a tenant id
//! must compare `tunnel_client.tenant_id == agent.tenant_id` BEFORE
//! evaluating any policy (defence-in-depth — see plan §7). The
//! cross-tenant integration test in `crates/tests/src/tunnel_tests.rs`
//! locks this; do not relax it.

pub mod auth;
/// Tunnel-client session driver — the shared flow vocabulary (P3b-1); the session
/// orchestration + the `TunnelSignaling` seam fold in at P3b-2.
pub mod driver;
/// Node env-var reads with `ROOMLER_NODE_*` → legacy `ROOMLER_AGENT_*` fallback.
pub mod env;
pub mod forward;
/// LocalAPI — the daemon's local control surface (P1: read-only protocol).
pub mod localapi;
pub mod mux;
/// Overlay L3 data plane (userspace WireGuard mesh) — feature `overlay`.
#[cfg(feature = "overlay")]
pub mod overlay;
pub mod policy;
pub mod signaling;
/// SOCKS5 server + client wire codec — the userspace-mode proxy + mesh chaining.
pub mod socks5;
pub mod transport;
/// SOCKS5 UDP ASSOCIATE relay for the tunnel client's userspace mode.
pub mod udp;
