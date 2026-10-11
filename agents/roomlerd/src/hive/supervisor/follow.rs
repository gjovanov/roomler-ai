// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P2d-3b — the member's wants (spec §3b, "The carrier"): for each
//! session this device holds a copy of, it asks the server for a stream
//! (`rc:hive.replica.want`) on one rung of the ladder at a time, and acts on
//! the answer. A dial it follows ([`super::stream`]); a refusal, or a stream
//! that ends, it answers by asking again, climbing down a rung, waiting, or
//! letting the session be.
//!
//! | Outcome | Next |
//! |---|---|
//! | refused `path_unavailable` or `no_answer`, or a stream that never got past its opening exchange (`unreachable`, `no_turn`, `allocate_failed`, `no_derp`, `auth_failed`, …) | the next rung down at once; past the bottom, wait, then the top |
//! | refused `source_offline`, `busy`, `rate_limited`, or a word this build does not know; a stream that ended for its data | wait, then the same rung |
//! | refused `source_unsupported` or `not_held`; a stream ended `diverged_at_floor` | wait long |
//! | a stream that carried and ended (`expired`, the source let go) | at once, from the top |
//! | a stream ended `caught_up`: the copy holds all of a session that is over | nothing more: parked |
//! | refused `not_a_member` or `stale_fence`; the server's close `member_left` or `device_removed` | stopped, until the next join |
//!
//! ⚠️ **Never ratchet.** A rung that carried is not remembered: every new
//! ask starts from `direct` again, so a member that fell to `derp` is back on
//! a direct path at the first ask that finds one.
//!
//! ⚠️ At most [`MAX_FOLLOWING`] sessions are asked for or followed at once,
//! as many as the stream lets a member follow; the rest wait their turn, in
//! order. A device holds copies of every session its owner ran, most of them
//! over, and a session over that the copy holds whole is let be, so the
//! slots go to the sessions that move.
//!
//! ⚠️ Only the primary organization's connection asks: a device holds copies
//! for its primary organization alone (P2c-3), and the connection that
//! starts this is that one's (`on_connected`).

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use bson::oid::ObjectId;
use roomler_ai_remote_control::hive::{HiveStreamPath, HiveStreamRefusal};
use roomler_ai_remote_control::signaling::ClientMsg;
use roomler_hive_node::GENESIS;
use tracing::{debug, info, warn};

use super::Supervisor;
use super::stream::MAX_FOLLOWING;

/// The first wait, after one outcome that carried nothing; each one after
/// doubles it, up to [`WAIT_MAX`].
const WAIT_MIN: Duration = if cfg!(test) {
    Duration::from_millis(50)
} else {
    Duration::from_secs(5)
};
const WAIT_MAX: Duration = if cfg!(test) {
    Duration::from_millis(400)
} else {
    Duration::from_secs(5 * 60)
};
/// The wait after an answer that will not change soon: a source whose build
/// carries no stream, one that does not hold the session, a copy at the
/// current fence the source does not hold.
const WAIT_LONG: Duration = if cfg!(test) {
    Duration::from_millis(800)
} else {
    Duration::from_secs(30 * 60)
};
/// The wait after `rate_limited`: the server counts a minute.
const WAIT_RATE: Duration = if cfg!(test) {
    Duration::from_millis(200)
} else {
    Duration::from_secs(60)
};
/// How long an ask waits for its answer before it is taken for `no_answer`:
/// the server's own wait for the source (30 s), and some.
const WANT_TIMEOUT: Duration = if cfg!(test) {
    Duration::from_secs(5)
} else {
    Duration::from_secs(45)
};

/// What this device is doing about one session it holds a copy of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    /// Asked on this rung; the answer is a dial or a refusal.
    Wanting(HiveStreamPath),
    /// Following this grant's stream.
    Following(ObjectId),
    /// Waiting until the next ask.
    Waiting,
    /// Waiting for a slot: as many are asked for or followed as may be.
    Queued,
    /// The copy holds all of a session that is over.
    Parked,
    /// The server says this device is no member of it, or holds it at
    /// another fence: asked for again only at the next join.
    Stopped,
}

/// One session held as a member.
struct Follow {
    /// The membership's fence, which every ask names.
    fence: u64,
    /// The rung the next ask tries, into [`Supervisor::ladder`].
    rung: usize,
    /// Outcomes in a row that carried nothing: each doubles the wait.
    misses: u32,
    phase: Phase,
    /// Moved on at every change of phase, so a timer or an answer meant for
    /// an earlier one finds this has moved on and does nothing.
    generation: u64,
}

/// Every session followed, and those waiting for a slot, in order.
#[derive(Default)]
pub(crate) struct Follows {
    map: HashMap<ObjectId, Follow>,
    queue: VecDeque<ObjectId>,
}

impl Follows {
    /// Sessions asked for or followed now: what the slots count.
    fn active(&self) -> usize {
        self.map
            .values()
            .filter(|f| matches!(f.phase, Phase::Wanting(_) | Phase::Following(_)))
            .count()
    }
}

/// What to do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Next {
    /// Ask at once, from the top of the ladder.
    Again,
    /// Ask at once, a rung down; past the bottom, wait, then the top.
    Down,
    /// Wait, then ask on the same rung.
    Later,
    /// Wait long, then ask from the top.
    LongLater,
    /// Wait a minute, then ask on the same rung.
    RateLater,
    Park,
    Stop,
}

/// A refusal's next step.
fn after_refusal(reason: Option<HiveStreamRefusal>) -> Next {
    match reason {
        Some(HiveStreamRefusal::PathUnavailable | HiveStreamRefusal::NoAnswer) => Next::Down,
        Some(HiveStreamRefusal::NotAMember | HiveStreamRefusal::StaleFence) => Next::Stop,
        Some(HiveStreamRefusal::SourceUnsupported | HiveStreamRefusal::NotHeld) => Next::LongLater,
        Some(HiveStreamRefusal::RateLimited) => Next::RateLater,
        // `source_offline`, `busy`, and a word this build cannot read.
        _ => Next::Later,
    }
}

/// A member stream's end's next step: the reason it ended in, and whether it
/// got past its opening exchange (the rung carried).
fn after_stream(reason: &str, answered: bool) -> Next {
    match reason {
        "caught_up" => Next::Park,
        // The device's own word, or the server's close.
        "not_a_member" | "secondary_org" | "member_left" | "device_removed" => Next::Stop,
        "diverged_at_floor" => Next::LongLater,
        // The grant ran out, or the source let go (its daemon restarted, it
        // was the last to receive): ask again, from the top.
        "expired" | "closed" | "source_closed" if answered => Next::Again,
        // Never past the opening exchange: this rung does not carry.
        "unreachable" | "no_address" | "no_turn" | "allocate_failed" | "no_derp"
        | "no_derp_key" | "auth_failed" | "no_answer" | "source_gone" | "path_unavailable"
        | "expired" | "closed" | "source_closed"
            if !answered =>
        {
            Next::Down
        }
        // Its data (a frame, a blob, the chain), a busy member, the store.
        _ => Next::Later,
    }
}

/// `wait`, give or take a fifth, so devices that lost the same source do
/// not all ask again at once.
fn jittered(wait: Duration) -> Duration {
    let fifth = wait.as_millis() as u64 / 5;
    if fifth == 0 {
        return wait;
    }
    let off = rand::random::<u64>() % (2 * fifth + 1);
    wait - Duration::from_millis(fifth) + Duration::from_millis(off)
}

/// The wait after `misses` outcomes in a row that carried nothing.
fn backoff(misses: u32) -> Duration {
    let doubled = WAIT_MIN.saturating_mul(1 << misses.saturating_sub(1).min(16));
    jittered(doubled.min(WAIT_MAX))
}

impl Supervisor {
    /// The rungs this device can ask on, top first: `derp` only while it has
    /// a live DERP connection, whose key the ask names.
    fn ladder(&self) -> Vec<HiveStreamPath> {
        let mut rungs = vec![HiveStreamPath::Direct, HiveStreamPath::Relay];
        if self.derp_handle().is_some() {
            rungs.push(HiveStreamPath::Derp);
        }
        rungs
    }

    /// At the primary organization's connection: every session held as a
    /// member is followed. One followed, queued or waiting already is left
    /// as it is; one asked for is asked again (its answer would have come on
    /// the connection that went); one parked or stopped stays so.
    pub(super) async fn follow_all(self: &Arc<Self>) {
        let Ok(store) = &self.store else {
            return;
        };
        let held = match store.memberships().await {
            Ok(m) => m,
            Err(e) => return warn!(%e, "hive: the memberships could not be read to follow them"),
        };
        for m in held {
            let Ok(session) = ObjectId::parse_str(&m.session) else {
                continue;
            };
            let ask = {
                let mut follows = self.follows.lock().unwrap_or_else(|e| e.into_inner());
                match follows.map.get_mut(&session) {
                    None => {
                        follows.map.insert(
                            session,
                            Follow {
                                fence: m.fence,
                                rung: 0,
                                misses: 0,
                                phase: Phase::Waiting,
                                generation: 0,
                            },
                        );
                        true
                    }
                    Some(f) => {
                        f.fence = f.fence.max(m.fence);
                        matches!(f.phase, Phase::Wanting(_))
                    }
                }
            };
            if ask {
                self.ask(session);
            }
        }
    }

    /// A join this device passed as a member, at `fence`: the session is
    /// followed from the top, whatever it was doing, unless it is followed
    /// or asked for already, which the new fence goes to.
    pub(super) fn follow_joined(self: &Arc<Self>, session: ObjectId, fence: u64) {
        let ask = {
            let mut follows = self.follows.lock().unwrap_or_else(|e| e.into_inner());
            let f = follows.map.entry(session).or_insert(Follow {
                fence,
                rung: 0,
                misses: 0,
                phase: Phase::Waiting,
                generation: 0,
            });
            f.fence = f.fence.max(fence);
            match f.phase {
                Phase::Wanting(_) | Phase::Following(_) | Phase::Queued => false,
                Phase::Waiting | Phase::Parked | Phase::Stopped => {
                    f.rung = 0;
                    f.misses = 0;
                    true
                }
            }
        };
        if ask {
            self.ask(session);
        }
    }

    /// Ask for `session`'s stream now, on its rung, or queue it when every
    /// slot is taken. The ask itself goes from a task of its own: it reads
    /// the copy's tip from the store.
    fn ask(self: &Arc<Self>, session: ObjectId) {
        let ladder = self.ladder();
        let asked = {
            let mut follows = self.follows.lock().unwrap_or_else(|e| e.into_inner());
            // Its own slot, if it held one (an ask again), is its to keep.
            let others = follows
                .map
                .iter()
                .filter(|(s, f)| {
                    **s != session && matches!(f.phase, Phase::Wanting(_) | Phase::Following(_))
                })
                .count();
            let full = others >= MAX_FOLLOWING;
            let Some(f) = follows.map.get_mut(&session) else {
                return;
            };
            f.generation += 1;
            if full {
                f.phase = Phase::Queued;
                if !follows.queue.contains(&session) {
                    follows.queue.push_back(session);
                }
                return debug!(session = %session, "hive: every follow slot is taken — queued");
            }
            if f.rung >= ladder.len() {
                f.rung = 0;
            }
            let path = ladder[f.rung];
            f.phase = Phase::Wanting(path);
            (path, f.fence, f.generation)
        };
        let (path, fence, generation) = asked;
        let sup = Arc::clone(self);
        tokio::spawn(async move {
            sup.send_want(session, fence, path).await;
            // No answer at all: taken for one the source never gave.
            tokio::time::sleep(WANT_TIMEOUT).await;
            if sup.still(session, generation) {
                debug!(session = %session, path = path.as_str(), "hive: an ask went unanswered");
                sup.next(session, Next::Down);
            }
        });
    }

    /// `rc:hive.replica.want`, with where the copy ends now, on the primary
    /// organization's connection. Nothing goes while it is down: the next
    /// connection asks again.
    async fn send_want(&self, session: ObjectId, fence: u64, path: HiveStreamPath) {
        let Ok(store) = &self.store else {
            return;
        };
        let (seq, hash) = match store.chain_tip(&session.to_hex()).await {
            Ok(Some(t)) => (t.seq, hex::encode(t.hash)),
            Ok(None) => (0, hex::encode(GENESIS)),
            Err(e) => {
                return warn!(session = %session, %e, "hive: a copy's tip could not be read to ask for it");
            }
        };
        let derp_pubkey = match path {
            HiveStreamPath::Derp => self.derp_handle().map(|h| h.self_pubkey_hex.clone()),
            HiveStreamPath::Direct | HiveStreamPath::Relay => None,
        };
        let tx = self
            .reporter
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let want = ClientMsg::HiveReplicaWant {
            session_id: session,
            fence,
            seq,
            hash,
            path: Some(path),
            derp_pubkey,
        };
        info!(session = %session, fence, seq, path = path.as_str(), "hive: asking for a stream");
        if let Some(tx) = tx {
            let _ = tx.try_send(want);
        }
    }

    /// Whether `session`'s follow is still where `generation` left it.
    fn still(&self, session: ObjectId, generation: u64) -> bool {
        self.follows
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .map
            .get(&session)
            .is_some_and(|f| f.generation == generation)
    }

    /// `rc:hive.replica.refused`, for the ask in flight on `path` at
    /// `fence`; one for any other moves nothing.
    pub(super) fn on_refused(
        self: &Arc<Self>,
        session: ObjectId,
        fence: u64,
        path: Option<HiveStreamPath>,
        reason: Option<HiveStreamRefusal>,
    ) {
        let current = self
            .follows
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .map
            .get(&session)
            .is_some_and(|f| {
                f.fence == fence && path.is_some_and(|p| f.phase == Phase::Wanting(p))
            });
        if !current {
            return debug!(session = %session, "hive: a refusal for no ask in flight — ignored");
        }
        self.next(session, after_refusal(reason));
    }

    /// A dial came for `session`: it is followed, on `grant_id`'s stream.
    pub(super) fn on_dialed(&self, session: ObjectId, grant_id: ObjectId) {
        let mut follows = self.follows.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(f) = follows.map.get_mut(&session) {
            f.generation += 1;
            f.phase = Phase::Following(grant_id);
            follows.queue.retain(|s| *s != session);
        }
    }

    /// A dial this device turned away before following it (`busy`, a rung
    /// this build cannot name): the answer to the ask in flight, if one is.
    pub(super) fn on_dial_dropped(self: &Arc<Self>, session: ObjectId, reason: &str) {
        let asking = self
            .follows
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .map
            .get(&session)
            .is_some_and(|f| matches!(f.phase, Phase::Wanting(_)));
        if asking {
            self.next(session, after_stream(reason, false));
        }
    }

    /// A member stream of `session` ended, in `reason`, past its opening
    /// exchange (`answered`) or not. One that is not the stream followed (an
    /// older one, beside the renewal that replaced it) moves nothing.
    pub(super) fn on_stream_over(
        self: &Arc<Self>,
        session: ObjectId,
        grant_id: ObjectId,
        reason: &str,
        answered: bool,
    ) {
        let followed = self
            .follows
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .map
            .get(&session)
            .is_some_and(|f| f.phase == Phase::Following(grant_id));
        if followed {
            self.next(session, after_stream(reason, answered));
        }
    }

    /// Act on `next` for `session`, and give a slot this frees to the
    /// session that waited longest for one.
    fn next(self: &Arc<Self>, session: ObjectId, next: Next) {
        let ladder_len = self.ladder().len();
        let (ask_now, wait, generation) = {
            let mut follows = self.follows.lock().unwrap_or_else(|e| e.into_inner());
            let Some(f) = follows.map.get_mut(&session) else {
                return;
            };
            f.generation += 1;
            // Out of its stream or its ask either way: the slot is free
            // until the next ask takes it.
            f.phase = Phase::Waiting;
            let (ask_now, wait) = match next {
                Next::Again => {
                    f.rung = 0;
                    f.misses = 0;
                    (true, None)
                }
                Next::Down if f.rung + 1 < ladder_len => {
                    f.rung += 1;
                    (true, None)
                }
                Next::Down => {
                    f.rung = 0;
                    f.misses += 1;
                    (false, Some(backoff(f.misses)))
                }
                Next::Later => {
                    f.misses += 1;
                    (false, Some(backoff(f.misses)))
                }
                Next::LongLater => {
                    f.rung = 0;
                    f.misses += 1;
                    (false, Some(jittered(WAIT_LONG)))
                }
                Next::RateLater => (false, Some(jittered(WAIT_RATE))),
                Next::Park => {
                    f.phase = Phase::Parked;
                    (false, None)
                }
                Next::Stop => {
                    f.phase = Phase::Stopped;
                    (false, None)
                }
            };
            (ask_now, wait, f.generation)
        };
        info!(session = %session, ?next, wait_ms = wait.map(|w| w.as_millis() as u64), "hive: a follow moves on");
        if ask_now {
            self.ask(session);
        } else if let Some(wait) = wait {
            let sup = Arc::clone(self);
            tokio::spawn(async move {
                tokio::time::sleep(wait).await;
                if sup.still(session, generation) {
                    sup.ask(session);
                }
            });
        }
        self.dequeue();
    }

    /// Give each free slot to the session queued longest.
    fn dequeue(self: &Arc<Self>) {
        loop {
            let next = {
                let mut follows = self.follows.lock().unwrap_or_else(|e| e.into_inner());
                if follows.active() >= MAX_FOLLOWING {
                    return;
                }
                let Some(session) = follows.queue.pop_front() else {
                    return;
                };
                match follows.map.get(&session) {
                    Some(f) if f.phase == Phase::Queued => session,
                    _ => continue,
                }
            };
            self.ask(next);
        }
    }

    /// What this device is doing about `session` now.
    #[cfg(all(test, unix))]
    pub(super) fn follow_phase(&self, session: ObjectId) -> Option<Phase> {
        self.follows
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .map
            .get(&session)
            .map(|f| f.phase)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each answer's next step, as the module's table says.
    #[test]
    fn each_answer_has_its_next_step() {
        use HiveStreamRefusal as R;
        for (word, next) in [
            (Some(R::PathUnavailable), Next::Down),
            (Some(R::NoAnswer), Next::Down),
            (Some(R::NotAMember), Next::Stop),
            (Some(R::StaleFence), Next::Stop),
            (Some(R::SourceUnsupported), Next::LongLater),
            (Some(R::NotHeld), Next::LongLater),
            (Some(R::RateLimited), Next::RateLater),
            (Some(R::SourceOffline), Next::Later),
            (Some(R::Busy), Next::Later),
            (Some(R::Other), Next::Later),
            (None, Next::Later),
        ] {
            assert_eq!(after_refusal(word), next, "{word:?}");
        }
        for (reason, answered, next) in [
            ("caught_up", true, Next::Park),
            ("member_left", true, Next::Stop),
            ("device_removed", false, Next::Stop),
            ("not_a_member", false, Next::Stop),
            ("replaced", true, Next::Later),
            ("diverged_at_floor", true, Next::LongLater),
            ("expired", true, Next::Again),
            ("source_closed", true, Next::Again),
            ("expired", false, Next::Down),
            ("unreachable", false, Next::Down),
            ("allocate_failed", false, Next::Down),
            ("no_derp", false, Next::Down),
            ("auth_failed", false, Next::Down),
            ("bad_frame", true, Next::Later),
            ("source:blob_missing", true, Next::Later),
            ("busy", false, Next::Later),
            ("refused", false, Next::Later),
        ] {
            assert_eq!(after_stream(reason, answered), next, "{reason} {answered}");
        }
    }

    /// A wait doubles with each miss, up to its ceiling, give or take a
    /// fifth.
    #[test]
    fn a_wait_doubles_to_its_ceiling() {
        for (misses, base) in [
            (1, WAIT_MIN),
            (2, WAIT_MIN * 2),
            (3, WAIT_MIN * 4),
            (30, WAIT_MAX),
        ] {
            let base = base.min(WAIT_MAX);
            for _ in 0..50 {
                let w = backoff(misses);
                assert!(
                    w >= base - base / 5 && w <= base + base / 5,
                    "{misses}: {w:?} around {base:?}"
                );
            }
        }
    }
}
