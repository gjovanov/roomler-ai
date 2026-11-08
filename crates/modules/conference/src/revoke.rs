// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! A call seat lasts only as long as the membership that admitted it.
//!
//! `media:join` admits a connection on what REST `call/join` checks: the
//! person is a member of the room's org, the room's visibility lets them in,
//! and they hold an open call session (`media_gate`). Those checks run once,
//! at the join. This module ends the seat when the membership under it ends:
//! the person is removed from the org or leaves it (the host's member route),
//! or leaves the room (`chat`'s). Core tells conference through
//! [`MembershipLifecycle`], after the membership is gone, so nothing can be
//! admitted on it again while the seats are withdrawn.
//!
//! What a membership's end does to the person's seats, wherever they are
//! served:
//!
//! - on the pod that holds them (the call's media owner, reached over the bus
//!   as [`WITHDRAW_SEATS`] when that is another pod), every connection of the
//!   person in that call's media closes: its transports, producers and
//!   consumers ([`withdraw_seats_here`]);
//! - each of those connections hears `room:call_ended` with `reason:
//!   "not_a_member"`, which the web client already reads as "this call is
//!   over for you": it tears its media down;
//! - everyone still in the call hears `media:peer_left` for each of them;
//! - the person leaves the call's participant set: their open call sessions
//!   close, the room's count drops, and a call they were the last one in
//!   ends.
//!
//! A `media:join` in flight when the membership ends is caught by the join
//! itself: it asks the gate again once its seat exists
//! (`ws_media::handle_media_join`), so a seat taken after the withdrawal
//! looked is closed there.

use std::sync::Arc;

use async_trait::async_trait;
use bson::oid::ObjectId;
use roomler_core::MembershipLifecycle;
use tracing::{info, warn};

use crate::ConferenceState;
use crate::media_cluster::{MediaRoute, resolve_media_route};

/// The `reason` of the `room:call_ended` a withdrawn seat hears.
pub(crate) const NOT_A_MEMBER: &str = "not_a_member";

/// The bus request that withdraws a person's seats on the pod holding them.
pub(crate) const WITHDRAW_SEATS: &str = "media.withdraw_seats";

/// Conference's [`MembershipLifecycle`]: the hook `Module::hooks` registers.
pub(crate) fn hooks(state: &ConferenceState) -> roomler_core::Hooks {
    roomler_core::Hooks {
        membership: Some(Arc::new(MembershipHooks {
            state: state.clone(),
        })),
        ..Default::default()
    }
}

struct MembershipHooks {
    state: ConferenceState,
}

#[async_trait]
impl MembershipLifecycle for MembershipHooks {
    async fn org_member_removed(
        &self,
        tenant_id: ObjectId,
        user_id: ObjectId,
    ) -> anyhow::Result<()> {
        org_membership_ended(&self.state, tenant_id, user_id).await
    }

    async fn room_member_removed(
        &self,
        tenant_id: ObjectId,
        room_id: ObjectId,
        user_id: ObjectId,
    ) -> anyhow::Result<()> {
        room_membership_ended(&self.state, tenant_id, room_id, user_id).await
    }
}

/// `user_id` left room `room_id`, or was removed from it: end their seats in
/// its call. Their membership row, and the call sessions it held, are gone
/// already, and the room's count with them (`RoomDao::leave`); what is left
/// is the media, and the call itself when they were the last one in it.
async fn room_membership_ended(
    state: &ConferenceState,
    tenant_id: ObjectId,
    room_id: ObjectId,
    user_id: ObjectId,
) -> anyhow::Result<()> {
    let closed = withdraw_seats(state, room_id, user_id).await;
    if closed == 0 {
        return Ok(());
    }
    info!(
        tenant = %tenant_id, room = %room_id, ?user_id, seats = closed,
        "call seats withdrawn: the room membership ended"
    );
    crate::call::settle_call_after_departure(state, room_id)
        .await
        .map_err(|e| anyhow::anyhow!("settling room {room_id}'s call: {e}"))
}

/// `user_id` is no longer a member of org `tenant_id`: end their seats in
/// every call of the org they may be in, and close their call sessions
/// there, as a hang-up would. One room's failure does not keep the others
/// from being reached; the first is returned.
async fn org_membership_ended(
    state: &ConferenceState,
    tenant_id: ObjectId,
    user_id: ObjectId,
) -> anyhow::Result<()> {
    // Each room with whether the person still holds an open call session in it.
    let mut rooms = state
        .rooms
        .call_rooms_of_member(tenant_id, user_id)
        .await
        .map_err(|e| anyhow::anyhow!("finding the calls to end: {e}"))?;
    // A seat on this pod whose call Mongo no longer shows as live (an island
    // that outlived its call) is media all the same.
    for rid in state.room_manager.rooms_with_user(&user_id) {
        if !rooms.iter().any(|(r, _)| *r == rid)
            && let Ok(room) = state.rooms.base.find_by_id(rid).await
            && room.tenant_id == tenant_id
        {
            rooms.push((rid, false));
        }
    }

    let mut first_error = None;
    for (rid, in_session) in rooms {
        let closed = withdraw_seats(state, rid, user_id).await;
        if closed > 0 {
            info!(
                tenant = %tenant_id, room = %rid, ?user_id, seats = closed,
                "call seats withdrawn: the org membership ended"
            );
        }
        // Their open call sessions in the room close as a hang-up closes
        // them: the count drops, and a call they were the last one in ends.
        if in_session
            && let Err(e) = crate::call::finalize_call_leave_db(state, rid, user_id, None).await
        {
            warn!(tenant = %tenant_id, room = %rid, ?user_id, %e, "closing the call sessions of an ended membership failed");
            first_error.get_or_insert_with(|| anyhow::anyhow!("room {rid}: {e}"));
        }
    }
    first_error.map_or(Ok(()), Err)
}

/// End `user_id`'s seats in `room_id`'s call wherever its media is served:
/// on this pod, or on the owner pod over the bus. Returns how many closed.
async fn withdraw_seats(state: &ConferenceState, room_id: ObjectId, user_id: ObjectId) -> usize {
    let owner = match resolve_media_route(state, &room_id, false).await {
        MediaRoute::Local { .. } => {
            return withdraw_seats_here(state, room_id, user_id).await.len();
        }
        MediaRoute::Remote(owner) => owner,
    };
    let Some(bus) = state.cluster_bus.clone() else {
        return 0;
    };
    let body = serde_json::json!({ "room_id": room_id.to_hex(), "user_id": user_id.to_hex() });
    match bus.request(&owner, WITHDRAW_SEATS, body).await {
        Ok(reply) => reply["closed"].as_array().map_or(0, Vec::len),
        Err(e) => {
            // The owner is gone (its media went with it), or the bus is down.
            warn!(room = %room_id, ?user_id, %owner, %e, "withdrawing call seats on the owner pod failed");
            0
        }
    }
}

/// End `user_id`'s seats in `room_id`'s media on this pod, the one serving
/// it. Each closed connection hears that its call ended; everyone left in the
/// call hears it leave. Returns the closed connections.
pub(crate) async fn withdraw_seats_here(
    state: &ConferenceState,
    room_id: ObjectId,
    user_id: ObjectId,
) -> Vec<String> {
    let closed = state
        .room_manager
        .close_participant_by_user(&room_id, &user_id);
    if closed.is_empty() {
        return closed;
    }
    let remaining = state.room_manager.connection_ids(&room_id);
    let ended = serde_json::json!({
        "type": "room:call_ended",
        "data": { "room_id": room_id.to_hex(), "reason": NOT_A_MEMBER }
    });
    for conn in &closed {
        roomler_core::ws::dispatcher::send_to_connection_routed(
            &state.ws_storage,
            &state.redis_pubsub,
            conn,
            &ended,
        )
        .await;
        let left = serde_json::json!({
            "type": "media:peer_left",
            "data": {
                "user_id": user_id.to_hex(),
                "connection_id": conn,
                "room_id": room_id.to_hex(),
            }
        });
        for other in &remaining {
            roomler_core::ws::dispatcher::send_to_connection_routed(
                &state.ws_storage,
                &state.redis_pubsub,
                other,
                &left,
            )
            .await;
        }
    }
    closed
}
