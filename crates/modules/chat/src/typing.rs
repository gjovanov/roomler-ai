// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! The typing indicator — chat's namespace on the user socket.
//!
//! `typing:start` / `typing:stop` carry a `room_id`; the event is fanned out
//! to the room's other members, cross-pod. FR-69 P3 — this used to be an arm
//! of the host's socket dispatch; it is the first handler a module registers
//! through [`roomler_core::Module::ws`], and the behaviour is unchanged.

use async_trait::async_trait;
use bson::oid::ObjectId;
use roomler_core::{WsCtx, WsHandler, ws::dispatcher::broadcast_with_redis};

use crate::ChatState;

pub struct Typing {
    pub state: ChatState,
}

#[async_trait]
impl WsHandler for Typing {
    async fn handle(&self, ctx: &WsCtx, msg: serde_json::Value) -> anyhow::Result<()> {
        let msg_type = msg.get("type").and_then(|t| t.as_str()).unwrap_or("");
        if msg_type != "typing:start" && msg_type != "typing:stop" {
            return Ok(());
        }
        let Some(room_id_str) = msg
            .get("data")
            .and_then(|d| d.get("room_id"))
            .and_then(|c| c.as_str())
        else {
            return Ok(());
        };
        let Ok(rid) = ObjectId::parse_str(room_id_str) else {
            return Ok(());
        };
        // The sender must be someone who may read the room: a member of the
        // room's own tenant (the frame names no tenant, so the room's is the
        // one to check) whom its visibility lets in. Anything else is dropped
        // as silently as a bad frame.
        let Ok(room) = self.state.rooms.base.find_by_id(rid).await else {
            return Ok(());
        };
        if crate::guards::require_room_in_tenant(&self.state, room.tenant_id, rid, ctx.principal)
            .await
            .is_err()
        {
            return Ok(());
        }
        // A lookup failure is not an error to the socket: the original arm
        // dropped the event silently, and so does this.
        let Ok(member_ids) = self.state.rooms.find_member_user_ids(rid).await else {
            return Ok(());
        };
        let recipients: Vec<ObjectId> = member_ids
            .into_iter()
            .filter(|id| *id != ctx.principal)
            .collect();
        let event = serde_json::json!({
            "type": msg_type,
            "data": {
                "room_id": room_id_str,
                "user_id": ctx.principal.to_hex(),
            }
        });
        broadcast_with_redis(
            &self.state.ws_storage,
            &self.state.redis_pubsub,
            &recipients,
            &event,
        )
        .await;
        Ok(())
    }
}
