// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-69 P5a — the host's TRANSITIONAL hook implementations: what `network`
//! will register through `Module::hooks` once it is a module, implemented
//! over the host's own overlay code until then and registered under the
//! `network` id, so the fleet module's cascades already run in
//! [`roomler_core::hooks::HOOK_ORDER`] through the core registry.
//!
//! When `network` is extracted (P7) this file goes with it — the `impl`
//! moves, the registration becomes the module's own.

use async_trait::async_trait;
use bson::oid::ObjectId;
use roomler_ai_remote_control::models::NodeRef;
use roomler_core::hooks::{FleetLifecycle, ReleasedLease, RenamePropagation};

use crate::state::AppState;

/// The overlay lease + MagicDNS label holder, over the host's overlay code.
pub struct HostNetworkHooks {
    pub state: AppState,
}

#[async_trait]
impl FleetLifecycle for HostNetworkHooks {
    /// Release the overlay lease BEFORE the row delete and BEFORE the kick
    /// (the kick's WS teardown runs `handle_overlay_leave`, which must find an
    /// already-tombstoned node rather than race the release CAS).
    async fn agent_removed(
        &self,
        tenant_id: ObjectId,
        agent_id: ObjectId,
        machine_id: &str,
        reason: &str,
    ) -> anyhow::Result<Option<ReleasedLease>> {
        let released = crate::ws::overlay::release_overlay_node_for(
            &self.state,
            tenant_id,
            machine_id,
            &NodeRef::Agent { agent_id },
            reason,
        )
        .await;
        Ok(released.map(|r| ReleasedLease {
            overlay_ip: r.overlay_ip,
        }))
    }

    /// Best-effort propagation onto the live overlay node: peers see the new
    /// MagicDNS label immediately (delta re-fan); the device itself re-learns
    /// its self-name on its next reconnect.
    async fn agent_renamed(
        &self,
        tenant_id: ObjectId,
        agent_id: ObjectId,
        name: &str,
    ) -> anyhow::Result<RenamePropagation> {
        let Some(node) = self
            .state
            .overlay_nodes
            .find_live_by_agent(tenant_id, agent_id)
            .await?
        else {
            return Ok(RenamePropagation::NoLiveNode);
        };
        Ok(
            match crate::ws::overlay::propagate_node_rename(&self.state, &node, name).await {
                Some(label) => RenamePropagation::Propagated(label),
                None => RenamePropagation::Failed,
            },
        )
    }
}
