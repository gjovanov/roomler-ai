// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! Cross-cutting helpers the host's remaining route files share.
//!
//! FR-69 P3 — the notification helpers moved into `roomler_core::notify`
//! (re-exported here) and the message guard went with the `chat` module.
//! The two room guards below are the host's copy for the call and recording
//! handlers, which conference takes over in P4; the `chat` module keeps its
//! own in `roomler_ai_mod_chat::guards`.

use bson::oid::ObjectId;
use roomler_ai_db::models::Room;

use crate::error::ApiError;
use crate::state::AppState;

pub use roomler_core::notify::notify_call_started;

/// Tenant membership + the room resolved WITHIN that tenant, and nothing else.
///
/// This is [`require_room_in_tenant`] without the room-level visibility gate.
/// ⚠️ Do not reach for this to "skip the check": every room-scoped route wants
/// [`require_room_in_tenant`]; a second caller here would be a visibility
/// bypass wearing a helper's name.
pub async fn resolve_room_in_tenant(
    state: &AppState,
    tenant_id: ObjectId,
    room_id: ObjectId,
    user_id: ObjectId,
) -> Result<Room, ApiError> {
    if !state.tenants.is_member(tenant_id, user_id).await? {
        return Err(ApiError::Forbidden("Not a member".to_string()));
    }
    Ok(state
        .rooms
        .base
        .find_by_id_in_tenant(tenant_id, room_id)
        .await?)
}

/// Object-level authorization gate for room-scoped routes.
///
/// Returns the room ONLY if it belongs to `tenant_id` AND the caller is a
/// member of that tenant — resolving the room *within* the tenant is what
/// closes the cross-tenant IDOR (`is_member(tid)` alone is satisfied by any
/// tenant the caller belongs to). Then the room-level read gate: `Public`
/// short-circuits; a `Secret` room answers 404 rather than 403 so it does not
/// confirm its existence.
pub async fn require_room_in_tenant(
    state: &AppState,
    tenant_id: ObjectId,
    room_id: ObjectId,
    user_id: ObjectId,
) -> Result<Room, ApiError> {
    let room = resolve_room_in_tenant(state, tenant_id, room_id, user_id).await?;

    if room.visibility.requires_membership()
        && !state.rooms.is_member(tenant_id, room_id, user_id).await?
    {
        return Err(if room.visibility.hidden_from_non_members() {
            ApiError::NotFound("Resource not found".to_string())
        } else {
            ApiError::Forbidden("Not a member of this room".to_string())
        });
    }

    Ok(room)
}
