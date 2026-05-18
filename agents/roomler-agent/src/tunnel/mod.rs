//! Agent-side `roomler-tunnel` plumbing.
//!
//! The server-side ACL gate in `crates/api/src/ws/tunnel.rs` is the
//! authoritative auth boundary (plan §"Multi-tenancy gotcha" +
//! §"Server-side ACL gate"). This module is the **second** gate the
//! operator can configure on the controlled host:
//!
//! 1. Server gates every `TcpForwardRequest` against `tunnel_policies`
//!    in the agent's tenant. Cross-tenant requests bounce there.
//! 2. Server relays the allowed request to the agent as a
//!    `ServerMsg::TcpForwardForward` over the agent's WS.
//! 3. Agent runs [`acl::AgentForwardAcl::check`] — the operator's
//!    local allowlist. Empty + enabled = "trust the server"; non-
//!    empty = belt-and-suspenders narrower allowlist.
//! 4. On allow, the agent [`dialer::dial_dst`] connects to dst with a
//!    bounded timeout and replies `ClientMsg::TcpForwardAccept`. On
//!    deny or dial failure, the agent replies
//!    `ClientMsg::TcpForwardReject` with a typed `RejectKind`.
//!
//! T2.6 ships the ACL + dialer + acceptor stub. The actual DC pump
//! (data flow between the agent's TCP socket and the WebRTC DC pool)
//! lands in T2.7-9; until then the acceptor replies Accept and
//! immediately closes the TCP socket so the end-to-end integration
//! test can exercise the WS round-trip.

pub mod acceptor;
pub mod acl;
pub mod dialer;
pub mod peer;
