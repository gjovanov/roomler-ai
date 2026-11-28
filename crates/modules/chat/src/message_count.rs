// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! #4448 — a room's stored `message_count` is the number of its live messages.
//!
//! Until #4448 nothing wrote the count, so every room made before the fix
//! reads 0 over its messages. This module's startup job, [`JOB`], runs one
//! pass of [`MessageCountRepair`] at every boot of every pod: it sets each
//! room's count to its live messages where the two differ, bounded, on a
//! compare with the count it read, and logs one line with its counts. Once
//! the stored rows are right a pass reads the live messages of the rooms it
//! reaches in its budget and changes nothing.
//!
//! It is not leader-gated, on the member-count repair's grounds
//! ([`crate::member_count`]): every write is a compare on the count it read,
//! and in a roll the last pod to boot passes after the last process of the
//! old code, which writes messages with no count beside them, is gone.

use std::time::Instant;

use roomler_ai_services::dao::room_message_count::MessageCountRepair;
use tracing::info;

use crate::ChatState;

/// The startup job's name.
pub const JOB: &str = "room-message-count";

/// One pass, and its one line.
pub(crate) async fn repair(state: &ChatState) -> anyhow::Result<()> {
    let started = Instant::now();
    let pass = MessageCountRepair::new(&state.db).pass().await?;
    info!(
        job = JOB,
        rooms_read = pass.read,
        changed = pass.changed,
        skipped = pass.skipped,
        more = pass.more,
        ms = started.elapsed().as_millis() as u64,
        "room message counts: a pass"
    );
    Ok(())
}
