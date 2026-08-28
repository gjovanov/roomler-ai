//! FR-19 — org-relay: framing, and the reachability responder.
//!
//! * [`wire`] — Geneve framing and the shape rules (P1a, #816). Re-exported at
//!   this level so call sites read `orgrelay::is_org_relay_shaped(..)`.
//! * [`responder`] — the P1 **bind-only** reachability responder: it answers
//!   probes and forwards nothing. There is no session table and no data path
//!   here; those arrive with P2.

pub mod responder;
mod wire;

pub use wire::*;
