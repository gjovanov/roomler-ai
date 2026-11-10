// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-138 P1b-2 — an end someone asked for, of a session no hub on THIS pod
//! holds (`docs/fr/FR-138-the-session-outlives-its-control-socket.md` §4.4,
//! §4.6). During a gap no pod holds a detached session, so its end cannot be
//! pushed to it: the end waits in the row, and the device's claim and the
//! viewer's rejoin read it there.
//!
//! Each end here:
//!
//! 1. is written straight to the row (`RemoteSessionDao::end_unheld`), only
//!    while the row is live (Active, not ended) and names what it must: the
//!    route's org for an admin's end, the person for the controller's own,
//!    the person and the device for a redial's `replaces`. At any fencing
//!    number: an end someone asked for wins over a resume;
//! 2. goes in the audit as its trail (`RowEnd::written`), which projects
//!    nothing: the row is the writer's;
//! 3. is published, AFTER the write, as the control event the pod that does
//!    hold the session applies (`terminate` for an admin's,
//!    `controller_terminate` for the controller's). A claim reads the row
//!    once more after it has rebuilt the session, so it either finds the end
//!    or receives the event;
//! 4. ends the session on this pod too, when a claim rebuilt it here
//!    meanwhile: a pod never receives its own control events.
//!
//! With the window at 0 none of this runs, and an end of a session this pod
//! does not hold writes nothing, as before FR-138.

use bson::{DateTime, oid::ObjectId};
use roomler_ai_mod_fleet::hub::RowEnd;
use roomler_ai_remote_control::models::{EndReason, RemoteSession, SessionPhase};
use roomler_ai_services::dao::remote_session::EndScope;
use roomler_core::cluster::session_slots::SlotScope;
use tracing::{info, warn};

use crate::RemoteState;

/// The window is on: only then may a session be held anywhere, and only then
/// does an end of one this pod does not hold write its row.
pub(crate) fn enabled(state: &RemoteState) -> bool {
    state.settings.rc.session_resume_secs > 0
}

/// Review 2, N10 — what writing an end to a row did, so a caller never
/// reports an end it failed to write as taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Written {
    /// The row was live and now holds the end: the org it is in.
    Wrote { tenant_id: ObjectId },
    /// Nothing to write: the window is off, a hub on this pod holds the
    /// session, or the row is not live (or does not name what it must).
    NotLive,
    /// The write failed: the row may still be live, and a claim may still
    /// resume it.
    Failed,
}

/// Write the end to the row and record its trail.
async fn write(
    state: &RemoteState,
    session_id: ObjectId,
    scope: &EndScope,
    reason: EndReason,
) -> Written {
    match state
        .remote_sessions
        .end_unheld(session_id, scope, reason, DateTime::now())
        .await
    {
        Ok(Some(before)) => {
            state.fleet.rc_hub.record_end(
                session_id,
                before.agent_id,
                before.tenant_id,
                reason,
                None,
                RowEnd::written(None),
            );
            info!(
                session = %session_id.to_hex(),
                ?reason,
                "rc session no hub here holds: its end is written to its row"
            );
            // A detached session kept its FR-115 slot, and nobody renews it:
            // the end gives it back now rather than at the lease's expiry. A
            // pod that still holds the session gives it back again when the
            // control event ends it there.
            if crate::slots::enabled(state)
                && let Some(slots) = &state.session_slots
                && let Err(e) = slots
                    .release(SlotScope::Org(before.tenant_id), session_id)
                    .await
            {
                warn!(session = %session_id.to_hex(), %e, "session slot: an ended session's slot was not given back; it expires on its own");
            }
            Written::Wrote {
                tenant_id: before.tenant_id,
            }
        }
        Ok(None) => Written::NotLive,
        Err(e) => {
            warn!(session = %session_id.to_hex(), %e, "rc session end: its row could not be written");
            Written::Failed
        }
    }
}

/// A claim rebuilt the session on THIS pod between the asker's look and the
/// write: end it here, for the same org and (when it names one) person.
fn end_here_if_rebuilt(
    state: &RemoteState,
    session_id: ObjectId,
    tenant_id: ObjectId,
    user_id: Option<ObjectId>,
    reason: EndReason,
) {
    let hub = &state.fleet.rc_hub;
    if hub
        .session_snapshot(session_id)
        .is_some_and(|(t, u)| t == tenant_id && user_id.is_none_or(|x| x == u))
    {
        let _ = hub.terminate(session_id, reason);
    }
}

/// An admin's end (`POST …/session/{id}/terminate`) of a session no hub on
/// this pod holds, in the org the route names: written to the row. The route
/// publishes its `terminate` control event after this, as it always has.
/// Review 2, N10 — [`Written::Failed`] when the write failed: the route must
/// not answer that end as taken, since a claim may still resume the session.
pub async fn admin_end(state: &RemoteState, tenant_id: ObjectId, session_id: ObjectId) -> Written {
    if !enabled(state) || state.fleet.rc_hub.session_snapshot(session_id).is_some() {
        return Written::NotLive;
    }
    let scope = EndScope {
        tenant_id: Some(tenant_id),
        ..EndScope::default()
    };
    let written = write(state, session_id, &scope, EndReason::AdminTerminated).await;
    if matches!(written, Written::Wrote { .. }) {
        end_here_if_rebuilt(
            state,
            session_id,
            tenant_id,
            None,
            EndReason::AdminTerminated,
        );
    }
    written
}

/// The controller's own `rc:terminate` of a session no hub on this pod
/// holds: written to the row when it names this person, then published to
/// the pod that holds it. `host_disconnect` is the device's claim alone, and
/// reads `controller_hangup` from a controller, as the hub records it.
pub async fn controller_end(
    state: &RemoteState,
    user_id: ObjectId,
    session_id: ObjectId,
    reason: EndReason,
) {
    if !enabled(state) || state.fleet.rc_hub.session_snapshot(session_id).is_some() {
        return;
    }
    let reason = match reason {
        EndReason::HostDisconnect => EndReason::ControllerHangup,
        r => r,
    };
    let scope = EndScope {
        controller_user_id: Some(user_id),
        ..EndScope::default()
    };
    end_and_publish(state, session_id, user_id, &scope, reason).await;
}

/// A held session never costs its own viewer the slot (spec §4.4). Run for
/// `user_id`'s `rc:session.request` to `agent_id` BEFORE every capacity gate
/// it meets: the org's FR-115 slot, which the caller claims next, then the
/// hub's own count and busy gate.
///
/// * `replaces` names the held session the redial gives up. This pod's hub
///   ends it when it holds it for this person and this device
///   (`Hub::end_replaced`); otherwise its row is written as
///   `controller_hangup`, when the row names this person and this device,
///   and the pod that holds it is told.
/// * Every session held for this person on this device here ends too, as
///   the orphan reap ends one for its own person (`Hub::reap_held_for`): a
///   page too old to send `replaces`, or the person's other tab.
///
/// Each one's slot is given back, and waited for, before the caller claims
/// one. Nothing runs at window 0.
pub async fn before_request(
    state: &RemoteState,
    user_id: ObjectId,
    agent_id: ObjectId,
    replaces: Option<ObjectId>,
) {
    if !enabled(state) {
        return;
    }
    let hub = &state.fleet.rc_hub;
    let mut leases = Vec::new();
    if let Some(old) = replaces {
        match hub.end_replaced(old, user_id, agent_id) {
            Some(lease) => leases.extend(lease),
            // Held here, but not this person's on this device: left alone.
            None if hub.session_snapshot(old).is_some() => {}
            None => {
                let scope = EndScope {
                    controller_user_id: Some(user_id),
                    agent_id: Some(agent_id),
                    ..EndScope::default()
                };
                end_and_publish(state, old, user_id, &scope, EndReason::ControllerHangup).await;
            }
        }
    }
    leases.extend(hub.reap_held_for(user_id, agent_id).into_iter().flatten());
    if let Some(slots) = &state.session_slots {
        for lease in leases {
            if let Err(e) = slots.release(lease.scope, lease.session).await {
                warn!(session = %lease.session, %e, "session slot: a held session's slot was not given back; it expires on its own");
            }
        }
    }
}

/// The controller's end: the row, then the control event for the pod that
/// holds the session. A row that is not live (a session still waiting for
/// consent on the pod that holds it) is not written, and the event still
/// goes out while the row is open and names this person.
async fn end_and_publish(
    state: &RemoteState,
    session_id: ObjectId,
    user_id: ObjectId,
    scope: &EndScope,
    reason: EndReason,
) {
    let tenant = match write(state, session_id, scope, reason).await {
        Written::Wrote { tenant_id } => Some(tenant_id),
        Written::NotLive | Written::Failed => match state
            .remote_sessions
            .find_for_controller(session_id, user_id)
            .await
        {
            Ok(row)
                if row.ended_at.is_none()
                    && row.phase != SessionPhase::Closed
                    && scope.agent_id.is_none_or(|a| a == row.agent_id) =>
            {
                Some(row.tenant_id)
            }
            _ => None,
        },
    };
    let Some(tenant_id) = tenant else {
        return;
    };
    roomler_ai_mod_fleet::ctrl::publish_rc_ctrl(
        state,
        "controller_terminate",
        serde_json::json!({
            "session_id": session_id.to_hex(),
            "tenant_id": tenant_id.to_hex(),
            "user_id": user_id.to_hex(),
            "reason": reason,
        }),
    )
    .await;
    end_here_if_rebuilt(state, session_id, tenant_id, Some(user_id), reason);
}

/// The rejoin's gate refused a session no hub on this pod holds: its viewer
/// may no longer control the device. Ended as an admin's end is: written to
/// the row, then the `terminate` control event for the pod that holds it.
pub(crate) async fn revoked(state: &RemoteState, row: &RemoteSession) {
    let Some(session_id) = row.id else {
        return;
    };
    let scope = EndScope {
        tenant_id: Some(row.tenant_id),
        controller_user_id: Some(row.controller_user_id),
        ..EndScope::default()
    };
    let wrote = matches!(
        write(state, session_id, &scope, EndReason::AdminTerminated).await,
        Written::Wrote { .. }
    );
    roomler_ai_mod_fleet::ctrl::publish_rc_ctrl(
        state,
        "terminate",
        serde_json::json!({
            "session_id": session_id.to_hex(),
            "tenant_id": row.tenant_id.to_hex(),
        }),
    )
    .await;
    if wrote {
        end_here_if_rebuilt(
            state,
            session_id,
            row.tenant_id,
            Some(row.controller_user_id),
            EndReason::AdminTerminated,
        );
    }
}
