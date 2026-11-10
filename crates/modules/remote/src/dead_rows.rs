// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-138 P1b-2 — `POST /api/admin/rc-sessions/close-dead`: the one-time
//! close of the session rows a pod that died before FR-138 left open
//! (`docs/fr/FR-138-the-session-outlives-its-control-socket.md` §2.3, §10).
//! P0 counted 11 such rows on production, started 2026-08-12 to 10-31, each
//! on a device that has had a session since: 17,757.7 hours the growth
//! digest read as live. FR-138's registration read and sweep close the rows
//! a dying pod leaves from now on. This closes the ones from before.
//!
//! Platform operators only: anyone else gets the 404 every `/api/admin/*`
//! route gives. A dry run by default, which lists what a real run would
//! close and writes nothing. Bounded by `limit`, and idempotent: a second
//! run finds nothing left.
//!
//! A row is dead when all of these hold:
//!
//! | Check | Why |
//! |---|---|
//! | live (Active, no `ended_at`) and created more than `min_age_hours` ago (24 by default) | a live session younger than that is someone's |
//! | its own clock has not moved since: no `stats_at`, or one that old | a live session's stats land about every 15 s |
//! | no hub on this pod holds it | a session this pod runs is not dead |
//! | its device has a session row created after it, in its org | the device came back and was used, so nothing will ever claim this one |
//!
//! A real run ends each as `agent_disconnect`, dated by the row's own last
//! clock (`crate::resume::inferred_end`: `detached_at`, else `stats_at`, else
//! `started_at`, else `created_at`; never the operator's clock, so it adds
//! no hours) and marked `end_source: backfill`. Each write is conditional on
//! the row as it was read: still live, at the fencing number, the
//! `resume_by` and the `stats_at` read.

use axum::{Json, extract::State};
use bson::DateTime;
use roomler_ai_mod_fleet::agent::fmt_dt;
use roomler_ai_mod_fleet::hub::RowEnd;
use roomler_ai_remote_control::models::{EndReason, EndSource, RemoteSession};
use roomler_core::{ApiError, extractors::auth::AuthUser, guards::require_platform_admin};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::RemoteState;
use crate::resume::inferred_end;

/// How many rows one call reads at most, by default and at most.
const DEFAULT_LIMIT: u32 = 50;
const MAX_LIMIT: u32 = 500;
/// How old a live row must be before it may be dead, in hours, by default.
const DEFAULT_MIN_AGE_HOURS: u32 = 24;
/// It may never be younger than this, whatever the call asks.
const MIN_AGE_HOURS_FLOOR: u32 = 1;

fn yes() -> bool {
    true
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CloseDeadRequest {
    /// List only, write nothing. On unless the body says otherwise.
    #[serde(default = "yes")]
    pub dry_run: bool,
    /// How many rows one call reads: [`DEFAULT_LIMIT`] by default,
    /// [`MAX_LIMIT`] at most.
    pub limit: Option<u32>,
    /// How old a live row must be, in hours: [`DEFAULT_MIN_AGE_HOURS`] by
    /// default, never under [`MIN_AGE_HOURS_FLOOR`].
    pub min_age_hours: Option<u32>,
}

/// One dead row, as it was read and as a real run ends it.
#[derive(Debug, Serialize)]
pub struct DeadRow {
    pub session_id: String,
    pub tenant_id: String,
    pub agent_id: String,
    pub created_at: String,
    pub started_at: Option<String>,
    /// The `ended_at` a real run writes: the row's own last clock.
    pub ends_at: String,
    /// Hours the row has been open, which the growth digest read as live.
    pub open_hours: f64,
}

#[derive(Debug, Serialize)]
pub struct CloseDeadResponse {
    pub dry_run: bool,
    /// The dead rows found, up to the limit.
    pub rows: Vec<DeadRow>,
    /// Rows this call ended (always 0 in a dry run).
    pub closed: u64,
    /// Rows a real run could not end: it ended, was resumed or detached, or
    /// its own clock moved, meanwhile, or the write failed (in the log). A
    /// later call reads again.
    pub skipped: u64,
    /// The read stopped at the limit: call again.
    pub more: bool,
}

/// `POST /api/admin/rc-sessions/close-dead` (the module doc).
pub async fn close_dead(
    State(state): State<RemoteState>,
    auth: AuthUser,
    Json(body): Json<CloseDeadRequest>,
) -> Result<Json<CloseDeadResponse>, ApiError> {
    require_platform_admin(&state, &auth)?;
    let limit = body.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let min_age_hours = body
        .min_age_hours
        .unwrap_or(DEFAULT_MIN_AGE_HOURS)
        .max(MIN_AGE_HOURS_FLOOR);
    let now = DateTime::now();
    let older_than = DateTime::from_millis(
        now.timestamp_millis()
            .saturating_sub(i64::from(min_age_hours) * 3_600_000),
    );

    let candidates = state
        .remote_sessions
        .dead_candidates(older_than, i64::from(limit) + 1)
        .await?;
    let more = candidates.len() > limit as usize;
    let mut dead = Vec::new();
    for row in candidates.into_iter().take(limit as usize) {
        let Some(session_id) = row.id else { continue };
        if state.fleet.rc_hub.session_snapshot(session_id).is_some() {
            continue;
        }
        if !state
            .remote_sessions
            .has_later_row(row.tenant_id, row.agent_id, row.created_at)
            .await?
        {
            continue;
        }
        dead.push(row);
    }

    let (mut closed, mut skipped) = (0u64, 0u64);
    if !body.dry_run {
        for row in &dead {
            let Some(session_id) = row.id else { continue };
            let at = inferred_end(row);
            match state.remote_sessions.close_dead(row, at).await {
                Ok(true) => {
                    closed += 1;
                    state.fleet.rc_hub.record_end(
                        session_id,
                        row.agent_id,
                        row.tenant_id,
                        EndReason::AgentDisconnect,
                        None,
                        RowEnd::inferred(row.attach_seq, at, EndSource::Backfill).already_written(),
                    );
                }
                Ok(false) => skipped += 1,
                Err(e) => {
                    skipped += 1;
                    warn!(session = %session_id, %e, "rc dead-row close: the row could not be written");
                }
            }
        }
    }
    let rows: Vec<DeadRow> = dead
        .iter()
        .filter_map(|row| {
            let session_id = row.id?;
            let at = inferred_end(row);
            Some(DeadRow {
                session_id: session_id.to_hex(),
                tenant_id: row.tenant_id.to_hex(),
                agent_id: row.agent_id.to_hex(),
                created_at: fmt_dt(row.created_at),
                started_at: row.started_at.map(fmt_dt),
                ends_at: fmt_dt(at),
                open_hours: open_hours(row, now),
            })
        })
        .collect();
    info!(
        operator = %auth.user_id,
        dry_run = body.dry_run,
        rows = rows.len(),
        closed,
        skipped,
        more,
        "rc dead-row close"
    );
    Ok(Json(CloseDeadResponse {
        dry_run: body.dry_run,
        rows,
        closed,
        skipped,
        more,
    }))
}

/// Hours from when the row went live (else was created) to `now`: what an
/// open row has added to the digest's session hours so far.
fn open_hours(row: &RemoteSession, now: DateTime) -> f64 {
    let from = row.started_at.unwrap_or(row.created_at);
    let ms = now
        .timestamp_millis()
        .saturating_sub(from.timestamp_millis());
    (ms as f64 / 3_600_000.0 * 10.0).round() / 10.0
}
