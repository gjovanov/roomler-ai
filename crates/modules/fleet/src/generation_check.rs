// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-155 P1c — every pod ends a registered entry below its row's current
//! `token_gen` within one period of `rc.presence_sweep_secs` (never under
//! 5 s, 30 s by default), plus the time of one read. This holds on a pod
//! that the rotation's control event does not reach, too: each pod checks
//! its own hub on its own loop, as `resume_scope::spawn_reconciler` does for
//! the resume windows, and reads Mongo only.
//!
//! A round collects this pod's entries ([`Hub::agent_generations`], no guard
//! held past it), reads the live rows' `token_gen` on the primary in one
//! query with a projection, and ends each entry below its row's generation
//! through [`Hub::end_below_generation`]. So the disconnect line reads
//! `by=rotated`, the pod keeps the floor, and the sessions go as at a
//! commit. That call checks the entry again under its lock, so an entry
//! replaced since the round collected it is left alone. A removed or
//! quarantined row is not read: the removal's own paths answer it.
//!
//! Spawned only while the rotation is on ([`Hub::set_token_rotation`]): with
//! it off, no round runs.
//!
//! [`Hub::agent_generations`]: crate::hub::Hub::agent_generations
//! [`Hub::end_below_generation`]: crate::hub::Hub::end_below_generation
//! [`Hub::set_token_rotation`]: crate::hub::Hub::set_token_rotation

use std::collections::HashMap;
use std::time::Duration;

use bson::oid::ObjectId;
use tracing::{info, warn};

use crate::FleetState;

/// Start this pod's check, unless the rotation is off. The first round runs
/// one period after the start.
pub fn spawn(state: FleetState) {
    if !state.rc_hub.token_rotation() {
        return;
    }
    let every = Duration::from_secs(state.settings.rc.presence_sweep_secs.max(5));
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(every).await;
            let ended = run(&state).await;
            if ended > 0 {
                info!(
                    ended,
                    "agent generation check: entries below their row's generation ended"
                );
            }
        }
    });
}

/// One round: how many entries it ended. A failed read ends nothing, and
/// the next round reads again.
pub async fn run(state: &FleetState) -> usize {
    let held = state.rc_hub.agent_generations();
    if held.is_empty() {
        return 0;
    }
    let ids: Vec<ObjectId> = held.iter().map(|(agent_id, _, _)| *agent_id).collect();
    let rows: HashMap<ObjectId, (ObjectId, u32)> =
        match state.agents.live_generations_on_the_primary(&ids).await {
            Ok(rows) => rows
                .into_iter()
                .map(|(agent_id, tenant_id, generation)| (agent_id, (tenant_id, generation)))
                .collect(),
            Err(e) => {
                warn!(
                    %e, entries = ids.len(),
                    "agent generation check: rows unreadable; the next round reads again"
                );
                return 0;
            }
        };
    let mut ended = 0;
    for (agent_id, tenant_id, generation) in held {
        if let Some(&(row_tenant, row_generation)) = rows.get(&agent_id)
            && row_tenant == tenant_id
            && generation < row_generation
            && state
                .rc_hub
                .end_below_generation(agent_id, tenant_id, row_generation)
        {
            ended += 1;
        }
    }
    ended
}
