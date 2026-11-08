// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! A room membership lasts only as long as the org membership under it.
//!
//! Chat owns room membership (`room_members`), and every room fan-out (new
//! messages, edits, reactions, pins, typing, mentions) reaches the people who
//! hold a row there. So when someone stops being a member of an org (an
//! admin removes them, or they leave), their rows in that org's rooms go
//! too: [`MembershipLifecycle::org_member_removed`], run by the host's member
//! route after the org membership row is gone. In [`HOOK_ORDER`], chat comes
//! after `conference`, which reads the same rows to end the person's call
//! seats first.
//!
//! Rows that outlived an org membership before this hook existed are ended by
//! [`prune`], an operator step: `POST /api/admin/room-members/prune`.
//!
//! [`HOOK_ORDER`]: roomler_core::hooks::HOOK_ORDER

use std::sync::Arc;

use async_trait::async_trait;
use axum::{Json, extract::State};
use bson::{Document, doc, oid::ObjectId};
use roomler_core::{ApiError, MembershipLifecycle, extractors::auth::AuthUser};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::ChatState;

/// Chat's [`MembershipLifecycle`]: the hook `Module::hooks` registers.
pub(crate) fn hooks(state: &ChatState) -> roomler_core::Hooks {
    roomler_core::Hooks {
        membership: Some(Arc::new(MembershipHooks {
            state: state.clone(),
        })),
        ..Default::default()
    }
}

struct MembershipHooks {
    state: ChatState,
}

#[async_trait]
impl MembershipLifecycle for MembershipHooks {
    async fn org_member_removed(
        &self,
        tenant_id: ObjectId,
        user_id: ObjectId,
    ) -> anyhow::Result<()> {
        let ended = end_room_memberships(&self.state, tenant_id, user_id).await?;
        if ended > 0 {
            info!(
                tenant = %tenant_id, ?user_id, rooms = ended,
                "room memberships ended with the org membership"
            );
        }
        Ok(())
    }
}

/// Drop every room membership `user_id` holds in org `tenant_id`, one room at
/// a time through [`RoomDao::leave`](roomler_ai_services::dao::room::RoomDao::leave),
/// so each room's member count (and its participant count, for a call session
/// still open) drops as a leave's would. Idempotent: a second run finds no row.
/// Returns how many rows went.
pub(crate) async fn end_room_memberships(
    state: &ChatState,
    tenant_id: ObjectId,
    user_id: ObjectId,
) -> anyhow::Result<u64> {
    let rows = state
        .rooms
        .members
        .find_many(doc! { "tenant_id": tenant_id, "user_id": user_id }, None)
        .await?;
    let mut ended = 0;
    for row in rows {
        if state.rooms.leave(tenant_id, row.room_id, user_id).await? {
            ended += 1;
        }
    }
    Ok(ended)
}

/// What one prune call does by default, and at most.
const DEFAULT_PRUNE_LIMIT: u32 = 200;
const MAX_PRUNE_LIMIT: u32 = 2_000;

fn yes() -> bool {
    true
}

/// `POST /api/admin/room-members/prune`.
#[derive(Debug, Deserialize)]
pub struct PruneRequest {
    /// Count only, change nothing. On unless the body says otherwise.
    #[serde(default = "yes")]
    pub dry_run: bool,
    /// How many (org, person) pairs one call may end:
    /// [`DEFAULT_PRUNE_LIMIT`] by default, [`MAX_PRUNE_LIMIT`] at most.
    pub limit: Option<u32>,
}

#[derive(Debug, Serialize)]
pub struct PruneResponse {
    pub dry_run: bool,
    /// (org, person) pairs holding room rows without an org membership,
    /// up to the limit.
    pub pairs: u64,
    /// The room rows those pairs hold.
    pub rows: u64,
    /// Pairs whose memberships this call ended (always 0 in a dry run).
    pub ended: u64,
    /// Pairs the call could not end. Each is in the log, and a later call
    /// tries again.
    pub failed: u64,
    /// More pairs may be waiting past the limit: call again.
    pub more: bool,
}

/// End the room memberships that outlived an org membership: the rows a
/// removal left before the membership hook existed, or that a removal whose
/// hook cascade failed left behind.
///
/// Platform operators only (404 for anyone else, like every `/api/admin`
/// route). A dry run unless `dry_run: false`, bounded by `limit` pairs per
/// call, and idempotent: each pair goes through the same cascade as a live
/// removal ([`HookRegistry::org_member_removed`]: call seats first, then the
/// rows), re-checked first, so a person who is a member again by now is left
/// alone, and a second call finds nothing left.
///
/// [`HookRegistry::org_member_removed`]: roomler_core::HookRegistry::org_member_removed
pub async fn prune(
    State(state): State<ChatState>,
    auth: AuthUser,
    Json(body): Json<PruneRequest>,
) -> Result<Json<PruneResponse>, ApiError> {
    roomler_core::guards::require_platform_admin(&state, &auth)?;
    let limit = body
        .limit
        .unwrap_or(DEFAULT_PRUNE_LIMIT)
        .clamp(1, MAX_PRUNE_LIMIT) as usize;

    let mut pairs = stale_pairs(&state, limit + 1).await?;
    let more = pairs.len() > limit;
    pairs.truncate(limit);
    let rows = pairs.iter().map(|p| p.rows).sum();

    let (mut ended, mut failed) = (0, 0);
    if !body.dry_run {
        for pair in &pairs {
            if state
                .tenants
                .is_member(pair.tenant_id, pair.user_id)
                .await?
            {
                continue;
            }
            match state
                .hooks
                .org_member_removed(pair.tenant_id, pair.user_id)
                .await
            {
                Ok(()) => ended += 1,
                Err(e) => {
                    failed += 1;
                    warn!(
                        tenant = %pair.tenant_id, user = %pair.user_id, %e,
                        "room-member prune: ending a membership failed"
                    );
                }
            }
        }
    }
    info!(
        operator = %auth.user_id, dry_run = body.dry_run, pairs = pairs.len(), rows, ended, failed, more,
        "room-member prune"
    );
    Ok(Json(PruneResponse {
        dry_run: body.dry_run,
        pairs: pairs.len() as u64,
        rows,
        ended,
        failed,
        more,
    }))
}

/// One (org, person) pair with room rows and no org membership.
struct StalePair {
    tenant_id: ObjectId,
    user_id: ObjectId,
    rows: u64,
}

/// Up to `limit` pairs, in a stable order: the room rows grouped by (org,
/// person), each looked up in `tenant_members` by its unique
/// `(tenant_id, user_id)` index, keeping the pairs with no match. Rows of
/// people with no account (`user_id` null, a guest) are not people this
/// applies to.
async fn stale_pairs(state: &ChatState, limit: usize) -> Result<Vec<StalePair>, ApiError> {
    use futures::TryStreamExt;

    let pipeline = vec![
        doc! { "$match": { "user_id": { "$type": "objectId" } } },
        doc! { "$group": {
            "_id": { "tenant_id": "$tenant_id", "user_id": "$user_id" },
            "rows": { "$sum": 1 },
        } },
        doc! { "$lookup": {
            "from": "tenant_members",
            "let": { "t": "$_id.tenant_id", "u": "$_id.user_id" },
            "pipeline": [
                { "$match": { "$expr": { "$and": [
                    { "$eq": ["$tenant_id", "$$t"] },
                    { "$eq": ["$user_id", "$$u"] },
                ] } } },
                { "$limit": 1 },
                { "$project": { "_id": 1 } },
            ],
            "as": "membership",
        } },
        doc! { "$match": { "membership": { "$size": 0 } } },
        doc! { "$sort": { "_id.tenant_id": 1, "_id.user_id": 1 } },
        doc! { "$limit": limit as i64 },
    ];
    let mut cursor = state
        .rooms
        .members
        .collection()
        .clone_with_type::<Document>()
        .aggregate(pipeline)
        .allow_disk_use(true)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let mut out = Vec::new();
    while let Some(group) = cursor
        .try_next()
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
    {
        let Ok(key) = group.get_document("_id") else {
            continue;
        };
        let (Ok(tenant_id), Ok(user_id)) =
            (key.get_object_id("tenant_id"), key.get_object_id("user_id"))
        else {
            continue;
        };
        let rows = group
            .get_i32("rows")
            .map(|n| n.max(0) as u64)
            .or_else(|_| group.get_i64("rows").map(|n| n.max(0) as u64))
            .unwrap_or(0);
        out.push(StalePair {
            tenant_id,
            user_id,
            rows,
        });
    }
    Ok(out)
}
