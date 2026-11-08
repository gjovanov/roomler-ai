// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! The `media:*` namespace on the user socket: join / transports / produce /
//! consume / pause / close / leave / play_audio / stop_audio — and the socket
//! close, which is a leave the client never sent.
//!
//! FR-69 P4 — moved from the host's `ws/handler.rs` unchanged below the
//! [`Media`] handler: [`route_and_dispatch_media`] is the C-4 placement gate
//! (local room → serve here; foreign claim → forward over the bus), and
//! [`dispatch_media_local`] is shared with the owner-side `media.cmd` bus
//! handler in `media_cluster.rs`, which is why it must never re-enter the
//! gate. Replies are connection-addressed and routed, so an error raised
//! while executing a forwarded command still reaches the viewer's pod.
//!
//! Every command passes the participation gate (`media_gate`) first: a
//! `media:join` needs what REST `call/join` checks — the room's tenant, the
//! room's visibility, and an open call session for this connection — and
//! every other command needs this connection's seat in that room's media.
//! The handlers below take the gate's proof ([`JoinPass`] or [`Seat`]), never
//! a bare room id, user or connection, so none can be reached around it.
//! [`route_and_dispatch_media`] admits a join BEFORE placement, so a refused
//! join claims no room; the owner admits a forwarded join again before it
//! seats it (`media_cluster::handle_media_cmd_rpc`), and
//! [`dispatch_media_local`] checks every other command's seat where it lives.

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use bson::oid::ObjectId;
use hmac::{Hmac, Mac};
use mediasoup::prelude::*;
use roomler_core::{WsCtx, WsHandler};
use sha1::Sha1;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{debug, info, warn};

use crate::ConferenceState;
use crate::media_gate::{self, JoinPass, Seat};

/// The one command a connection may send before it holds a seat.
const JOIN: &str = "media:join";

/// The namespace handler the module registers for `Role::User` / `media`.
pub struct Media {
    pub state: ConferenceState,
}

#[async_trait]
impl WsHandler for Media {
    async fn handle(&self, ctx: &WsCtx, msg: serde_json::Value) -> anyhow::Result<()> {
        let msg_type = msg.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let data = msg.get("data");
        route_and_dispatch_media(
            &self.state,
            &ctx.principal,
            &ctx.connection_id,
            msg_type,
            data,
        )
        .await;
        Ok(())
    }

    async fn closed(&self, ctx: &WsCtx) {
        on_closed(&self.state, &ctx.principal, &ctx.connection_id).await;
    }
}

/// The socket closed: drop this connection's media — its local island, or
/// the transports a remote owner holds for it — and close its call session
/// in Mongo. Historically the disconnect path cleaned only media, and a
/// refresh mid-call left Mongo at `in_progress`/count>=1 forever ("1 Active
/// call" until a manual re-join + hangup); the Mongo lookup is the fallback
/// for a session whose `media:join` never happened (HTTP call/join only).
pub async fn on_closed(state: &ConferenceState, user_id: &ObjectId, connection_id: &str) {
    // Capture the call room BEFORE the media maps are consumed below
    // (`forward_close_leave` removes the remote_media_conns entry) — the
    // DB half of the leave still needs it.
    let remote_media_rid = state
        .remote_media_conns
        .get(connection_id)
        .map(|e| *e.value());
    let local_media_rid = state.room_manager.get_connection_room(connection_id);

    // C-4 — the conn may have joined a room OWNED BY ANOTHER POD: tell
    // the owner to drop its transports (best-effort; no-op when the map
    // has no entry).
    crate::media_cluster::forward_close_leave(state, user_id, connection_id).await;

    if let Some(room_id) = local_media_rid {
        let remaining_conns = state
            .room_manager
            .get_other_connection_ids(&room_id, connection_id);

        state
            .room_manager
            .close_participant(&room_id, connection_id);

        if !remaining_conns.is_empty() {
            let event = serde_json::json!({
                "type": "media:peer_left",
                "data": {
                    "user_id": user_id.to_hex(),
                    "connection_id": connection_id,
                    "room_id": room_id.to_hex(),
                }
            });
            for conn_id in &remaining_conns {
                roomler_core::ws::dispatcher::send_to_connection_routed(
                    &state.ws_storage,
                    &state.redis_pubsub,
                    conn_id,
                    &event,
                )
                .await;
            }
        }
    }

    // DB half of the leave: close this connection's call session, broadcast
    // the new count, auto-end when it was the last one.
    let db_rid = match local_media_rid.or(remote_media_rid) {
        Some(rid) => Some(rid),
        None => state
            .rooms
            .find_call_room_for_connection(*user_id, connection_id)
            .await
            .ok()
            .flatten(),
    };
    if let Some(rid) = db_rid
        && let Err(e) =
            crate::call::finalize_call_leave_db(state, rid, *user_id, Some(connection_id)).await
    {
        warn!(?user_id, %connection_id, room = %rid, %e, "disconnect call-leave DB cleanup failed");
    }
}

async fn route_and_dispatch_media(
    state: &ConferenceState,
    user_id: &ObjectId,
    connection_id: &str,
    msg_type: &str,
    data: Option<&serde_json::Value>,
) {
    use crate::media_cluster::{MediaRoute, forward_media_cmd, resolve_media_route};

    let (rid, frame) = match requested_room(data) {
        Ok(requested) => requested,
        Err(why) => {
            refuse(state, user_id, connection_id, msg_type, why).await;
            return;
        }
    };
    if msg_type == JOIN {
        // Admitted BEFORE placement: a refused join claims no room, builds no
        // router and reaches no other pod.
        let pass = match media_gate::admit_join(state, *user_id, connection_id, rid).await {
            Ok(pass) => pass,
            Err(refusal) => {
                refuse_join(state, user_id, connection_id, rid, &refusal).await;
                return;
            }
        };
        match resolve_media_route(state, &rid, true).await {
            MediaRoute::Local { create } => {
                // A conn whose room came home (fold/rehome) no longer needs
                // close-time forwarding to a remote owner.
                state.remote_media_conns.remove(connection_id);
                handle_media_join(state, &pass, frame, create).await;
            }
            MediaRoute::Remote(owner) => {
                // The owner admits the join again before it seats the
                // connection (`handle_media_cmd_rpc`): the pod that creates
                // the participant decides, whichever pod forwarded the join.
                let ok =
                    forward_media_cmd(state, &owner, user_id, connection_id, msg_type, data).await;
                // Track remote membership so WS close can tell the owner to
                // drop the transports. (The prune fallback may have served
                // the join LOCALLY after all — skip the marker then.)
                if ok && !state.room_manager.has_room(&rid) {
                    state
                        .remote_media_conns
                        .insert(connection_id.to_string(), rid);
                }
            }
        }
        return;
    }
    match resolve_media_route(state, &rid, false).await {
        MediaRoute::Local { .. } => {
            // A conn whose room came home (fold/rehome) no longer needs
            // close-time forwarding to a remote owner.
            state.remote_media_conns.remove(connection_id);
            dispatch_media_local(state, user_id, connection_id, msg_type, data, false).await;
        }
        MediaRoute::Remote(owner) => {
            // The seat lives on the owner, which checks it there.
            forward_media_cmd(state, &owner, user_id, connection_id, msg_type, data).await;
            if msg_type == "media:leave" {
                state.remote_media_conns.remove(connection_id);
            }
        }
    }
}

/// Execute one media command against the LOCAL room manager. Shared by
/// the WS dispatch above and the owner-side `media.cmd` bus handler
/// (`ws/media_cluster.rs`) — which is why it must never re-enter the
/// placement gate.
///
/// The participation gate runs here for every caller, on the pod that
/// executes: a join (the prune fallback's re-served one) is admitted before
/// the connection is seated, and every other command needs the connection's
/// seat in this pod's room, so a command forwarded to the owner is checked
/// where the seats live, whichever pod forwarded it.
pub async fn dispatch_media_local(
    state: &ConferenceState,
    user_id: &ObjectId,
    connection_id: &str,
    msg_type: &str,
    data: Option<&serde_json::Value>,
    create_allowed: bool,
) {
    let (rid, frame) = match requested_room(data) {
        Ok(requested) => requested,
        Err(why) => {
            refuse(state, user_id, connection_id, msg_type, why).await;
            return;
        }
    };
    if msg_type == JOIN {
        match media_gate::admit_join(state, *user_id, connection_id, rid).await {
            Ok(pass) => handle_media_join(state, &pass, frame, create_allowed).await,
            Err(refusal) => refuse_join(state, user_id, connection_id, rid, &refusal).await,
        }
        return;
    }
    let Some(seat) = media_gate::seat(state, *user_id, connection_id, rid) else {
        // No seat in this room's media on this pod: the connection never
        // joined it, or has left, or the room closed or moved meanwhile.
        refuse(
            state,
            user_id,
            connection_id,
            msg_type,
            media_gate::NOT_A_PARTICIPANT,
        )
        .await;
        return;
    };
    dispatch_seated(state, &seat, msg_type, frame).await;
}

/// One command of a connection that holds a seat. Nothing here has a bare
/// user, connection or room: each handler acts on the [`Seat`].
async fn dispatch_seated(
    state: &ConferenceState,
    seat: &Seat,
    msg_type: &str,
    data: &serde_json::Value,
) {
    match msg_type {
        "media:connect_transport" => {
            handle_media_connect_transport(state, seat, data).await;
        }
        "media:produce" => {
            handle_media_produce(state, seat, data).await;
        }
        "media:consume" => {
            handle_media_consume(state, seat, data).await;
        }
        "media:producer_close" => {
            handle_media_producer_close(state, seat, data).await;
        }
        "media:producer_pause" => {
            handle_media_producer_pause(state, seat, data).await;
        }
        "media:view" => {
            handle_media_view(state, seat, data).await;
        }
        "media:leave" => {
            handle_media_leave(state, seat).await;
        }
        "media:play_audio" => {
            handle_play_audio(state, seat, data).await;
        }
        "media:stop_audio" => {
            handle_stop_audio(state, seat, data).await;
        }
        _ => {
            debug!(user_id = ?seat.user_id(), msg_type, "Unknown media message type");
        }
    }
}

/// The room a frame names, which the gate checks and every handler acts on,
/// with the frame's data.
fn requested_room(
    data: Option<&serde_json::Value>,
) -> Result<(ObjectId, &serde_json::Value), &'static str> {
    let data = data.ok_or("Missing room_id")?;
    let raw = data
        .get("room_id")
        .and_then(|v| v.as_str())
        .ok_or("Missing room_id")?;
    let rid = ObjectId::parse_str(raw).map_err(|_| "Invalid room_id")?;
    Ok((rid, data))
}

/// Refuse a command. Only the commands that already answered their own
/// failures — join, produce, consume — say why; the rest stay silent as they
/// always were (a `media:leave` after the room closed, or a view report that
/// races a fold, wants no answer).
async fn refuse(
    state: &ConferenceState,
    user_id: &ObjectId,
    connection_id: &str,
    msg_type: &str,
    why: &str,
) {
    debug!(?user_id, %connection_id, msg_type, why, "media command not run");
    if matches!(msg_type, JOIN | "media:produce" | "media:consume") {
        send_media_error(state, connection_id, why).await;
    }
}

/// A join the gate refused: logged with its reason, answered without one.
pub(crate) async fn refuse_join(
    state: &ConferenceState,
    user_id: &ObjectId,
    connection_id: &str,
    rid: ObjectId,
    refusal: &media_gate::Refusal,
) {
    info!(?user_id, %connection_id, room = %rid, %refusal, "media:join refused");
    send_media_error(state, connection_id, media_gate::NOT_A_PARTICIPANT).await;
}

async fn send_media_error(state: &ConferenceState, connection_id: &str, message: &str) {
    let msg = serde_json::json!({
        "type": "media:error",
        "data": { "message": message }
    });
    // Conn-addressed + routed: an error raised while executing a
    // forwarded command must reach the viewer's pod.
    roomler_core::ws::dispatcher::send_to_connection_routed(
        &state.ws_storage,
        &state.redis_pubsub,
        connection_id,
        &msg,
    )
    .await;
}

/// Run a join the gate admitted, on the pod that serves the room: seat the
/// connection and hand it the transports, the TURN credential and the
/// producers already in the call.
pub(crate) async fn handle_media_join(
    state: &ConferenceState,
    pass: &JoinPass,
    data: &serde_json::Value,
    create_allowed: bool,
) {
    let (rid, user_id, connection_id) = (pass.room_id(), pass.user_id(), pass.connection_id());

    let mut room_exists = state.room_manager.has_room(&rid);
    debug!(?user_id, %connection_id, ?rid, room_exists, create_allowed, "media:join room check");
    if !room_exists && create_allowed {
        // C-4 — materialization is claim-gated: `create_allowed` comes
        // from the placement resolver (NX claim won, or the belt
        // fallback with a live conference while the directory is down).
        // The Mongo in_progress check lives in the resolver too.
        match state.room_manager.create_room(rid).await {
            Ok(_) => {
                info!(?rid, "media:join materialized conference room on this pod");
                room_exists = true;
            }
            Err(e) => {
                warn!(?rid, %e, "media:join room create failed");
            }
        }
    }
    if !room_exists {
        send_media_error(state, connection_id, "Room does not exist").await;
        return;
    }

    // FR-32 P1b — plan `video_max_participants`. Counted from the pod-local
    // room registry rather than Mongo, because that is where a live call
    // actually exists; the tenant-affinity LB co-locates a tenant's conference
    // on ONE pod, so the local count is the whole call. ⚠ Distinct USERS, not
    // connections: one person on laptop and phone is one participant.
    //
    // A rejoining participant is already in the map, so `>=` would refuse them
    // their own seat — hence the membership test before the count.
    //
    // FR-115 P4b — the org that pays for this meeting's TURN relay, named in
    // the TURN username below so coturn's statistics name it: the room's own
    // tenant, which the gate read from the room.
    let room_org = pass.tenant_id();
    if let Ok(tenant) = state.tenants.base.find_by_id(room_org).await {
        let present = state.room_manager.get_participant_user_ids(&rid);
        // FR-102 P4 — through core, which also counts the collision per org.
        if !present.contains(&user_id)
            && let Err(d) = roomler_core::quota::check(
                state,
                &tenant,
                roomler_ai_services::quota::Limit::VideoMaxParticipants,
                present.len() as u64,
            )
        {
            send_media_error(state, connection_id, &d.message()).await;
            return;
        }
    }

    // FR-127 P1 — `caps: ["view"]` declares a client that will report what it
    // renders (`media:view`): its video consumers start paused and resume when
    // a report names them, audio always flows. Absent → a legacy client, every
    // consumer resumed as before. The flag rides the join so the server knows
    // the client's generation before its first `media:consume`, with no
    // ordering to get wrong. ⚠️ Read only while the lever is on: with
    // `[meetings] visible_tiles = false` the server keeps today's shape
    // whatever the client says, and tells it so in `meeting.view` below.
    let view_capable = state.settings.meetings.visible_tiles
        && data
            .get("caps")
            .and_then(|c| c.as_array())
            .is_some_and(|caps| caps.iter().any(|c| c.as_str() == Some("view")));

    let transport_pair = match state
        .room_manager
        .create_transports(rid, user_id, connection_id.to_string(), view_capable)
        .await
    {
        Ok(tp) => tp,
        Err(e) => {
            send_media_error(
                state,
                connection_id,
                &format!("Failed to create transports: {}", e),
            )
            .await;
            return;
        }
    };

    // The gate again, now that the seat exists. A membership that ended
    // while this join was on its way withdrew the seats it could find
    // (`revoke`), and this one did not exist yet; asking again here closes
    // that gap: the seat is either seen by the withdrawal, or sees the
    // membership gone and goes itself, before the client hears anything.
    if let Err(refusal) = media_gate::admit_join(state, user_id, connection_id, rid).await {
        state
            .room_manager
            .close_participant_of_user(&rid, connection_id, &user_id);
        refuse_join(state, &user_id, connection_id, rid, &refusal).await;
        return;
    }

    // FR-127 P1.1 — read the capabilities and release the rooms guard BEFORE
    // the send: a guard held across an await parks every thread that wants its
    // shard (`room_manager.rs` `tests`).
    let caps = state
        .room_manager
        .rooms_ref()
        .get(&rid)
        .map(|room| serde_json::to_value(room.router.rtp_capabilities()).unwrap_or_default());
    if let Some(caps) = caps {
        let msg = serde_json::json!({
            "type": "media:router_capabilities",
            "data": { "rtp_capabilities": caps }
        });
        roomler_core::ws::dispatcher::send_to_connection_routed(
            &state.ws_storage,
            &state.redis_pubsub,
            connection_id,
            &msg,
        )
        .await;
    }

    // ⚠️ `Some("")` is "configured" to the type system and "not configured" to
    // everyone else. Treat a blank URL as absent, or we advertise an ICE server
    // the browser refuses outright and no one can join a call at all.
    let turn_url_configured = state
        .settings
        .turn
        .url
        .as_deref()
        .map(str::trim)
        .filter(|u| !u.is_empty());
    // FR-115 P4b — the TURN user, charged to the room's org
    // (`<expiry>:<user>:org-<org>`), so coturn's statistics name the org that
    // pays; coturn reads only the expiry part.
    let turn_user =
        roomler_ai_remote_control::turn_creds::charged(&user_id.to_hex(), &room_org.to_hex());
    let ice_servers: Vec<serde_json::Value> = if let Some(url) = turn_url_configured {
        let (turn_username, turn_credential) =
            if let Some(ref secret) = state.settings.turn.shared_secret {
                let expiry = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs()
                    + 86400;
                let username = format!("{}:{}", expiry, turn_user);
                let mut mac = Hmac::<Sha1>::new_from_slice(secret.as_bytes())
                    .expect("HMAC key length is valid");
                mac.update(username.as_bytes());
                let credential = BASE64.encode(mac.finalize().into_bytes());
                debug!(%username, "Generated TURN ephemeral credentials");
                (username, credential)
            } else {
                (
                    state
                        .settings
                        .turn
                        .username
                        .as_deref()
                        .unwrap_or("")
                        .to_string(),
                    state
                        .settings
                        .turn
                        .password
                        .as_deref()
                        .unwrap_or("")
                        .to_string(),
                )
            };
        // Build TURN URLs with multiple transport variants.
        // UDP TURN often fails behind NAT/firewalls, so include TCP and TLS
        // fallbacks. Also emit `turn:HOST:443?transport=udp` because many
        // corporate firewalls allow UDP/443 (QUIC) but block UDP/3478.
        let urls = roomler_ai_remote_control::turn_url::expand_turn_url(
            url,
            &roomler_ai_remote_control::turn_url::VariantCaps::media(),
        );
        vec![serde_json::json!({
            "urls": urls,
            "username": turn_username,
            "credential": turn_credential,
        })]
    } else {
        vec![]
    };

    let force_relay = state.settings.turn.force_relay.unwrap_or(false);

    if force_relay {
        info!("force_relay=true — clients will use iceTransportPolicy='relay' via TURN server");
    }

    info!(
        %connection_id,
        force_relay,
        view_capable,
        // The RESOLVED per-pod announced IP (S6 map), not the static setting —
        // on a multi-pod deployment they differ and the static one misleads.
        announced_ip = %state
            .room_manager
            .announced_ip()
            .unwrap_or(state.settings.mediasoup.announced_ip.as_str()),
        turn_url = ?state.settings.turn.url,
        send_ice_candidates = %transport_pair.send_transport.ice_candidates,
        recv_ice_candidates = %transport_pair.recv_transport.ice_candidates,
        "media:join transport_created ICE diagnostics"
    );

    let msg = serde_json::json!({
        "type": "media:transport_created",
        "data": {
            "send_transport": transport_pair.send_transport,
            "recv_transport": transport_pair.recv_transport,
            "ice_servers": ice_servers,
            "force_relay": force_relay,
            // FR-127 P1 — which levers this server runs, so a behaviour the
            // CLIENT carries (Opus DTX now, simulcast in P2) still has a
            // server-side switch. An older client ignores the field.
            "meeting": meeting_levers(&state.settings.meetings),
        }
    });
    roomler_core::ws::dispatcher::send_to_connection_routed(
        &state.ws_storage,
        &state.redis_pubsub,
        connection_id,
        &msg,
    )
    .await;

    let producers = state.room_manager.get_producer_ids(&rid, connection_id);
    for (uid, conn_id, pid, kind, source) in producers {
        let msg = serde_json::json!({
            "type": "media:new_producer",
            "data": {
                "producer_id": pid.to_string(),
                "user_id": uid.to_hex(),
                "connection_id": conn_id,
                "kind": match kind { MediaKind::Audio => "audio", MediaKind::Video => "video" },
                "source": source,
            }
        });
        roomler_core::ws::dispatcher::send_to_connection_routed(
            &state.ws_storage,
            &state.redis_pubsub,
            connection_id,
            &msg,
        )
        .await;
    }
}

async fn handle_media_connect_transport(
    state: &ConferenceState,
    seat: &Seat,
    data: &serde_json::Value,
) {
    let (rid, connection_id) = (seat.room_id(), seat.connection_id());
    let transport_id = match data.get("transport_id").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => return,
    };
    let dtls_parameters: DtlsParameters = match data
        .get("dtls_parameters")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
    {
        Some(p) => p,
        None => return,
    };

    if let Err(e) = state
        .room_manager
        .connect_transport(&rid, connection_id, transport_id, dtls_parameters)
        .await
    {
        warn!(%connection_id, %e, "connect_transport failed");
    }
}

async fn handle_media_produce(state: &ConferenceState, seat: &Seat, data: &serde_json::Value) {
    let (rid, user_id, connection_id) = (seat.room_id(), seat.user_id(), seat.connection_id());
    let kind: MediaKind = match data
        .get("kind")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
    {
        Some(k) => k,
        None => {
            send_media_error(state, connection_id, "Invalid kind").await;
            return;
        }
    };
    let rtp_parameters: RtpParameters = match data
        .get("rtp_parameters")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
    {
        Some(p) => p,
        None => {
            send_media_error(state, connection_id, "Invalid rtp_parameters").await;
            return;
        }
    };
    let source = data
        .get("source")
        .and_then(|v| v.as_str())
        .unwrap_or(match kind {
            MediaKind::Audio => "audio",
            MediaKind::Video => "camera",
        })
        .to_string();

    match state
        .room_manager
        .produce(&rid, connection_id, kind, rtp_parameters, source.clone())
        .await
    {
        Ok(producer_id) => {
            let result_msg = serde_json::json!({
                "type": "media:produce_result",
                "data": { "id": producer_id.to_string() }
            });
            roomler_core::ws::dispatcher::send_to_connection_routed(
                &state.ws_storage,
                &state.redis_pubsub,
                connection_id,
                &result_msg,
            )
            .await;

            let other_conns = state
                .room_manager
                .get_other_connection_ids(&rid, connection_id);

            if !other_conns.is_empty() {
                let event = serde_json::json!({
                    "type": "media:new_producer",
                    "data": {
                        "producer_id": producer_id.to_string(),
                        "user_id": user_id.to_hex(),
                        "connection_id": connection_id,
                        "kind": match kind { MediaKind::Audio => "audio", MediaKind::Video => "video" },
                        "source": source,
                    }
                });
                for conn_id in &other_conns {
                    roomler_core::ws::dispatcher::send_to_connection_routed(
                        &state.ws_storage,
                        &state.redis_pubsub,
                        conn_id,
                        &event,
                    )
                    .await;
                }
            }
        }
        Err(e) => {
            send_media_error(state, connection_id, &format!("produce failed: {}", e)).await;
        }
    }
}

async fn handle_media_consume(state: &ConferenceState, seat: &Seat, data: &serde_json::Value) {
    let (rid, connection_id) = (seat.room_id(), seat.connection_id());
    let producer_id_str = match data.get("producer_id").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => {
            send_media_error(state, connection_id, "Missing producer_id").await;
            return;
        }
    };
    let rtp_capabilities: RtpCapabilities = match data
        .get("rtp_capabilities")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
    {
        Some(c) => c,
        None => {
            send_media_error(state, connection_id, "Invalid rtp_capabilities").await;
            return;
        }
    };

    let producer_id = match producer_id_str.parse::<ProducerId>() {
        Ok(id) => id,
        Err(_) => {
            send_media_error(state, connection_id, "Invalid producer_id").await;
            return;
        }
    };

    match state
        .room_manager
        .consume(&rid, connection_id, producer_id, &rtp_capabilities)
        .await
    {
        Ok(consumer_info) => {
            let msg = serde_json::json!({
                "type": "media:consumer_created",
                "data": {
                    "id": consumer_info.id,
                    "producer_id": consumer_info.producer_id,
                    "kind": consumer_info.kind,
                    "rtp_parameters": consumer_info.rtp_parameters,
                    // FR-30 P4 — the pause EVENT only reaches whoever was
                    // already in the room, so the state has to ride the
                    // subscription too. Without it, joining after someone
                    // muted shows no badge until their next toggle.
                    "producer_paused": consumer_info.producer_paused,
                }
            });
            roomler_core::ws::dispatcher::send_to_connection_routed(
                &state.ws_storage,
                &state.redis_pubsub,
                connection_id,
                &msg,
            )
            .await;
        }
        Err(e) => {
            send_media_error(state, connection_id, &format!("consume failed: {}", e)).await;
        }
    }
}

/// FR-127 P1 — the `meeting` object of `media:transport_created`: the levers
/// this server runs. `view` — the server pauses what a view client does not
/// show, so the client reports its view; `audio_dtx` — the client produces
/// its microphone with Opus DTX; `simulcast` — `null` until P2 hands the
/// client a layer ladder here. A server switch turned off turns the client's
/// half off too, with no client release.
pub fn meeting_levers(meetings: &roomler_ai_config::MeetingsSettings) -> serde_json::Value {
    serde_json::json!({
        "view": meetings.visible_tiles,
        "audio_dtx": meetings.audio_dtx,
        "simulcast": serde_json::Value::Null,
    })
}

/// FR-127 P1 — a viewer's report of what it renders: `{ room_id, seq,
/// video: [{ producer_id, w, h }] }`, the WHOLE set, not a delta. The video
/// consumers it names are resumed, the rest paused; audio is never touched
/// (`media/view.rs`). No reply: the report is idempotent and the next one
/// supersedes it, so there is nothing for the client to wait for.
///
/// Dispatched where `media:consume` is — through the placement gate, so a
/// viewer whose socket is on another pod reaches the owner's
/// `ParticipantMedia` over `media.cmd` like every other command.
async fn handle_media_view(state: &ConferenceState, seat: &Seat, data: &serde_json::Value) {
    let (rid, user_id, connection_id) = (seat.room_id(), seat.user_id(), seat.connection_id());
    // The kill switch: the server never joined this client as view-capable
    // (`view = None`), so its report would be ignored anyway — say so once
    // at debug and skip the parse.
    if !state.settings.meetings.visible_tiles {
        debug!(?user_id, %connection_id, "media:view ignored: visible_tiles is off");
        return;
    }
    let seq = data.get("seq").and_then(|v| v.as_u64()).unwrap_or(0);
    // A tile whose producer id does not parse is skipped, not an error: the
    // producer may have closed between the client's render and this report.
    let tiles: Vec<(ProducerId, (u32, u32))> = data
        .get("video")
        .and_then(|v| v.as_array())
        .map(|tiles| {
            tiles
                .iter()
                .filter_map(|t| {
                    let pid = t.get("producer_id")?.as_str()?.parse::<ProducerId>().ok()?;
                    let dim = |key: &str| {
                        t.get(key)
                            .and_then(|v| v.as_u64())
                            .and_then(|v| u32::try_from(v).ok())
                            .unwrap_or(0)
                    };
                    Some((pid, (dim("w"), dim("h"))))
                })
                .collect()
        })
        .unwrap_or_default();

    let outcome = state
        .room_manager
        .apply_view(
            &rid,
            connection_id,
            seq,
            tiles,
            state.settings.meetings.view_max,
        )
        .await;
    debug!(?user_id, %connection_id, ?rid, seq, ?outcome, "media:view");
}

/// FR-30 — relay "my camera/mic just went off (or back on)" to the room.
///
/// Without this a peer cannot tell: `producer.pause()` in mediasoup-client is
/// LOCAL, and a track with `enabled = false` keeps its RTP stream alive with
/// black frames, so the receiving track stays `live` and UNMUTED. Measured on
/// prod 2026-08-29 — the far side saw `enabled:true, readyState:"live",
/// muted:false` after the sender switched their camera off, which is why
/// "hide participants without video" could never hide anyone.
///
/// Shaped exactly like `handle_media_producer_close` below, and additive: an
/// older client never sends this and ignores the event it would receive.
async fn handle_media_producer_pause(
    state: &ConferenceState,
    seat: &Seat,
    data: &serde_json::Value,
) {
    let (rid, user_id, connection_id) = (seat.room_id(), seat.user_id(), seat.connection_id());
    let Some(producer_id_str) = data.get("producer_id").and_then(|v| v.as_str()) else {
        return;
    };
    // Absent means "pause": the only caller that omits it is a client that
    // predates the resume half, and pausing is the safe reading.
    let paused = data.get("paused").and_then(|v| v.as_bool()).unwrap_or(true);

    let Ok(producer_id) = producer_id_str.parse::<ProducerId>() else {
        return;
    };

    if !state
        .room_manager
        .set_producer_paused(&rid, connection_id, &producer_id, paused)
        .await
    {
        // The producer is not ours, or is already gone. Say nothing: fanning
        // out an unverified claim would let one participant blank another's
        // tile for everybody.
        return;
    }

    let other_conns = state
        .room_manager
        .get_other_connection_ids(&rid, connection_id);
    if other_conns.is_empty() {
        return;
    }

    let event = serde_json::json!({
        "type": "media:producer_paused",
        "data": {
            "producer_id": producer_id.to_string(),
            "user_id": user_id.to_hex(),
            "paused": paused,
        }
    });
    for conn_id in &other_conns {
        roomler_core::ws::dispatcher::send_to_connection_routed(
            &state.ws_storage,
            &state.redis_pubsub,
            conn_id,
            &event,
        )
        .await;
    }
}

async fn handle_media_producer_close(
    state: &ConferenceState,
    seat: &Seat,
    data: &serde_json::Value,
) {
    let (rid, user_id, connection_id) = (seat.room_id(), seat.user_id(), seat.connection_id());
    let producer_id_str = match data.get("producer_id").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => return,
    };

    let producer_id = match producer_id_str.parse::<ProducerId>() {
        Ok(id) => id,
        Err(_) => return,
    };

    if state
        .room_manager
        .close_producer(&rid, connection_id, &producer_id)
    {
        state
            .room_manager
            .remove_rtp_tap(&rid, &producer_id.to_string());

        let other_conns = state
            .room_manager
            .get_other_connection_ids(&rid, connection_id);

        if !other_conns.is_empty() {
            let event = serde_json::json!({
                "type": "media:producer_closed",
                "data": {
                    "producer_id": producer_id.to_string(),
                    "user_id": user_id.to_hex(),
                }
            });
            for conn_id in &other_conns {
                roomler_core::ws::dispatcher::send_to_connection_routed(
                    &state.ws_storage,
                    &state.redis_pubsub,
                    conn_id,
                    &event,
                )
                .await;
            }
        }
    }
}

/// The seat's own leave: only a connection seated in the room can leave it,
/// so a leave can no longer announce a `media:peer_left` for someone who was
/// never there, nor drop the connection's mapping to the room it IS in.
async fn handle_media_leave(state: &ConferenceState, seat: &Seat) {
    let (rid, user_id, connection_id) = (seat.room_id(), seat.user_id(), seat.connection_id());

    let other_conns = state
        .room_manager
        .get_other_connection_ids(&rid, connection_id);

    state.room_manager.close_participant(&rid, connection_id);

    if !other_conns.is_empty() {
        let event = serde_json::json!({
            "type": "media:peer_left",
            "data": {
                "user_id": user_id.to_hex(),
                "connection_id": connection_id,
                "room_id": rid.to_hex(),
            }
        });
        for conn_id in &other_conns {
            roomler_core::ws::dispatcher::send_to_connection_routed(
                &state.ws_storage,
                &state.redis_pubsub,
                conn_id,
                &event,
            )
            .await;
        }
    }
}

/// Play one of the call's room files to everyone in the call. The file must
/// be live, in the room's tenant, and one of THIS room's files — the list the
/// call's files panel shows (`GET …/room/{room_id}/file`) — so a participant
/// cannot announce a file of a room they may not read, or a removed one.
async fn handle_play_audio(state: &ConferenceState, seat: &Seat, data: &serde_json::Value) {
    let (rid, connection_id) = (seat.room_id(), seat.connection_id());
    let fid = match data
        .get("file_id")
        .and_then(|v| v.as_str())
        .and_then(|s| ObjectId::parse_str(s).ok())
    {
        Some(id) => id,
        None => return,
    };

    // The room's tenant. The seat proves this connection is in the room's
    // call, so the id is one the caller may use; the read only learns whose
    // room it is.
    let room = match state.rooms.base.find_by_id(rid).await {
        Ok(r) => r,
        Err(e) => {
            warn!(%e, "Failed to find room for file playback");
            return;
        }
    };

    // FR-69 P3 — `files` is the `chat` module's; a DAO is a stateless handle
    // on the collection, so this playback path builds its own.
    let files = roomler_ai_services::dao::file::FileDao::new(&state.db);
    let file = match files
        .base
        .find_live_by_id_in_tenant(room.tenant_id, fid)
        .await
    {
        Ok(f) if f.context.room_id == Some(rid) => f,
        Ok(_) => {
            debug!(room = %rid, file_id = %fid, "playback refused: not one of this room's files");
            return;
        }
        Err(e) => {
            debug!(room = %rid, file_id = %fid, %e, "playback refused: no live file");
            return;
        }
    };

    // Each playback has its own id, so a stop names the playback it ends.
    let playback_id = ObjectId::new().to_hex();

    let file_url = format!(
        "/api/tenant/{}/file/{}/download",
        room.tenant_id.to_hex(),
        fid.to_hex(),
    );
    let msg = serde_json::json!({
        "type": "media:audio_playback",
        "data": {
            "action": "start",
            "file_url": file_url,
            "file_id": fid.to_hex(),
            "filename": file.filename,
            "playback_id": playback_id,
            "room_id": rid.to_hex(),
        }
    });

    roomler_core::ws::dispatcher::send_to_connection_routed(
        &state.ws_storage,
        &state.redis_pubsub,
        connection_id,
        &msg,
    )
    .await;
    let other_conns = state
        .room_manager
        .get_other_connection_ids(&rid, connection_id);
    for cid in &other_conns {
        roomler_core::ws::dispatcher::send_to_connection_routed(
            &state.ws_storage,
            &state.redis_pubsub,
            cid,
            &msg,
        )
        .await;
    }

    info!(%rid, file_id = %fid, %playback_id, "Audio playback started");
}

/// Stop a playback for everyone in the call; only a participant may.
async fn handle_stop_audio(state: &ConferenceState, seat: &Seat, data: &serde_json::Value) {
    let (rid, connection_id) = (seat.room_id(), seat.connection_id());
    let playback_id = match data.get("playback_id").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => return,
    };

    let msg = serde_json::json!({
        "type": "media:audio_playback",
        "data": {
            "action": "stop",
            "playback_id": playback_id,
            "room_id": rid.to_hex(),
        }
    });

    roomler_core::ws::dispatcher::send_to_connection_routed(
        &state.ws_storage,
        &state.redis_pubsub,
        connection_id,
        &msg,
    )
    .await;
    let other_conns = state
        .room_manager
        .get_other_connection_ids(&rid, connection_id);
    for cid in &other_conns {
        roomler_core::ws::dispatcher::send_to_connection_routed(
            &state.ws_storage,
            &state.redis_pubsub,
            cid,
            &msg,
        )
        .await;
    }

    info!(%rid, %playback_id, "Audio playback stopped");
}
