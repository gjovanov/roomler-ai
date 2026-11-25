// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! #4409 — a room's stored `member_count` is the number of its member rows.
//!
//! Until #4409 a new room was stored with a count of 1 and its creator's join
//! added another, so every room made before the fix reads one ahead. This
//! module's startup job, [`JOB`], runs one pass of [`MemberCountRepair`] at
//! every boot of every pod: it sets each room's count to its member rows where
//! the two differ, bounded, on a compare with the count it read, and logs one
//! line with its counts. Once the stored rows are right a pass reads and
//! changes nothing.
//!
//! It is not leader-gated. Every write it makes is a compare on the count it
//! read, so it stays right beside a live pod's own joins and leaves and beside
//! a second pass, and in a roll the last pod to boot runs a pass after the
//! last process of the old code is gone: gated, the one pass would run inside
//! the first pod's lease while the old pod still makes rooms one ahead.

use std::time::Instant;

use roomler_ai_services::dao::room_member_count::MemberCountRepair;
use tracing::info;

use crate::ChatState;

/// The startup job's name.
pub const JOB: &str = "room-member-count";

/// One pass, and its one line.
pub(crate) async fn repair(state: &ChatState) -> anyhow::Result<()> {
    let started = Instant::now();
    let pass = MemberCountRepair::new(&state.db).pass().await?;
    info!(
        job = JOB,
        rooms_read = pass.read,
        changed = pass.changed,
        skipped = pass.skipped,
        more = pass.more,
        ms = started.elapsed().as_millis() as u64,
        "room member counts: a pass"
    );
    Ok(())
}
