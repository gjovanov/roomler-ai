// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-138 P1b-2 — `rc:session.rejoin`, the viewer's claim on a session held
//! for it (`docs/fr/FR-138-the-session-outlives-its-control-socket.md` §4.4,
//! §4.5), answered on the pod the viewer's new socket reached.
//!
//! Each rejoin gets exactly one answer, and it names the session:
//! `rc:session.resumed`, `rc:session.detached {resume_by}` (ask again, in a
//! second, until `resume_by`), or `rc:terminate`. Never an `rc:error`: the
//! page asks again only after a `detached`, so a rejoin left unanswered
//! would hold it until its own deadline.
//!
//! | Where the session is | Answer |
//! |---|---|
//! | held by this pod's hub, for this person | the gate a request runs, again; then bound here and `rc:session.resumed`, or ended `admin_terminated` |
//! | the row names someone else, or nothing | `rc:terminate agent_disconnect`, the same as for no session: nothing leaks |
//! | the row is ended | `rc:terminate` with the row's own reason, except `controller_hangup`, answered `agent_disconnect` (`rejoin_end_reason`) |
//! | the row is live, and the gate refuses | written to the row as `admin_terminated`, and published, as an admin's end; `rc:terminate admin_terminated` |
//! | the gate cannot be run (a read failed) | nothing bound, nothing written: `rc:session.detached` with when the session ends if nobody claims it, so the viewer asks again |
//! | the row cannot be read | `rc:session.detached` with the window from now: the viewer asks again, and the first answer that reads the row brings its deadline in |
//! | detached by the device's side, inside `resume_by` | `rc:session.detached` with the row's `resume_by`: ask again |
//! | detached, past `resume_by` | `rc:terminate agent_disconnect` |
//! | live in the hub of the pod that holds the device's socket | relayed there (`rc.cmd`), bound there, answered through the relay |
//! | live, but in no hub yet (the claim is not answered, or a pod died without a word) | `rc:session.detached` with when it would end, or `rc:terminate agent_disconnect` once that has passed |
//!
//! Review 2, N14 — the gate costs about five reads, and a page asks every
//! second while its session waits for the device's claim. Where nothing is
//! bound or relayed (a detached row, a live row in no hub yet) the gate runs
//! at most once per 5 s per session and viewer ([`gate_due`]), and a refusal
//! answers as the table says: a viewer removed from the org is refused
//! within about 5 s. Before a bind (held here) or a relay it always runs.
//!
//! With the window at 0 the rejoin is dropped unanswered, as a server before
//! FR-138 drops it. No page sends one then: none was told `resume_secs`.

use std::time::{Duration, Instant};

use bson::{DateTime, oid::ObjectId};
use dashmap::DashMap;
use roomler_ai_mod_fleet::hub::OrgPolicy;
use roomler_ai_remote_control::models::{
    ConsentMode, EndReason, RcOverlayMode, RemoteSession, SessionPhase,
};
use roomler_ai_remote_control::session::ClientTx;
use roomler_ai_remote_control::signaling::{ClientMsg, ServerMsg};
use roomler_ai_services::dao::base::DaoError;
use tracing::{debug, info, warn};

use crate::RemoteState;
use crate::controller::{ResumeGate, resume_gate};

/// Review 2, N14 — when each (session, viewer) last ran the gate on a rejoin
/// that binds nothing (`RemoteState::rejoin_gates`).
pub type GateRuns = DashMap<(ObjectId, ObjectId), Instant>;

/// At most one gate per session and viewer this often, while nothing is
/// bound or relayed.
const GATE_EVERY: Duration = Duration::from_secs(5);

/// Past this many entries, the ones older than [`GATE_EVERY`] are dropped:
/// only a session waiting for its claim has one, so the map stays small.
const GATE_RUNS_KEPT: usize = 1024;

/// Whether `user_id`'s rejoin of `session_id` runs the gate now: the first
/// ask does, and then one every [`GATE_EVERY`]. Synchronous: no guard of the
/// map outlives this call.
fn gate_due(state: &RemoteState, session_id: ObjectId, user_id: ObjectId) -> bool {
    let runs = &state.rejoin_gates;
    if runs.len() > GATE_RUNS_KEPT {
        runs.retain(|_, at| at.elapsed() < GATE_EVERY);
    }
    let now = Instant::now();
    let mut due = true;
    runs.entry((session_id, user_id))
        .and_modify(|at| {
            if at.elapsed() < GATE_EVERY {
                due = false;
            } else {
                *at = now;
            }
        })
        .or_insert(now);
    due
}

/// The viewer's socket the rejoin came in on.
pub struct Viewer<'a> {
    pub user_id: ObjectId,
    pub name: &'a str,
    pub tx: &'a ClientTx,
    /// The address the relay routes the answer back to, when the session is
    /// held on another pod.
    pub connection_id: &'a str,
}

fn end(session_id: ObjectId, reason: EndReason) -> ServerMsg {
    ServerMsg::Terminate {
        session_id,
        reason,
        limit: None,
    }
}

fn detached(session_id: ObjectId, resume_by: DateTime) -> ServerMsg {
    ServerMsg::SessionDetached {
        session_id,
        resume_by: u64::try_from(resume_by.timestamp_millis()).unwrap_or(0),
    }
}

/// `window_secs` after `from`.
fn after(from: DateTime, window_secs: u32) -> DateTime {
    DateTime::from_millis(
        from.timestamp_millis()
            .saturating_add(i64::from(window_secs).saturating_mul(1000)),
    )
}

/// Review 2, N9 — the reason a rejoin is told when it finds the row ended.
/// The page that rejoins never hung up: its socket went. So a row ended
/// `controller_hangup` was ended by the hold's window running out (a rejoin
/// that lands just after it, before the device's close reaches the page) or
/// by the same person's other request reaping it, and the page reads
/// `controller_hangup` as its own hangup and stops. It is told
/// `agent_disconnect` instead, which it redials on, as a page before FR-138
/// redialed after every drop. Every other reason is the row's own.
fn rejoin_end_reason(row_reason: Option<EndReason>) -> EndReason {
    match row_reason {
        None | Some(EndReason::ControllerHangup) => EndReason::AgentDisconnect,
        Some(reason) => reason,
    }
}

/// The answer while the session waits for its claim: ask again until `by`,
/// and once `by` has passed, it is over.
fn until(session_id: ObjectId, by: DateTime, now: DateTime) -> ServerMsg {
    if by > now {
        detached(session_id, by)
    } else {
        end(session_id, EndReason::AgentDisconnect)
    }
}

/// Answer one `rc:session.rejoin` (the table above).
pub async fn rejoin(state: &RemoteState, v: Viewer<'_>, session_id: ObjectId) {
    let window =
        roomler_ai_mod_fleet::resume_scope::capped_window(state.settings.rc.session_resume_secs);
    if window == 0 {
        debug!(session = %session_id.to_hex(), "rc:session.rejoin dropped: the window is off");
        return;
    }
    let hub = &state.fleet.rc_hub;
    let answer = |msg: ServerMsg| {
        let _ = v.tx.try_send(msg);
    };

    // Held on this pod, for this person.
    if let Some(p) = hub
        .held_parties(session_id)
        .filter(|p| p.controller_user_id == v.user_id)
    {
        match resume_gate(state, v.user_id, p.tenant_id, p.agent_id).await {
            ResumeGate::Allow { .. } => match hub.rejoin(session_id, v.user_id, v.tx) {
                Ok(()) => return,
                // It ended, or may not be held: the row says how.
                Err(e) => {
                    debug!(session = %session_id.to_hex(), %e, "rc:session.rejoin: not held here any more; reading the row");
                }
            },
            ResumeGate::Deny(why) => {
                info!(session = %session_id.to_hex(), user = %v.user_id, %why, "rc:session.rejoin: the viewer may no longer control the device");
                hub.refuse_rejoin(session_id, v.user_id, v.tx, EndReason::AdminTerminated);
                return;
            }
            // Decided nothing: when the hold ends, and the viewer asks again.
            ResumeGate::Unknown(why) => {
                warn!(session = %session_id.to_hex(), %why, "rc:session.rejoin: the gate could not be run; ask again");
                let by = p
                    .held_until
                    .unwrap_or_else(|| after(DateTime::now(), window));
                answer(detached(session_id, by));
                return;
            }
        }
    }

    // Not held here: the row, which must name this person.
    let row = match state
        .remote_sessions
        .find_for_controller(session_id, v.user_id)
        .await
    {
        Ok(row) => row,
        Err(DaoError::NotFound) => {
            answer(end(session_id, EndReason::AgentDisconnect));
            return;
        }
        Err(e) => {
            warn!(session = %session_id.to_hex(), %e, "rc:session.rejoin: the row could not be read; ask again");
            answer(detached(session_id, after(DateTime::now(), window)));
            return;
        }
    };
    if row.ended_at.is_some() || row.phase == SessionPhase::Closed {
        answer(end(session_id, rejoin_end_reason(row.end_reason)));
        return;
    }
    if row.phase != SessionPhase::Active {
        answer(end(session_id, EndReason::AgentDisconnect));
        return;
    }
    // Detached when the device's socket went, and not claimed yet: nothing is
    // bound, so the gate runs once per `GATE_EVERY` for this viewer (N14).
    if let Some(by) = row.resume_by {
        if gate_due(state, session_id, v.user_id)
            && run_gate(state, &v, &row, session_id).await.is_none()
        {
            return;
        }
        answer(until(session_id, by, DateTime::now()));
        return;
    }
    // Live in the hub of the pod that holds the device's socket: bound there,
    // so the gate always runs first.
    let owner = if hub.is_agent_online(row.agent_id) {
        None
    } else {
        foreign_owner(state, row.agent_id).await
    };
    if let Some(owner) = owner {
        match run_gate(state, &v, &row, session_id).await {
            None => return,
            Some(true) => {
                if relay(state, &v, &owner, session_id).await {
                    return;
                }
            }
            Some(false) => {}
        }
    } else if gate_due(state, session_id, v.user_id)
        && run_gate(state, &v, &row, session_id).await.is_none()
    {
        return;
    }
    // In no hub yet: the device's claim is not answered, or the pod that held
    // it died without a word. The viewer may ask again until it would end.
    let by = wait_until(state, &row, window).await;
    answer(until(session_id, by, DateTime::now()));
}

/// The gate a request runs, for the viewer of a session no hub on this pod
/// holds. `None` when it refused: the row is ended as an admin's end is, and
/// the viewer is told `admin_terminated`. `Some(true)` when allowed;
/// `Some(false)` when a read failed and nothing was decided, so nothing is
/// bound, the deadline is answered, and the viewer asks again.
async fn run_gate(
    state: &RemoteState,
    v: &Viewer<'_>,
    row: &RemoteSession,
    session_id: ObjectId,
) -> Option<bool> {
    match resume_gate(state, v.user_id, row.tenant_id, row.agent_id).await {
        ResumeGate::Allow { .. } => Some(true),
        ResumeGate::Deny(why) => {
            info!(session = %session_id.to_hex(), user = %v.user_id, %why, "rc:session.rejoin: the viewer may no longer control the device");
            crate::row_end::revoked(state, row).await;
            let _ = v.tx.try_send(end(session_id, EndReason::AdminTerminated));
            None
        }
        ResumeGate::Unknown(why) => {
            warn!(session = %session_id.to_hex(), %why, "rc:session.rejoin: the gate could not be run; ask again");
            Some(false)
        }
    }
}

/// The pod that holds the device's socket, when it is another one.
async fn foreign_owner(state: &RemoteState, agent_id: ObjectId) -> Option<String> {
    let redis = state.redis_pubsub.as_ref()?;
    let owner = tokio::time::timeout(
        Duration::from_millis(250),
        redis.agent_presence_foreign(&agent_id.to_hex()),
    )
    .await
    .ok()?
    .ok()??;
    roomler_core::cluster::directory::OwnerRecord::parse(&owner)
        .map(|r| r.pod_id)
        .filter(|pod| !pod.is_empty())
}

/// Relay the rejoin to `owner`, whose hub binds it to a proxy for this
/// socket and answers through it. `true` when it was bound there. The gate
/// already ran here, as it does for a relayed request. Anything else (the
/// owner's hub does not hold it, or the bus failed) is answered here.
async fn relay(state: &RemoteState, v: &Viewer<'_>, owner: &str, session_id: ObjectId) -> bool {
    let Ok(frame) = serde_json::to_value(ClientMsg::SessionRejoin { session_id }) else {
        return false;
    };
    let relayed = crate::relay::relay_rc_frame(
        state,
        owner,
        v.connection_id,
        v.user_id,
        v.name,
        ConsentMode::Prompt,
        &None,
        None,
        &None,
        false,
        RcOverlayMode::Off,
        OrgPolicy::Unavailable,
        None,
        None,
        None,
        None,
        &frame,
    )
    .await;
    matches!(relayed, Ok(None))
}

/// When a live session no hub holds would end if nobody claims it: the
/// device's last word to any pod (`last_seen_at`) or the row's own last
/// clock, whichever is later, plus the window. The pod that held it wrote no
/// `resume_by`, so this is the server's best reading of the device's own
/// deadline, at most a heartbeat early.
async fn wait_until(state: &RemoteState, row: &RemoteSession, window: u32) -> DateTime {
    let row_clock = crate::resume::inferred_end(row);
    let from = match state
        .fleet
        .agents
        .find_live_in_tenant(row.tenant_id, row.agent_id)
        .await
    {
        Ok(agent) => agent.last_seen_at.max(row_clock),
        Err(_) => row_clock,
    };
    after(from, window)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(ms: i64) -> DateTime {
        DateTime::from_millis(ms)
    }

    /// While the deadline is ahead the viewer is told it and asks again; at
    /// it or past it the session is over, `agent_disconnect`.
    #[test]
    fn a_waiting_session_is_detached_until_its_deadline_then_ended() {
        let sid = ObjectId::new();
        match until(sid, at(10_000), at(9_999)) {
            ServerMsg::SessionDetached {
                session_id,
                resume_by,
            } => {
                assert_eq!(session_id, sid);
                assert_eq!(resume_by, 10_000);
            }
            other => panic!("{other:?}"),
        }
        for now in [10_000, 10_001] {
            assert!(matches!(
                until(sid, at(10_000), at(now)),
                ServerMsg::Terminate {
                    reason: EndReason::AgentDisconnect,
                    limit: None,
                    ..
                }
            ));
        }
        assert_eq!(after(at(1_000), 60), at(61_000));
        assert_eq!(after(at(i64::MAX - 1), 60), at(i64::MAX), "saturates");
    }

    /// Review 2, N9 — a page that rejoins never hung up, so a row ended
    /// `controller_hangup` (its hold ran out) answers a reason the page
    /// redials on; a row with no reason does too, and every other reason is
    /// the row's own.
    #[test]
    fn a_rejoin_that_finds_its_hold_ended_is_told_a_reason_it_redials_on() {
        assert_eq!(
            rejoin_end_reason(Some(EndReason::ControllerHangup)),
            EndReason::AgentDisconnect
        );
        assert_eq!(rejoin_end_reason(None), EndReason::AgentDisconnect);
        for reason in [
            EndReason::AgentDisconnect,
            EndReason::AdminTerminated,
            EndReason::HostDisconnect,
            EndReason::IdleTimeout,
            EndReason::Error,
        ] {
            assert_eq!(rejoin_end_reason(Some(reason)), reason, "{reason:?}");
        }
    }
}
