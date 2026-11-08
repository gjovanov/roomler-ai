// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! Object-level authorisation for room-, message- and file-keyed routes.
//!
//! FR-69 P3 — moved from the api crate's `routes/helpers.rs` unchanged. P4 —
//! the room guards gained `_with` forms over the two DAOs, so `conference`
//! (which owns its own `RoomDao` handle for call state) shares ONE visibility
//! rule instead of a copy; the `ChatState` forms below bind them.
//!
//! One rule for every id a request names: the object is resolved within the
//! path tenant, it is live, and its room is one the caller may read. There is
//! one guard per id kind, and every handler keyed by that kind goes through
//! it:
//!
//! | id | guard |
//! |---|---|
//! | room | [`require_room_in_tenant`] |
//! | message | [`require_message_in_room`]: the path room, then the message in it |
//! | file | [`require_file`]: the file, then its own room |
//!
//! A listing that spans rooms (the org's files, search) keeps to
//! [`room_reach`], the same rule over every room of the org at once.

use std::collections::HashSet;

use bson::{Document, doc, oid::ObjectId};
use roomler_ai_db::models::{File, Message, Room, RoomVisibility};
use roomler_ai_services::dao::{room::RoomDao, tenant::TenantDao};
use roomler_core::ApiError;

use crate::ChatState;

/// The answer for an object that is not there, or that the caller may not
/// learn is there: the same bytes whatever the reason.
fn not_found() -> ApiError {
    ApiError::NotFound("Resource not found".to_string())
}

/// [`resolve_room_in_tenant`] over the two DAOs it needs, for a caller whose
/// state is not [`ChatState`].
pub async fn resolve_room_in_tenant_with(
    tenants: &TenantDao,
    rooms: &RoomDao,
    tenant_id: ObjectId,
    room_id: ObjectId,
    user_id: ObjectId,
) -> Result<Room, ApiError> {
    if !tenants.is_member(tenant_id, user_id).await? {
        return Err(ApiError::NotAMember);
    }
    Ok(rooms.base.find_by_id_in_tenant(tenant_id, room_id).await?)
}

/// Room-level read authorization for a room already resolved in the tenant.
///
/// Tenant membership answers "may you be here at all"; this answers "may you
/// be in THIS room" — the question that previously had no answer, so every
/// member could read every room while the sidebar drew a padlock on most of
/// them.
///
/// `Public` (the default, and what every pre-existing room reads back as)
/// short-circuits, so this costs no query for the overwhelming majority of
/// requests.
async fn room_visibility_gate(
    rooms: &RoomDao,
    tenant_id: ObjectId,
    room_id: ObjectId,
    room: &Room,
    user_id: ObjectId,
) -> Result<(), ApiError> {
    if room.visibility.requires_membership()
        && !rooms.is_member(tenant_id, room_id, user_id).await?
    {
        // NOT FOUND, not FORBIDDEN, for a `Secret` room: 403 would confirm it
        // exists to someone who is not supposed to know that, which is the
        // whole point of Secret. `Private` is listed anyway, so its existence
        // is not a secret and a 403 is the more useful answer.
        return Err(if room.visibility.hidden_from_non_members() {
            not_found()
        } else {
            ApiError::Forbidden("Not a member of this room".to_string())
        });
    }
    Ok(())
}

/// [`require_room_in_tenant`] over the two DAOs it needs — the one room
/// visibility rule, shared with `conference` and the host's PDF export.
pub async fn require_room_in_tenant_with(
    tenants: &TenantDao,
    rooms: &RoomDao,
    tenant_id: ObjectId,
    room_id: ObjectId,
    user_id: ObjectId,
) -> Result<Room, ApiError> {
    let room = resolve_room_in_tenant_with(tenants, rooms, tenant_id, room_id, user_id).await?;
    room_visibility_gate(rooms, tenant_id, room_id, &room, user_id).await?;
    Ok(room)
}

/// Tenant membership + the room resolved WITHIN that tenant, and nothing else.
///
/// This is [`require_room_in_tenant`] without the room-level visibility gate,
/// and it exists for exactly one caller: `join`. Joining is how someone
/// BECOMES a member, so routing it through a check that requires membership
/// would make Private rooms unjoinable — a room you can see, cannot read, and
/// cannot ask to enter.
///
/// ⚠️ Do not reach for this to "skip the check" anywhere else. Every other
/// room-scoped route wants [`require_room_in_tenant`]; a second caller here
/// would be a visibility bypass wearing a helper's name.
pub async fn resolve_room_in_tenant(
    state: &ChatState,
    tenant_id: ObjectId,
    room_id: ObjectId,
    user_id: ObjectId,
) -> Result<Room, ApiError> {
    resolve_room_in_tenant_with(&state.tenants, &state.rooms, tenant_id, room_id, user_id).await
}

/// Object-level authorization gate for room-scoped collaboration routes.
///
/// Returns the room ONLY if it belongs to `tenant_id`, the caller is a member
/// of that tenant, and the room's visibility lets the caller read it.
/// `is_member(tid)` alone is satisfied by any tenant the caller belongs to (a
/// user can create their own tenant for free), so it does NOT stop reading or
/// mutating ANOTHER tenant's room by id. Resolving the room *within* the
/// tenant (`{_id, tenant_id}`) does: a foreign room resolves to nothing → 404,
/// leaking neither its content nor its existence.
pub async fn require_room_in_tenant(
    state: &ChatState,
    tenant_id: ObjectId,
    room_id: ObjectId,
    user_id: ObjectId,
) -> Result<Room, ApiError> {
    require_room_in_tenant_with(&state.tenants, &state.rooms, tenant_id, room_id, user_id).await
}

/// The message guard: every handler keyed by a `message_id` goes through it.
///
/// The path names a room and a message in it, and both must hold: the room
/// passes [`require_room_in_tenant`] (tenant, room, visibility), and the
/// message is a LIVE message of that same room. A message of another room or
/// tenant, a deleted one and an id that names nothing all answer the same 404.
/// So the room a handler fans out to is the path room, which is the message's
/// own.
pub async fn require_message_in_room(
    state: &ChatState,
    tenant_id: ObjectId,
    room_id: ObjectId,
    message_id: ObjectId,
    user_id: ObjectId,
) -> Result<(Room, Message), ApiError> {
    let room = require_room_in_tenant(state, tenant_id, room_id, user_id).await?;
    let message = state
        .messages
        .find_live_in_room(tenant_id, room_id, message_id)
        .await?;
    Ok((room, message))
}

/// The file guard: every handler keyed by a `file_id` goes through it.
///
/// The file is a LIVE file of the path tenant, and its own room (a file's
/// room is where it was uploaded) passes the same visibility gate as the
/// room routes. A file whose room is not a room of the tenant answers 404,
/// as a missing file does. A file outside any room is the org's, for every
/// member.
pub async fn require_file(
    state: &ChatState,
    tenant_id: ObjectId,
    file_id: ObjectId,
    user_id: ObjectId,
) -> Result<File, ApiError> {
    if !state.tenants.is_member(tenant_id, user_id).await? {
        return Err(ApiError::NotAMember);
    }
    let file = state
        .files
        .base
        .find_live_by_id_in_tenant(tenant_id, file_id)
        .await?;
    if let Some(room_id) = file.context.room_id {
        let room = state
            .rooms
            .base
            .find_by_id_in_tenant(tenant_id, room_id)
            .await?;
        room_visibility_gate(&state.rooms, tenant_id, room_id, &room, user_id).await?;
    }
    Ok(file)
}

/// The rooms of one tenant as one caller reaches them: the rule of
/// [`require_room_in_tenant`] over every room of the tenant at once, for a
/// listing that spans rooms and must leave out the ones the caller may not
/// read. Rooms in any state count, as the single-room guard resolves them.
pub struct RoomReach {
    /// Rooms whose content the caller may read: every Public room, and every
    /// Private or Secret room the caller is a member of.
    pub readable: Vec<ObjectId>,
    /// Rooms the caller may see listed: the readable ones and the Private
    /// ones, which the room list shows to every member so they can ask to be
    /// let in. A Secret room is listed to its members only.
    pub listed: Vec<ObjectId>,
}

/// [`RoomReach`] for `user_id` in `tenant_id`. The caller has already checked
/// tenant membership. Two reads: the tenant's rooms (id and visibility only)
/// and the caller's memberships in them.
pub async fn room_reach(
    state: &ChatState,
    tenant_id: ObjectId,
    user_id: ObjectId,
) -> Result<RoomReach, ApiError> {
    use futures::TryStreamExt;

    let mut member_of = HashSet::new();
    let mut memberships = state
        .rooms
        .members
        .collection()
        .clone_with_type::<Document>()
        .find(doc! { "tenant_id": tenant_id, "user_id": user_id })
        .projection(doc! { "room_id": 1, "_id": 0 })
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    while let Some(row) = memberships
        .try_next()
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
    {
        if let Ok(room_id) = row.get_object_id("room_id") {
            member_of.insert(room_id);
        }
    }

    let mut reach = RoomReach {
        readable: Vec::new(),
        listed: Vec::new(),
    };
    let mut rooms = state
        .rooms
        .base
        .collection()
        .clone_with_type::<Document>()
        .find(doc! { "tenant_id": tenant_id })
        .projection(doc! { "_id": 1, "visibility": 1 })
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    while let Some(row) = rooms
        .try_next()
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
    {
        let Ok(room_id) = row.get_object_id("_id") else {
            continue;
        };
        // Absent reads back as Public, as the room model's default does.
        let visibility: RoomVisibility = row
            .get("visibility")
            .and_then(|v| bson::from_bson(v.clone()).ok())
            .unwrap_or_default();
        let member = member_of.contains(&room_id);
        if !visibility.requires_membership() || member {
            reach.readable.push(room_id);
            reach.listed.push(room_id);
        } else if !visibility.hidden_from_non_members() {
            reach.listed.push(room_id);
        }
    }
    Ok(reach)
}
