// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! The inverse edges: what ends an agent session besides its owner.
//!
//! A removed device, a removed member and an archived org each end every
//! live session they leave behind — on the record at once (a removal cannot
//! wait on a device), and with an `rc:hive.stop` to any device still
//! connected here. `hive` leads `HOOK_ORDER`: the cascade runs while the
//! device's row and socket still exist to address.
//!
//! A failing write stops the cascade — the contract every holder keeps, so a
//! removal never reports success with sessions still running under it. The
//! hooks are idempotent: a re-run finds nothing live and ends nothing.

use std::sync::Arc;

use async_trait::async_trait;
use bson::{Document, doc, oid::ObjectId};
use roomler_ai_remote_control::signaling::ServerMsg;
use roomler_core::{FleetLifecycle, ReleasedLease, TenantLifecycle, hooks::TenantArchived};
use tracing::info;

use crate::HiveState;

pub struct HiveHooks {
    state: HiveState,
}

impl HiveHooks {
    pub fn new(state: HiveState) -> Arc<Self> {
        Arc::new(Self { state })
    }
}

/// End every live session matching `filter`, telling each connected device.
async fn end_and_tell(
    state: &HiveState,
    filter: Document,
    reason: &'static str,
) -> anyhow::Result<u64> {
    let live = state.sessions.live_matching(filter.clone()).await?;
    for s in &live {
        let Some(sid) = s.id else { continue };
        let msg = ServerMsg::HiveStop {
            session_id: sid,
            fence: u64::try_from(s.fence).unwrap_or_default(),
            reason: reason.to_string(),
        };
        // Best effort: a device not connected here is told nothing now, and
        // its session ends on the record regardless — what it still runs is
        // cut off from the org by the removal itself.
        let _ = state
            .fleet
            .rc_hub
            .push_hive(s.location.device_id, s.tenant_id, msg);
    }
    let ended = state.sessions.end_all_matching(filter, reason).await?;
    // Say why in each session's room (P0d) — best-effort, after the record
    // is already ended.
    for s in live {
        let mut s = s;
        s.end_reason = Some(reason.to_string());
        s.detail = None;
        crate::room::note(state, &s, crate::room::ended_note(&s)).await;
    }
    Ok(ended)
}

#[async_trait]
impl FleetLifecycle for HiveHooks {
    async fn agent_removed(
        &self,
        tenant_id: ObjectId,
        agent_id: ObjectId,
        _machine_id: &str,
        _reason: &str,
    ) -> anyhow::Result<Option<ReleasedLease>> {
        let ended = end_and_tell(
            &self.state,
            doc! { "tenant_id": tenant_id, "location.device_id": agent_id },
            "device_removed",
        )
        .await?;
        if ended > 0 {
            info!(tenant = %tenant_id, device = %agent_id, ended, "hive: device removed — its sessions ended");
        }
        // Its viewers are told; the device itself is going, so not it.
        crate::view::end_grants(
            &self.state,
            |_, _, device| *device == agent_id,
            "device_removed",
            false,
        )
        .await;
        Ok(None)
    }
}

#[async_trait]
impl TenantLifecycle for HiveHooks {
    async fn tenant_archived(
        &self,
        tenant_id: ObjectId,
        _reason: &str,
    ) -> anyhow::Result<TenantArchived> {
        let ended = end_and_tell(
            &self.state,
            doc! { "tenant_id": tenant_id },
            "tenant_archived",
        )
        .await?;
        if ended > 0 {
            info!(tenant = %tenant_id, ended, "hive: organization archived — its sessions ended");
        }
        crate::view::end_grants(
            &self.state,
            |tenant, _, _| *tenant == tenant_id,
            "tenant_archived",
            true,
        )
        .await;
        Ok(TenantArchived::default())
    }

    /// A member who leaves or is removed stops driving agent sessions on the
    /// org's devices — at once, not when someone notices.
    async fn member_removed(&self, tenant_id: ObjectId, user_id: ObjectId) -> anyhow::Result<()> {
        let ended = end_and_tell(
            &self.state,
            doc! { "tenant_id": tenant_id, "owner_id": user_id },
            "member_removed",
        )
        .await?;
        if ended > 0 {
            info!(tenant = %tenant_id, user = %user_id, ended, "hive: member removed — their sessions ended");
        }
        // And stops READING them — theirs or anyone's — now, not at the next
        // renewal.
        crate::view::end_grants(
            &self.state,
            |tenant, user, _| *tenant == tenant_id && *user == user_id,
            "member_removed",
            true,
        )
        .await;
        Ok(())
    }
}
