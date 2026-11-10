// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-138 P1b-2 — `rc:session.rejoin`, the viewer's claim on a session held
//! for it (`docs/fr/FR-138-the-session-outlives-its-control-socket.md` §4.4,
//! §4.5), answered on the pod the viewer's new socket reached.
//!
//! | Where the session is | Answer |
//! |---|---|
//! | held by this pod's hub, for this person | the gate a request runs, again; then bound here and `rc:session.resumed`, or ended `admin_terminated` |
//! | the row names someone else, or nothing | `rc:terminate agent_disconnect`, the same as for no session: nothing leaks |
//! | the row is ended | `rc:terminate` with the row's own reason |
//! | the row is live, and the gate refuses | written to the row as `admin_terminated`, and published, as an admin's end; `rc:terminate admin_terminated` |
//! | the gate cannot be run (a read failed), or the row cannot be read | no answer: the viewer asks again in a second, and nothing is written |
//! | detached by the device's side, inside `resume_by` | `rc:session.detached` with the row's `resume_by`: ask again |
//! | detached, past `resume_by` | `rc:terminate agent_disconnect` |
//! | live in the hub of the pod that holds the device's socket | relayed there (`rc.cmd`), bound there, answered through the relay |
//! | live, but in no hub yet (the claim is not answered, or a pod died without a word) | `rc:session.detached` with when it would end, or `rc:terminate agent_disconnect` once that has passed |
//!
//! With the window at 0 the rejoin is dropped unanswered, as a server before
//! FR-138 drops it, and the viewer's own deadline falls back to its redial.

use std::time::Duration;

use bson::{DateTime, oid::ObjectId};
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
            ResumeGate::Allow { .. } => {
                if let Err(e) = hub.rejoin(session_id, v.user_id, v.tx) {
                    debug!(session = %session_id.to_hex(), %e, "rc:session.rejoin: not held any more");
                    answer(end(session_id, EndReason::AgentDisconnect));
                }
            }
            ResumeGate::Deny(why) => {
                info!(session = %session_id.to_hex(), user = %v.user_id, %why, "rc:session.rejoin: the viewer may no longer control the device");
                hub.refuse_rejoin(session_id, v.user_id, v.tx, EndReason::AdminTerminated);
            }
            // Decided nothing: unanswered, and the viewer asks again.
            ResumeGate::Unknown(why) => {
                warn!(session = %session_id.to_hex(), %why, "rc:session.rejoin: the gate could not be run");
            }
        }
        return;
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
            // Unanswered: the viewer asks again in a second.
            warn!(session = %session_id.to_hex(), %e, "rc:session.rejoin: the row could not be read");
            return;
        }
    };
    if row.ended_at.is_some() || row.phase == SessionPhase::Closed {
        answer(end(
            session_id,
            row.end_reason.unwrap_or(EndReason::AgentDisconnect),
        ));
        return;
    }
    if row.phase != SessionPhase::Active {
        answer(end(session_id, EndReason::AgentDisconnect));
        return;
    }
    match resume_gate(state, v.user_id, row.tenant_id, row.agent_id).await {
        ResumeGate::Allow { .. } => {}
        ResumeGate::Deny(why) => {
            info!(session = %session_id.to_hex(), user = %v.user_id, %why, "rc:session.rejoin: the viewer may no longer control the device");
            crate::row_end::revoked(state, &row).await;
            answer(end(session_id, EndReason::AdminTerminated));
            return;
        }
        ResumeGate::Unknown(why) => {
            warn!(session = %session_id.to_hex(), %why, "rc:session.rejoin: the gate could not be run");
            return;
        }
    }
    let now = DateTime::now();
    if let Some(by) = row.resume_by {
        // Detached when the device's socket went, and not claimed yet.
        if by > now {
            answer(detached(session_id, by));
        } else {
            answer(end(session_id, EndReason::AgentDisconnect));
        }
        return;
    }
    // Live in the hub of the pod that holds the device's socket: bound there.
    if !hub.is_agent_online(row.agent_id)
        && let Some(owner) = foreign_owner(state, row.agent_id).await
        && relay(state, &v, &owner, session_id).await
    {
        return;
    }
    // In no hub yet: the device's claim is not answered, or the pod that held
    // it died without a word. The viewer may ask again until it would end.
    match wait_until(state, &row, window).await {
        by if by > now => answer(detached(session_id, by)),
        _ => answer(end(session_id, EndReason::AgentDisconnect)),
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
/// already ran here, as it does for a relayed request.
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
    DateTime::from_millis(
        from.timestamp_millis()
            .saturating_add(i64::from(window).saturating_mul(1000)),
    )
}
