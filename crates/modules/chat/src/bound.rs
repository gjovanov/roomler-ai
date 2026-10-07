// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-90 — what another module may do with chat, through chat: a room bound
//! to an object that module owns, and messages an agent writes into it.
//!
//! The first caller is `hive`: an agent session is a `Secret` room bound to
//! `{module: "hive", ref: <session id>}`, and each turn is a stub message the
//! session authors. Chat keeps its own invariants on this path exactly as on
//! the REST one — who may read the room, the fan-out to its members — and the
//! caller supplies only what it owns: the name, the binding, the content.
//!
//! [`BoundChat`] is STATELESS — the core plus two DAOs over `core.db` — so a
//! module built on chat re-creates it with [`BoundChat::new`] rather than
//! taking chat's state as its `Module::Deps` (FR-69 rule 5: only a live object
//! is a dependency; `remote`'s Hub is one, a DAO is not).
//!
//! None of this is reachable from a route: no user can bind a room or post as
//! an agent.

use std::collections::HashMap;
use std::sync::Arc;

use bson::oid::ObjectId;
use roomler_ai_db::models::{Binding, Message, Room};
use roomler_ai_services::dao::base::DaoResult;
use roomler_ai_services::dao::message::MessageDao;
use roomler_ai_services::dao::room::RoomDao;
use roomler_core::Core;

/// Chat's bound-room surface for another module.
#[derive(Clone)]
pub struct BoundChat {
    core: Core,
    rooms: Arc<RoomDao>,
    messages: Arc<MessageDao>,
}

impl BoundChat {
    /// Built from the core alone: nothing here outlives a call.
    pub fn new(core: &Core) -> Self {
        Self {
            rooms: Arc::new(RoomDao::new(&core.db)),
            messages: Arc::new(MessageDao::new(&core.db)),
            core: core.clone(),
        }
    }

    /// A `Secret` room bound to `binding`, with `owner` as its only member,
    /// at a `path` the caller guarantees unique.
    pub async fn create_bound_room(
        &self,
        tenant_id: ObjectId,
        name: String,
        path: String,
        owner: ObjectId,
        binding: Binding,
    ) -> DaoResult<Room> {
        self.rooms
            .create_bound(tenant_id, name, path, owner, binding)
            .await
    }

    /// Whether `user` is a member of `room_id` in `tenant_id` — chat's own
    /// rule, so a module deciding who may see what a room is about decides it
    /// exactly as the room's routes do.
    pub async fn is_member(
        &self,
        tenant_id: ObjectId,
        room_id: ObjectId,
        user: ObjectId,
    ) -> DaoResult<bool> {
        self.rooms.is_member(tenant_id, room_id, user).await
    }

    /// Post a message authored by an agent (`author_id`, shown as
    /// `author_display`) into `room_id`, and fan it out to the room's members
    /// as `message:create` — the event every client already renders.
    pub async fn post_agent_message(
        &self,
        tenant_id: ObjectId,
        room_id: ObjectId,
        author_id: ObjectId,
        author_display: String,
        binding: Binding,
        content: String,
    ) -> DaoResult<ObjectId> {
        let message = self
            .messages
            .create_agent(
                tenant_id,
                room_id,
                author_id,
                author_display,
                binding,
                content,
            )
            .await?;
        let id = message.id.expect("a stored message has an id");
        self.fan_out(room_id, "message:create", message).await;
        Ok(id)
    }

    /// Replace an agent message's content and fan out `message:update`.
    /// `false` = no such agent message in this tenant and room — a person's
    /// message is never touched by this path.
    pub async fn update_agent_message(
        &self,
        tenant_id: ObjectId,
        room_id: ObjectId,
        message_id: ObjectId,
        content: String,
    ) -> DaoResult<bool> {
        match self
            .messages
            .update_agent_content(tenant_id, room_id, message_id, content)
            .await?
        {
            Some(message) => {
                self.fan_out(room_id, "message:update", message).await;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    async fn fan_out(&self, room_id: ObjectId, kind: &str, message: Message) {
        let members = match self.rooms.find_member_user_ids(room_id).await {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(room = %room_id, %e, "chat: agent message not fanned out — members unreadable");
                return;
            }
        };
        let response = crate::message::to_response(message, &HashMap::new(), None);
        let event = serde_json::json!({ "type": kind, "data": &response });
        roomler_core::ws::dispatcher::broadcast_with_redis(
            &self.core.ws_storage,
            &self.core.redis_pubsub,
            &members,
            &event,
        )
        .await;
    }
}
