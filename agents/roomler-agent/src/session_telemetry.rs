//! Per-session counters for `rc:session.stats` (wave 2).
//!
//! `remote_sessions.stats` has been declared-but-never-written since
//! Phase 4 — every recorded session carries zeros. The transport numbers
//! (bytes, RTT) come from the peer connection's own `get_stats()`, but
//! two of the interesting values are produced deep inside the media pump
//! and the input handler, which are free functions keyed by session id
//! rather than methods on [`crate::peer::AgentPeer`].
//!
//! A tiny process-global registry keyed by session id is the least
//! invasive way to collect them: the producers bump a counter (one
//! relaxed atomic add on paths that already do far more work), and the
//! reporter reads them. Entries are dropped with their `AgentPeer`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use bson::oid::ObjectId;
use dashmap::DashMap;

#[derive(Default, Debug)]
pub struct SessionCounters {
    /// Keyframes the viewer asked for (PLI/FIR + our own resync pulses).
    /// A session with a high count spent it recovering from loss.
    pub keyframe_requests: AtomicU32,
    /// Input events accepted for injection (post-suppression).
    pub input_events: AtomicU64,
}

impl SessionCounters {
    pub fn note_keyframe(&self) {
        self.keyframe_requests.fetch_add(1, Ordering::Relaxed);
    }
    pub fn note_input(&self) {
        self.input_events.fetch_add(1, Ordering::Relaxed);
    }
}

fn registry() -> &'static DashMap<ObjectId, Arc<SessionCounters>> {
    static REG: std::sync::OnceLock<DashMap<ObjectId, Arc<SessionCounters>>> =
        std::sync::OnceLock::new();
    REG.get_or_init(DashMap::new)
}

/// Counters for a session, created on first touch. Cheap enough to call
/// from a producer path; the clone is an `Arc` bump.
pub fn counters(session_id: ObjectId) -> Arc<SessionCounters> {
    registry()
        .entry(session_id)
        .or_insert_with(|| Arc::new(SessionCounters::default()))
        .clone()
}

/// Drop a finished session's counters (called from `AgentPeer::drop`) so
/// a long-lived agent doesn't accumulate one entry per session forever.
pub fn forget(session_id: ObjectId) {
    registry().remove(&session_id);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_are_per_session_and_forgettable() {
        let a = ObjectId::new();
        let b = ObjectId::new();
        counters(a).note_keyframe();
        counters(a).note_keyframe();
        counters(a).note_input();
        counters(b).note_input();

        assert_eq!(counters(a).keyframe_requests.load(Ordering::Relaxed), 2);
        assert_eq!(counters(a).input_events.load(Ordering::Relaxed), 1);
        // Sessions never bleed into each other.
        assert_eq!(counters(b).keyframe_requests.load(Ordering::Relaxed), 0);

        forget(a);
        // A forgotten session starts clean rather than resurrecting stale
        // totals if the id is ever touched again.
        assert_eq!(counters(a).keyframe_requests.load(Ordering::Relaxed), 0);
        forget(a);
        forget(b);
    }
}
