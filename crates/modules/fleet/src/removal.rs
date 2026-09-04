// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! The ONE device-removal sequence behind the admin `DELETE …/agent/{id}`
//! route, the ephemeral self-unenroll and the ephemeral reaper — factored so
//! they cannot drift (FR-51 F3).
//!
//! FR-69 P5a — moved from the host's `ws/ephemeral.rs`. The overlay release
//! that opened the sequence is `network`'s, which fleet cannot call (the
//! module graph is a DAG); it now runs through the core hook registry
//! ([`roomler_core::hooks::FleetLifecycle::agent_removed`]), in
//! [`roomler_core::hooks::HOOK_ORDER`] — session holders, then lease holders,
//! then this, the record owner. Same order as before, one owner per step.

use bson::oid::ObjectId;
use roomler_ai_remote_control::models::Agent;
use roomler_ai_services::dao::base::{DaoError, DaoResult};
use roomler_core::hooks::ReleasedLease;

use crate::FleetState;

/// Remove ONE device from the fleet: holders release (overlay lease first) →
/// row delete → hub kick, in that order.
///
/// The ORDER is inherited from `delete_agent` and is load-bearing: the
/// overlay lease is released BEFORE the row delete and BEFORE the kick,
/// because the kick's WS teardown runs `handle_overlay_leave`, which must
/// find an already-tombstoned node rather than race the release CAS with a
/// second `removes` fan.
///
/// The row delete is chosen by the ROW's own nature, not by the caller: an
/// ephemeral row is hard-deleted (its tombstone would reserve a random
/// machine_id forever — FR-51 F4), a permanent row is tombstoned exactly as
/// before. Returns what the lease holders freed, for the route's response.
pub async fn remove_agent_device(
    state: &FleetState,
    agent: &Agent,
    reason: &str,
) -> DaoResult<Option<ReleasedLease>> {
    let aid = agent
        .id
        .ok_or_else(|| DaoError::Validation("agent missing _id".into()))?;

    // Holders first (remote: sessions; network: the overlay lease). A holder
    // that fails stops the cascade — deleting the row while a lease is still
    // held is the state this order exists to prevent.
    let released = state
        .hooks
        .agent_removed(agent.tenant_id, aid, &agent.machine_id, reason)
        .await
        .map_err(|e| DaoError::Validation(format!("removal hook failed: {e}")))?;

    if agent.ephemeral {
        state
            .agents
            .hard_delete_ephemeral(agent.tenant_id, aid)
            .await?;
        // The use row is the trail that survives the hard delete; stamping it
        // is best-effort (a missing stamp loses a detail, not the removal).
        match state
            .enrollment_keys
            .record_removal(agent.tenant_id, aid, reason)
            .await
        {
            Ok(_) => {}
            Err(e) => {
                tracing::debug!(agent_id = %aid, %e, "ephemeral removal stamp failed")
            }
        }
    } else {
        state.agents.soft_delete(agent.tenant_id, aid).await?;
    }

    // Kick the socket last — locally, and on whichever pod holds it.
    state.rc_hub.unregister_agent(aid, None);
    crate::ctrl::publish_rc_ctrl(
        &state.core,
        "kick",
        serde_json::json!({ "agent_id": aid.to_hex() }),
    )
    .await;

    Ok(released)
}

/// The agent id the reaper and the routes log; kept next to the sequence so
/// its callers do not each re-derive it.
pub fn agent_id_of(agent: &Agent) -> Option<ObjectId> {
    agent.id
}
