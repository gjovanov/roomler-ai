//! C-1 — multi-pod cluster foundation (see `docs/multi-pod-scale-out.md`).
//!
//! Three pieces, all fail-soft when Redis is absent:
//! - [`identity`]: stable `pod_id` + per-process `epoch` fencing token.
//! - [`directory`]: entity → owning-pod records (LWW for connection-bound
//!   entities, NX for server-materialized ones), three shared Lua ops.
//! - [`bus`]: per-pod channels + request/reply with deadlines; the RPC
//!   deadline is the ACTIVE failure detector, directory TTLs the passive
//!   backstop, `roomler:pod-alive:*` advisory only.

pub mod bus;
pub mod directory;
pub mod identity;
pub mod metrics;
