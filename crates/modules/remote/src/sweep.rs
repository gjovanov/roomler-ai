// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-138 P1b-2 — the sweep (`docs/fr/FR-138-the-session-outlives-its-control-socket.md`
//! §4.4, "The sweep"): what ends a session row whose end nobody saw, once no
//! device will ever claim it. One pod at a time, as the presence sweep runs
//! and on its cadence (`rc.presence_sweep_secs`): a per-cycle Redis claim
//! keyed by the database's name, its own key.
//!
//! | Rows it ends | When | Dated by |
//! |---|---|---|
//! | detached (`resume_by` set) and live past `resume_by` | at every window, 0 included: only a detach writes `resume_by`, so with the window at 0 from the start there are none, and a window turned back to 0 still ends what was detached before | `detached_at` |
//! | live with no `resume_by`, quiet for longer than the window, on a device in the resume scope that the presence sweep found offline for longer than the window: the pod that held it died without a word, and the device has not come back | only with the window on | `stats_at`, else `started_at` |
//!
//! Each end is `agent_disconnect` with `end_source: sweep`, written straight
//! to the row conditional on the row as the sweep read it: still live, at the
//! `attach_seq`, the `resume_by` and the `stats_at` read (a claim that resumed
//! it meanwhile bumps the one and clears the other; a session whose own clock
//! moved is alive), and recorded in the audit as its trail. The session's
//! FR-115 slot, which nobody renews once its session was detached, is given
//! back. A row outside the scope with no `resume_by` stays as it is, until
//! the scope reaches its device.

use std::time::Duration;

use bson::DateTime;
use roomler_ai_mod_fleet::hub::RowEnd;
use roomler_ai_remote_control::models::{EndReason, EndSource, RemoteSession};
use roomler_core::cluster::session_slots::SlotScope;
use tracing::{debug, info, warn};

use crate::RemoteState;

/// The most rows one cycle ends of each kind; the next cycle reads on.
const SWEEP_LIMIT: i64 = 200;

/// What one cycle ended.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Swept {
    /// Detached rows past their `resume_by`.
    pub detached: usize,
    /// Rows a pod that died without a word left open, on a device gone
    /// offline for longer than the window.
    pub abandoned: usize,
}

/// Spawn the sweep loop, on the presence sweep's cadence. The first cycle is
/// a full interval out, so a short-lived test server never races a test.
pub fn spawn(state: RemoteState) {
    let every = state.settings.rc.presence_sweep_secs.max(5);
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(every)).await;
            if let Some(redis) = &state.redis_pubsub {
                let key = format!("roomler:rc-resume-sweep:{}", state.db.name());
                let ttl = every.saturating_sub(5).max(5);
                match redis.try_claim(&key, ttl).await {
                    Ok(true) => {}
                    Ok(false) => continue,
                    Err(e) => {
                        debug!(%e, "rc resume sweep: the cycle's claim failed; skipping it");
                        continue;
                    }
                }
            }
            let swept = run_sweep(&state).await;
            if swept != Swept::default() {
                info!(
                    detached = swept.detached,
                    abandoned = swept.abandoned,
                    "rc resume sweep ended rows nobody will claim"
                );
            }
        }
    });
}

/// One cycle (the table above). Public so a test drives it directly.
pub async fn run_sweep(state: &RemoteState) -> Swept {
    let now = DateTime::now();
    let mut swept = Swept::default();

    match state.remote_sessions.detached_past(now, SWEEP_LIMIT).await {
        Ok(rows) => {
            for row in rows {
                let at = row
                    .detached_at
                    .unwrap_or_else(|| crate::resume::inferred_end(&row));
                if end_row(state, &row, at).await {
                    swept.detached += 1;
                }
            }
        }
        Err(e) => warn!(%e, "rc resume sweep: the detached rows could not be read"),
    }

    let window =
        roomler_ai_mod_fleet::resume_scope::capped_window(state.settings.rc.session_resume_secs);
    if window == 0 {
        return swept;
    }
    let window_ms = i64::from(window) * 1000;
    let quiet_since = DateTime::from_millis(now.timestamp_millis().saturating_sub(window_ms));
    let rows = match state
        .remote_sessions
        .unclaimed_quiet(quiet_since, SWEEP_LIMIT)
        .await
    {
        Ok(rows) => rows,
        Err(e) => {
            warn!(%e, "rc resume sweep: the quiet rows could not be read");
            return swept;
        }
    };
    for row in rows {
        if abandoned(state, &row, quiet_since).await
            && end_row(state, &row, crate::resume::inferred_end(&row)).await
        {
            swept.abandoned += 1;
        }
    }
    swept
}

/// A live row with no `resume_by` whose pod died without a word: its device
/// is in the scope, no pod here holds it, and the presence sweep found the
/// device offline before `quiet_since` (the window ago). Anything unread is
/// not abandoned: the row waits for the next cycle.
async fn abandoned(state: &RemoteState, row: &RemoteSession, quiet_since: DateTime) -> bool {
    let Some(session_id) = row.id else {
        return false;
    };
    let hub = &state.fleet.rc_hub;
    if hub.session_snapshot(session_id).is_some() || hub.is_agent_online(row.agent_id) {
        return false;
    }
    let agent = match state
        .fleet
        .agents
        .find_live_in_tenant(row.tenant_id, row.agent_id)
        .await
    {
        Ok(agent) => agent,
        Err(e) => {
            debug!(session = %session_id, %e, "rc resume sweep: the device could not be read; left for later");
            return false;
        }
    };
    let offline = agent.last_presence.as_deref() == Some(roomler_ai_mod_fleet::presence::OFFLINE)
        && agent.last_seen_at < quiet_since;
    if !offline {
        return false;
    }
    match state
        .fleet
        .rc_resume_scope
        .covers(row.tenant_id, row.agent_id)
        .await
    {
        Ok(covered) => covered,
        Err(e) => {
            debug!(session = %session_id, %e, "rc resume sweep: the scope could not be read; left for later");
            false
        }
    }
}

/// End one row the sweep found, as it read it, dated `at`; its trail in the
/// audit and its slot given back. `true` when it ended it.
async fn end_row(state: &RemoteState, row: &RemoteSession, at: DateTime) -> bool {
    let Some(session_id) = row.id else {
        return false;
    };
    match state.remote_sessions.end_swept(row, at).await {
        Ok(true) => {}
        Ok(false) => return false,
        Err(e) => {
            warn!(session = %session_id, %e, "rc resume sweep: the row could not be written");
            return false;
        }
    }
    info!(
        session = %session_id,
        agent = %row.agent_id,
        ended_at_ms = at.timestamp_millis(),
        "rc session nobody will claim — swept at its own last clock"
    );
    state.fleet.rc_hub.record_end(
        session_id,
        row.agent_id,
        row.tenant_id,
        EndReason::AgentDisconnect,
        None,
        RowEnd::inferred(row.attach_seq, at, EndSource::Sweep).already_written(),
    );
    if crate::slots::enabled(state)
        && let Some(slots) = &state.session_slots
        && let Err(e) = slots
            .release(SlotScope::Org(row.tenant_id), session_id)
            .await
    {
        debug!(session = %session_id, %e, "rc resume sweep: the slot was not given back; it expires on its own");
    }
    true
}
