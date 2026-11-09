// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-138 — the server's answer to a device that comes back for the sessions
//! it kept (`docs/fr/FR-138-the-session-outlives-its-control-socket.md`
//! §4.4), run in this module's agent-socket `hello`, before the socket's read
//! loop starts. Only for a connection announced a window: with the window at
//! 0, or a device outside the scope or without `rc-resume`, nothing here runs
//! and nothing is read.
//!
//! **The claim.** For each session the hello names, the pod the device
//! reached decides from the session's ROW, never from any pod's memory:
//!
//! | Check | Fails as |
//! |---|---|
//! | the row exists in the device's tenant and names this device | `agent_disconnect`, nothing written |
//! | the row is live and not ended | the row's own end reason (an admin's end written during the gap reads `admin_terminated`), nothing written |
//! | `resume_by` has not passed, when the row has one | `agent_disconnect`, the row ended as inferred |
//! | the gate a request runs, run again (`controller::authorize_resume`) | `admin_terminated` |
//! | the org's maximum length, counted from `started_at` | `idle_timeout`, limit `max_duration` |
//! | the FR-115 slot, taken again under the session's own id | `agent_disconnect` |
//!
//! A claim that passes commits with one conditional update that bumps the
//! fencing number ([`RemoteSessionDao::commit_resume`]); the hub rebuilds the
//! session and counts it; the row is read once more, so an end that landed
//! meanwhile wins; and only then does the device hear `rc:session.resumed`.
//! Consent is the one check not re-run.
//!
//! **The registration read.** Then every live row of the device created
//! before this registration that the hello does not claim (and the hub did
//! not just reap) is ended as `agent_disconnect`, dated by the row's own last
//! clock ([`inferred_end`]) and marked `end_source: registration`, fenced at
//! the number read. That is what closes a row a pod that died left open.
//!
//! [`RemoteSessionDao::commit_resume`]: roomler_ai_services::dao::remote_session::RemoteSessionDao::commit_resume

use std::time::Duration;

use bson::DateTime;
use bson::oid::ObjectId;
use roomler_ai_mod_fleet::hub::{Resumed, ResumedSession, RowEnd};
use roomler_ai_remote_control::models::{EndReason, EndSource, RemoteSession, SessionPhase};
use roomler_ai_services::dao::base::DaoError;
use roomler_core::AgentCtx;
use roomler_core::cluster::session_slots::{Lease, SlotScope};
use tracing::{debug, info, warn};

use crate::RemoteState;
use crate::slots::SlotDecision;

/// The most rows an earlier connection left open that one registration ends.
/// A device holds at most its capacity of live sessions, so past this the
/// next registration reads on.
const REGISTRATION_READ_LIMIT: i64 = 64;

/// The device's hello landed under a window: answer each claim, then end
/// what earlier connections left open.
pub async fn on_hello(state: &RemoteState, ctx: &AgentCtx) {
    if ctx.resume.window_secs == 0 {
        return;
    }
    for &session_id in &ctx.resume.claims {
        claim(state, ctx, session_id).await;
    }
    registration_read(state, ctx).await;
}

/// The best-known real end of a session whose end nobody saw (spec §4.4):
/// when it was detached, else its last stats merge, else when it went live
/// (else when it was asked for). Never the reader's clock, so a row left
/// open for weeks adds no hours.
pub fn inferred_end(row: &RemoteSession) -> DateTime {
    row.detached_at
        .or(row.stats_at)
        .or(row.started_at)
        .unwrap_or(row.created_at)
}

fn refuse(
    state: &RemoteState,
    ctx: &AgentCtx,
    session_id: ObjectId,
    reason: EndReason,
    limit: Option<&'static str>,
    row: Option<RowEnd>,
) {
    state
        .fleet
        .rc_hub
        .refuse_claim(ctx.agent_id, &ctx.tx, session_id, reason, limit, row);
}

/// Decide one claim, and answer it.
async fn claim(state: &RemoteState, ctx: &AgentCtx, session_id: ObjectId) {
    let agent = ctx.agent_id;
    // The row, resolved in the device's tenant and naming the device: a claim
    // never reaches a row of another tenant or device, and writes none.
    let row = match state
        .remote_sessions
        .find_in_tenant(ctx.tenant_id, session_id)
        .await
    {
        Ok(row) if row.agent_id == agent => row,
        Ok(_) => {
            warn!(%agent, session = %session_id, "rc session claim names another device's session — refused");
            return refuse(
                state,
                ctx,
                session_id,
                EndReason::AgentDisconnect,
                None,
                None,
            );
        }
        Err(DaoError::NotFound) => {
            info!(%agent, session = %session_id, "rc session claim names no session of the device's org — refused");
            return refuse(
                state,
                ctx,
                session_id,
                EndReason::AgentDisconnect,
                None,
                None,
            );
        }
        Err(e) => {
            warn!(%agent, session = %session_id, %e, "rc session claim: the row could not be read — refused");
            return refuse(
                state,
                ctx,
                session_id,
                EndReason::AgentDisconnect,
                None,
                None,
            );
        }
    };
    // Ended while no pod held it (an admin's end, the controller's): its own
    // reason, and the row is left as it is.
    if row.ended_at.is_some() || row.phase == SessionPhase::Closed {
        let reason = row.end_reason.unwrap_or(EndReason::AgentDisconnect);
        return refuse(state, ctx, session_id, reason, None, None);
    }
    if row.phase != SessionPhase::Active {
        return refuse(
            state,
            ctx,
            session_id,
            EndReason::AgentDisconnect,
            None,
            None,
        );
    }
    let seq = row.attach_seq;
    let now = DateTime::now();
    if row.resume_by.is_some_and(|by| by <= now) {
        let end = RowEnd::inferred(seq, inferred_end(&row), EndSource::Registration);
        return refuse(
            state,
            ctx,
            session_id,
            EndReason::AgentDisconnect,
            None,
            Some(end),
        );
    }
    let authz = match crate::controller::authorize_resume(state, row.controller_user_id, agent)
        .await
    {
        Ok(authz) => authz,
        Err(why) => {
            info!(%agent, session = %session_id, %why, "rc session claim: the controller may no longer control the device — refused");
            let end = RowEnd::fenced(seq);
            return refuse(
                state,
                ctx,
                session_id,
                EndReason::AdminTerminated,
                None,
                Some(end),
            );
        }
    };
    // FR-101 — the org's maximum length runs from when the session went
    // live, never from the resume.
    let started_at = row.started_at.unwrap_or(row.created_at);
    let max_duration = match authz.max_duration {
        None => None,
        Some(limit) => {
            let lived_ms = now
                .timestamp_millis()
                .saturating_sub(started_at.timestamp_millis());
            let lived = Duration::from_millis(u64::try_from(lived_ms).unwrap_or(0));
            match limit.checked_sub(lived) {
                Some(left) if !left.is_zero() => Some((left, limit)),
                _ => {
                    let end = RowEnd::fenced(seq);
                    return refuse(
                        state,
                        ctx,
                        session_id,
                        EndReason::IdleTimeout,
                        Some("max_duration"),
                        Some(end),
                    );
                }
            }
        }
    };
    let Ok(slot) = reclaim_slot(state, &row, session_id, authz.org_session_cap).await else {
        info!(%agent, session = %session_id, "rc session claim: the org's slots are taken — refused");
        let end = RowEnd::fenced(seq);
        return refuse(
            state,
            ctx,
            session_id,
            EndReason::AgentDisconnect,
            None,
            Some(end),
        );
    };

    // The commit: at the number read, still live, its window still open.
    let committed = state
        .remote_sessions
        .commit_resume(ctx.tenant_id, agent, session_id, seq, now)
        .await;
    if !matches!(committed, Ok(true)) {
        if let Some(lease) = &slot {
            crate::slots::release_unused(state, lease, None).await;
        }
        if let Err(e) = &committed {
            warn!(%agent, session = %session_id, %e, "rc session claim: the commit failed — refused");
        }
        // Someone ended it, another pod resumed it, or its window closed:
        // read it again and answer from what is there.
        let reason = match state
            .remote_sessions
            .find_in_tenant(ctx.tenant_id, session_id)
            .await
        {
            Ok(r) if r.ended_at.is_some() => r.end_reason.unwrap_or(EndReason::AgentDisconnect),
            _ => EndReason::AgentDisconnect,
        };
        return refuse(state, ctx, session_id, reason, None, None);
    }
    let next = seq + 1;

    let built = state.fleet.rc_hub.resume_claimed(
        agent,
        &ctx.tx,
        ResumedSession {
            session_id,
            tenant_id: ctx.tenant_id,
            controller_user_id: row.controller_user_id,
            permissions: row.permissions,
            created_at: row.created_at,
            started_at,
            attach_seq: next,
            max_duration,
            slot: slot.clone(),
        },
    );
    if let Some(lease) = &slot {
        crate::slots::settle(lease);
    }
    if built == Resumed::NotPending {
        // Displaced or closed while the claim was decided: the row stays at
        // the committed number, for whichever connection claims it next.
        info!(%agent, session = %session_id, "rc session claim: the connection that made it is gone");
        return;
    }

    // An end wins over a resume: one that landed after the commit is in the
    // row now, and one after this read reaches the hub by its control event.
    match state
        .remote_sessions
        .find_in_tenant(ctx.tenant_id, session_id)
        .await
    {
        Ok(r) if r.ended_at.is_some() => {
            let reason = r.end_reason.unwrap_or(EndReason::AgentDisconnect);
            info!(%agent, session = %session_id, ?reason, "rc session claim: an end landed before the resume — ended");
            state.fleet.rc_hub.drop_resumed(session_id, reason);
            return;
        }
        Ok(_) => {}
        Err(e) => {
            debug!(%agent, session = %session_id, %e, "rc session claim: the second read failed; the resume stands");
        }
    }
    state
        .fleet
        .rc_hub
        .confirm_resumed(agent, &ctx.tx, session_id, next);
}

/// FR-115 — the session's slot taken again under its own id: its own lease,
/// renewed, when it outlived the gap; a new one when the org has room. `Err`
/// when the org is full (another session took the slot during the gap).
/// `None` when slots are off or the org has no cap.
async fn reclaim_slot(
    state: &RemoteState,
    row: &RemoteSession,
    session_id: ObjectId,
    cap: Option<u32>,
) -> Result<Option<Lease>, ()> {
    let Some(cap) = cap else {
        return Ok(None);
    };
    if !crate::slots::enabled(state) {
        return Ok(None);
    }
    match crate::slots::claim_for(
        state,
        SlotScope::Org(row.tenant_id),
        session_id,
        row.controller_user_id,
        row.agent_id,
        cap,
    )
    .await
    {
        SlotDecision::Claimed(lease) => Ok(Some(lease)),
        SlotDecision::Fallback => Ok(None),
        SlotDecision::Full { .. } => Err(()),
    }
}

/// End every live row of the device that an earlier connection left open:
/// created before this registration, not claimed by this hello, not reaped
/// by the hub just now (whose end is real). Each is fenced at the number read
/// and dated by its own last clock.
async fn registration_read(state: &RemoteState, ctx: &AgentCtx) {
    let Some(before) = ctx.resume.registered_at else {
        return;
    };
    let rows = match state
        .remote_sessions
        .open_rows_before(ctx.tenant_id, ctx.agent_id, before, REGISTRATION_READ_LIMIT)
        .await
    {
        Ok(rows) => rows,
        Err(e) => {
            warn!(agent = %ctx.agent_id, %e, "rc registration read failed; the next registration reads again");
            return;
        }
    };
    for row in rows {
        let Some(session_id) = row.id else {
            continue;
        };
        if ctx.resume.claims.contains(&session_id)
            || ctx.resume.reaped.contains(&session_id)
            || state.fleet.rc_hub.session_snapshot(session_id).is_some()
        {
            continue;
        }
        let at = inferred_end(&row);
        info!(
            agent = %ctx.agent_id,
            session = %session_id,
            ended_at_ms = at.timestamp_millis(),
            "rc session an earlier connection left open — ended at its own last clock"
        );
        state.fleet.rc_hub.record_end(
            session_id,
            ctx.agent_id,
            ctx.tenant_id,
            EndReason::AgentDisconnect,
            None,
            RowEnd::inferred(row.attach_seq, at, EndSource::Registration),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use roomler_ai_remote_control::models::SessionStats;
    use roomler_ai_remote_control::permissions::Permissions;

    fn row(
        created: i64,
        started: Option<i64>,
        stats: Option<i64>,
        detached: Option<i64>,
    ) -> RemoteSession {
        let at = DateTime::from_millis;
        RemoteSession {
            id: Some(ObjectId::new()),
            agent_id: ObjectId::new(),
            tenant_id: ObjectId::new(),
            controller_user_id: ObjectId::new(),
            watchers: Vec::new(),
            permissions: Permissions::VIEW,
            phase: SessionPhase::Active,
            created_at: at(created),
            started_at: started.map(at),
            ended_at: None,
            end_reason: None,
            recording_url: None,
            stats: SessionStats::default(),
            carrier: None,
            video: None,
            attach_seq: 0,
            detached_at: detached.map(at),
            resume_by: None,
            stats_at: stats.map(at),
            end_source: None,
        }
    }

    /// The spec's order: `detached_at`, else `stats_at`, else `started_at`,
    /// and `created_at` for a row that never went live. Never later than the
    /// row's own last clock.
    #[test]
    fn an_inferred_end_is_the_rows_own_last_clock() {
        let at = DateTime::from_millis;
        assert_eq!(inferred_end(&row(1, Some(2), Some(3), Some(4))), at(4));
        assert_eq!(inferred_end(&row(1, Some(2), Some(3), None)), at(3));
        assert_eq!(inferred_end(&row(1, Some(2), None, None)), at(2));
        assert_eq!(inferred_end(&row(1, None, None, None)), at(1));
    }
}
