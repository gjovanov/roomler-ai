// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! #4448 — a room's stored `message_count` is the number of its live messages.
//!
//! `MessageDao` moves the count beside each message it writes or deletes;
//! this module keeps it right when something went past that, with
//! [`MessageCountRepair`]'s sweeps (its module doc says how a pass is bounded
//! and why it is safe beside live traffic and beside another pass):
//!
//! - [`JOB`], a startup job on every boot of every pod: it begins a new sweep
//!   and runs one pass. Not leader-gated, on the member-count repair's grounds
//!   ([`crate::member_count`]): in a roll the last pod to boot begins a sweep
//!   after the last process of the old code, which writes messages with no
//!   count beside them, is gone.
//! - [`SWEEP_JOB`], a periodic job on every pod, every
//!   [`roomler_ai_services::dao::room_message_count::TICK`]: one pass when one
//!   is due, so a sweep a pass's budget stopped finishes without a deploy, and
//!   a finished sweep is begun again once a day.
//!
//! Each pass logs one line. In steady state it sets no count; a pass that
//! sets any logs at warn, with the rooms and by how much, because it means a
//! writer missed a message.

use std::time::Instant;

use bson::DateTime;
use roomler_ai_services::dao::room_message_count::{MessageCountPass, MessageCountRepair};
use tracing::{debug, info, warn};

use crate::ChatState;

/// The startup job's name.
pub const JOB: &str = "room-message-count";
/// The periodic job's name.
pub const SWEEP_JOB: &str = "room-message-count-sweep";

/// At boot: a new sweep, and its first pass.
pub(crate) async fn at_boot(state: &ChatState) -> anyhow::Result<()> {
    let repair = MessageCountRepair::new(&state.db);
    repair.begin_sweep().await?;
    let started = Instant::now();
    let pass = repair.pass().await?;
    log(JOB, &pass, started);
    Ok(())
}

/// On the periodic tick: one pass, if one is due.
pub(crate) async fn tick(state: &ChatState) -> anyhow::Result<()> {
    let repair = MessageCountRepair::new(&state.db);
    if !repair.due(DateTime::now()).await? {
        debug!(job = SWEEP_JOB, "room message counts: no pass due");
        return Ok(());
    }
    let started = Instant::now();
    let pass = repair.pass().await?;
    log(SWEEP_JOB, &pass, started);
    Ok(())
}

/// A pass's one line: warn when it set a count, info otherwise.
fn log(job: &'static str, pass: &MessageCountPass, started: Instant) {
    let ms = started.elapsed().as_millis() as u64;
    if pass.corrected() {
        warn!(
            job,
            rooms_read = pass.read,
            changed = pass.changed,
            raised = pass.raised,
            lowered = pass.lowered,
            skipped = pass.skipped,
            more = pass.more,
            cursor_moved = pass.cursor_moved,
            ms,
            "room message counts: a pass corrected stored counts (a writer missed messages)"
        );
    } else {
        info!(
            job,
            rooms_read = pass.read,
            changed = 0u64,
            skipped = pass.skipped,
            more = pass.more,
            cursor_moved = pass.cursor_moved,
            ms,
            "room message counts: a pass"
        );
    }
}
