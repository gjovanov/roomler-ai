//! Overlay evidence counters — process-global, always compiled (increment
//! sites may live behind features/cfgs; a build without them reports honest
//! zeros). Cumulative since daemon start — consumers DIFF two readings,
//! never judge absolutes (the summary-counter rule).
//!
//! (The multi-org v2 retirement counters — mux-NAT rewrites/restores +
//! SkipAsSource flips — served their purpose: fleet-wide zeros over the
//! ≥7-day soak licensed the W7c deletion of the whole compensation layer,
//! and the counters left with it.)

use std::sync::atomic::AtomicU64;

/// PR-B1 tripwire — direct-socket binds that could NOT take the stable base
/// port and walked the band. On a host with a configured stable port this is
/// either an external squatter (Hyper-V/WSL reservation) or — the 2026-08-10
/// wedge — a second in-process binder colliding with the first's leaked
/// sockets. Nonzero on a quiet host is a bug signal, not noise.
pub static DIRECT_BIND_WALKS: AtomicU64 = AtomicU64::new(0);

/// A3 — peer endpoints ADOPTED via WG-style roaming: an authenticated inbound
/// from a source other than the peer's registered endpoint repointed the
/// carrier. Expected to tick a few times as a symmetric-NAT peer's real
/// mapping is learned, then settle; a steadily climbing count means endpoint
/// thrash (a roam war or a spoof-probe storm) — worth a look, not silent.
pub static ROAM_ADOPTIONS: AtomicU64 = AtomicU64::new(0);

/// C1 (disco) — out-of-tunnel carrier echoes this node ANSWERED. The
/// responder-only stage has no other observable: a node that answers is a
/// node the future prober can measure, so this counter IS the C1 field gate
/// (every node nonzero before any prober ships).
pub static DISCO_ANSWERED: AtomicU64 = AtomicU64::new(0);

/// C2 (disco) — verified PONGs dropped because the owning engine's sink was
/// full. Every drop reads as LOSS in that engine's per-path table upstream
/// (the exact silent seam the 2026-08-12 half-protocol incident hid in), so
/// a nonzero here re-frames a bad loss number as backpressure, not path
/// quality.
pub static DISCO_PONG_DROPS: AtomicU64 = AtomicU64::new(0);

/// Authenticated/limiter-passed handshake initiations dropped because the
/// engine's accept channel (`direct_events`, depth 16) was full. Each drop
/// costs the initiating peer a ~5 s retransmit; a burst here during a
/// churn storm explains "slow re-establish" without blaming the network.
pub static DIRECT_INBOUND_DROPS: AtomicU64 = AtomicU64::new(0);
