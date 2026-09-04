// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! Object-level authorisation for the call and recording routes.
//!
//! The room guard is chat's (`roomler_ai_mod_chat::guards`): rooms are the
//! container a call runs in, and the visibility rules are one set. This is the
//! thin binding to [`ConferenceState`], so the handlers read as before.

use bson::oid::ObjectId;
use roomler_ai_db::models::Room;
use roomler_core::ApiError;

use crate::ConferenceState;

/// Tenant membership + the room resolved WITHIN that tenant + the room-level
/// read gate. See `roomler_ai_mod_chat::guards::require_room_in_tenant` for
/// the invariant and why a `Secret` room answers 404.
pub async fn require_room_in_tenant(
    state: &ConferenceState,
    tenant_id: ObjectId,
    room_id: ObjectId,
    user_id: ObjectId,
) -> Result<Room, ApiError> {
    roomler_ai_mod_chat::guards::require_room_in_tenant_with(
        &state.tenants,
        &state.rooms,
        tenant_id,
        room_id,
        user_id,
    )
    .await
}
