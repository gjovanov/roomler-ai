//! Multi-org v2 — retirement-evidence counters.
//!
//! The multi-org compensation layers (the mux NAT, the Windows SkipAsSource
//! reconcile) are scheduled for deletion once the per-org-network-spaces
//! architecture makes them structurally unnecessary — but the deletion gate
//! is EVIDENCE, not calendar: fleet-wide zeros here over a soak window.
//! These counters exist so "the layer is idle" is a number in
//! `roomler status` instead of a grep over rotated logs.
//!
//! Process-global statics, always compiled (the increment sites live behind
//! the `overlay`/`overlay-l3` features and Windows cfgs; a build without
//! them reports honest zeros). Cumulative since daemon start — consumers
//! DIFF two readings, never judge absolutes (the summary-counter rule).

use std::sync::atomic::{AtomicU64, Ordering};

/// Cross-org egress sources the mux NAT rewrote (tun_mux Hook A).
pub static MUX_NAT_REWRITES: AtomicU64 = AtomicU64::new(0);
/// Inbound reply destinations the mux NAT restored (tun_mux Hook B).
pub static MUX_NAT_RESTORES: AtomicU64 = AtomicU64::new(0);
/// Windows `SkipAsSource` flags the reconcile actually FLIPPED (a no-op
/// reconcile pass counts nothing).
pub static SKIP_AS_SOURCE_FLIPS: AtomicU64 = AtomicU64::new(0);

/// PR-B1 tripwire — direct-socket binds that could NOT take the stable base
/// port and walked the band. On a host with a configured stable port this is
/// either an external squatter (Hyper-V/WSL reservation) or — the 2026-08-10
/// wedge — a second in-process binder colliding with the first's leaked
/// sockets. Nonzero on a quiet host is a bug signal, not noise.
pub static DIRECT_BIND_WALKS: AtomicU64 = AtomicU64::new(0);

/// One relaxed snapshot of all three, in declaration order.
pub fn snapshot() -> (u64, u64, u64) {
    (
        MUX_NAT_REWRITES.load(Ordering::Relaxed),
        MUX_NAT_RESTORES.load(Ordering::Relaxed),
        SKIP_AS_SOURCE_FLIPS.load(Ordering::Relaxed),
    )
}
