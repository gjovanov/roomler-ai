//! PR-1 rehome: direction-aware cross-pod convergence for rc + tunnel.
//!
//! A cross-pod miss (controller/tunnel-client on pod A, agent's presence
//! record on pod B) has two very different cures and they are NOT
//! symmetric in cost:
//!
//! - **Move the controller** (it re-dials with the right `tid`): free.
//! - **Move the agent** (cycle its WS so the LB re-hashes it): tears the
//!   agent's rc sessions, tunnel sessions in BOTH directions, and its
//!   whole overlay runtime (WG/DERP carriers) for seconds; on corp-VPN
//!   hosts a carrier rebuild under VPN-on can relay-lock the mesh until
//!   a VPN-off window.
//!
//! The 2026-08-04 incident was a mis-keyed CONTROLLER (its WS dialed
//! key-less on a deep link and hashed by client IP) while the agent sat
//! on its hash-correct pod carrying live tunnel flows; the pre-PR-1
//! server nudged the agent 11 times in 15 s (all refused, correctly)
//! and never told the controller anything it could act on.
//!
//! [`rehome_direction`] decides who moves, using only data already at
//! hand; no knowledge of the LB's ring is needed:
//! 1. dial key absent/wrong: the controller is mis-keyed, it moves.
//! 2. key correct + conn provably newer than the agent's record (guard
//!    band, `rc.rehome_direction_guard_ms`): the conn reflects the LB's
//!    CURRENT verdict, so the agent is parked; nudge it (idle-only,
//!    paced by [`nudge_gate`] / [`nudge_book`]).
//! 3. anything else (older conn / inside the band): ambiguous, the
//!    controller moves. Ambiguity self-resolves toward case 2: the
//!    record's `since_ms` is frozen at registration while every retry
//!    re-establishes the conn, so the delta grows monotonically.

use std::sync::atomic::Ordering;
use std::time::Instant;

use bson::oid::ObjectId;
use dashmap::DashMap;
use tracing::warn;

/// Who moves to converge a cross-pod miss.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RehomeDirection {
    /// Reply `agent_on_other_pod` only; the client re-dials. NEVER
    /// nudge the agent for these: in the observed incident class the
    /// agent is exactly where the LB wants it.
    ControllerMove {
        /// Log label: why the controller was judged the moving party.
        reason: &'static str,
    },
    /// Correct key + provably newer conn: the agent is parked off its
    /// ring pod; nudge it (idle-only, cooldown-gated).
    NudgeAgent,
}

/// Decide the convergence direction for one cross-pod miss. Pure; unit
/// tested below. `agent_tenant_hex` empty (lookup failed) is treated as
/// "cannot prove the key correct", so the controller moves: a wrong
/// suppression costs one extra client redial; a wrong nudge costs a
/// busy agent's planes.
pub fn rehome_direction(
    dialed_tid: Option<&str>,
    agent_tenant_hex: &str,
    conn_established_ms: i64,
    agent_since_ms: i64,
    guard_band_ms: i64,
) -> RehomeDirection {
    let Some(tid) = dialed_tid else {
        return RehomeDirection::ControllerMove {
            reason: "keyless_dial",
        };
    };
    if agent_tenant_hex.is_empty() || tid != agent_tenant_hex {
        return RehomeDirection::ControllerMove {
            reason: "wrong_key",
        };
    }
    if conn_established_ms.saturating_sub(agent_since_ms) > guard_band_ms {
        RehomeDirection::NudgeAgent
    } else {
        RehomeDirection::ControllerMove {
            reason: "ambiguous_age",
        }
    }
}

/// Owner-side per-agent nudge pacing state (mirrors the C-5 DERP
/// cooldown trio; settings-driven so the two-pod tests can shrink it).
#[derive(Debug, Default)]
pub struct NudgeCooldown {
    pub last: Option<Instant>,
    pub attempts: u32,
}

/// agent_id -> pacing state, hung off `AppState`.
pub type NudgeCooldowns = DashMap<ObjectId, NudgeCooldown>;

/// Pacing knobs, resolved once from settings at the call site.
#[derive(Debug, Clone, Copy)]
pub struct NudgePacing {
    pub cooldown: std::time::Duration,
    pub max_attempts: u32,
    pub attempts_reset_after: std::time::Duration,
}

/// Gate verdict for one prospective agent-WS cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NudgeGate {
    /// Fire away; [`nudge_book`] the cycle if it actually happens.
    Allow,
    /// A cycle fired too recently for this agent.
    Cooldown,
    /// The attempts cap tripped inside the reset window: repeated FIRED
    /// cycles are not converging (split evidence, counted + warned).
    Stuck,
}

/// The ping-pong guard for agent-WS cycles. This is a PEEK: nothing is
/// booked. Attempts count FIRED cycles only ([`nudge_book`] runs after
/// the hub actually fires), so busy refusals can never trip the stuck
/// signal.
pub fn nudge_gate(
    cooldowns: &NudgeCooldowns,
    agent_id: ObjectId,
    pacing: NudgePacing,
) -> NudgeGate {
    nudge_gate_at(cooldowns, agent_id, pacing, Instant::now())
}

/// Time-injected core (tests advance a synthetic clock FORWARD;
/// subtracting from a fresh runner's `Instant::now()` can underflow the
/// monotonic epoch and panic).
pub fn nudge_gate_at(
    cooldowns: &NudgeCooldowns,
    agent_id: ObjectId,
    pacing: NudgePacing,
    now: Instant,
) -> NudgeGate {
    let entry = cooldowns.entry(agent_id).or_default();
    let Some(last) = entry.last else {
        return NudgeGate::Allow;
    };
    let since = now.duration_since(last);
    if since >= pacing.attempts_reset_after {
        return NudgeGate::Allow;
    }
    if since < pacing.cooldown {
        return NudgeGate::Cooldown;
    }
    if entry.attempts >= pacing.max_attempts {
        let total =
            crate::cluster::metrics::AGENT_NUDGE_STUCK_TOTAL.fetch_add(1, Ordering::Relaxed) + 1;
        warn!(
            agent = %agent_id,
            agent_nudge_stuck_total = total,
            "SPLIT EVIDENCE: agent nudge attempts capped; cross-pod rc miss not converging"
        );
        return NudgeGate::Stuck;
    }
    NudgeGate::Allow
}

/// Book one FIRED cycle (call only after the hub reported `Nudged`).
pub fn nudge_book(cooldowns: &NudgeCooldowns, agent_id: ObjectId, pacing: NudgePacing) {
    nudge_book_at(cooldowns, agent_id, pacing, Instant::now())
}

/// Time-injected core of [`nudge_book`].
pub fn nudge_book_at(
    cooldowns: &NudgeCooldowns,
    agent_id: ObjectId,
    pacing: NudgePacing,
    now: Instant,
) {
    let mut entry = cooldowns.entry(agent_id).or_default();
    let reset = entry
        .last
        .is_none_or(|last| now.duration_since(last) >= pacing.attempts_reset_after);
    entry.attempts = if reset { 1 } else { entry.attempts + 1 };
    entry.last = Some(now);
}

/// Requester-side throttle: agent_id -> last `rc.agent_nudge` RPC sent
/// from THIS pod. One refusing owner received 11 RPCs in 15 s from a
/// single click storm pre-PR-1.
pub type NudgeRequestThrottle = DashMap<ObjectId, Instant>;

/// `true` = send the RPC (slot booked).
pub fn nudge_request_allowed(
    throttle: &NudgeRequestThrottle,
    agent_id: ObjectId,
    min_interval: std::time::Duration,
) -> bool {
    nudge_request_allowed_at(throttle, agent_id, min_interval, Instant::now())
}

/// Time-injected core, same monotonic-epoch caveat as above.
pub fn nudge_request_allowed_at(
    throttle: &NudgeRequestThrottle,
    agent_id: ObjectId,
    min_interval: std::time::Duration,
    now: Instant,
) -> bool {
    match throttle.entry(agent_id) {
        dashmap::mapref::entry::Entry::Vacant(v) => {
            v.insert(now);
            true
        }
        dashmap::mapref::entry::Entry::Occupied(mut o) => {
            if now.duration_since(*o.get()) < min_interval {
                false
            } else {
                o.insert(now);
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const GUARD: i64 = 5000;

    #[test]
    fn keyless_or_wrong_key_moves_the_controller_and_never_nudges() {
        let t = "69a1dbbad2000f26adc875ce";
        assert_eq!(
            rehome_direction(None, t, 2_000_000, 1_000, GUARD),
            RehomeDirection::ControllerMove {
                reason: "keyless_dial"
            }
        );
        assert_eq!(
            rehome_direction(Some("68eeeeeeeeeeeeeeeeeeeeee"), t, 2_000_000, 1_000, GUARD),
            RehomeDirection::ControllerMove {
                reason: "wrong_key"
            }
        );
        // Agent-tenant lookup failed: cannot prove the key correct.
        assert_eq!(
            rehome_direction(Some(t), "", 2_000_000, 1_000, GUARD),
            RehomeDirection::ControllerMove {
                reason: "wrong_key"
            }
        );
    }

    #[test]
    fn correct_key_and_provably_newer_conn_nudges_the_agent() {
        let t = "69a1dbbad2000f26adc875ce";
        assert_eq!(
            rehome_direction(Some(t), t, 100_000, 10_000, GUARD),
            RehomeDirection::NudgeAgent
        );
    }

    #[test]
    fn ambiguous_age_moves_the_controller() {
        let t = "69a1dbbad2000f26adc875ce";
        // Inside the guard band either way: an LB map flip between two
        // dials makes neither party provably wrong.
        assert_eq!(
            rehome_direction(Some(t), t, 12_000, 10_000, GUARD),
            RehomeDirection::ControllerMove {
                reason: "ambiguous_age"
            }
        );
        // Controller OLDER than the record: a parked pre-flip tab.
        assert_eq!(
            rehome_direction(Some(t), t, 5_000, 10_000, GUARD),
            RehomeDirection::ControllerMove {
                reason: "ambiguous_age"
            }
        );
    }

    fn pacing() -> NudgePacing {
        NudgePacing {
            cooldown: Duration::from_secs(60),
            max_attempts: 3,
            attempts_reset_after: Duration::from_secs(600),
        }
    }

    #[test]
    fn gate_paces_books_caps_and_resets() {
        let cd = NudgeCooldowns::default();
        let agent = ObjectId::new();
        let t0 = Instant::now();

        assert_eq!(nudge_gate_at(&cd, agent, pacing(), t0), NudgeGate::Allow);
        nudge_book_at(&cd, agent, pacing(), t0);
        // Inside the cooldown: refused, and NOT booked.
        assert_eq!(
            nudge_gate_at(&cd, agent, pacing(), t0 + Duration::from_secs(10)),
            NudgeGate::Cooldown
        );
        // Cycles 2 and 3 spaced past the cooldown: allowed + booked.
        let t2 = t0 + Duration::from_secs(61);
        assert_eq!(nudge_gate_at(&cd, agent, pacing(), t2), NudgeGate::Allow);
        nudge_book_at(&cd, agent, pacing(), t2);
        let t3 = t2 + Duration::from_secs(61);
        assert_eq!(nudge_gate_at(&cd, agent, pacing(), t3), NudgeGate::Allow);
        nudge_book_at(&cd, agent, pacing(), t3);
        // A 4th cycle inside the reset window: capped (split evidence).
        assert_eq!(
            nudge_gate_at(&cd, agent, pacing(), t3 + Duration::from_secs(61)),
            NudgeGate::Stuck
        );
        // Quiet period passes: allowed again, booking restarts the count.
        let t5 = t3 + Duration::from_secs(601);
        assert_eq!(nudge_gate_at(&cd, agent, pacing(), t5), NudgeGate::Allow);
        nudge_book_at(&cd, agent, pacing(), t5);
        assert_eq!(cd.get(&agent).unwrap().attempts, 1);
    }

    #[test]
    fn busy_refusals_do_not_consume_attempts() {
        let cd = NudgeCooldowns::default();
        let agent = ObjectId::new();
        let t0 = Instant::now();
        // Many peeks, zero books: still Allow, attempts untouched.
        for i in 0..10 {
            assert_eq!(
                nudge_gate_at(&cd, agent, pacing(), t0 + Duration::from_secs(i)),
                NudgeGate::Allow
            );
        }
        assert_eq!(cd.get(&agent).unwrap().attempts, 0);
    }

    #[test]
    fn requester_throttle_books_one_slot_per_interval() {
        let th = NudgeRequestThrottle::default();
        let agent = ObjectId::new();
        let t0 = Instant::now();
        assert!(nudge_request_allowed_at(
            &th,
            agent,
            Duration::from_secs(5),
            t0
        ));
        assert!(!nudge_request_allowed_at(
            &th,
            agent,
            Duration::from_secs(5),
            t0 + Duration::from_secs(1)
        ));
        assert!(nudge_request_allowed_at(
            &th,
            agent,
            Duration::from_secs(5),
            t0 + Duration::from_secs(6)
        ));
    }
}
