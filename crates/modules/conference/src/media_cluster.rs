// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! C-4 — media claim-or-route: mediasoup rooms become cluster-safe.
//!
//! A conference room is server-materialized state with no client-owned
//! socket, so ownership is a **SET-NX claim** (`roomler:own:media:<room>`)
//! — creation itself is mutually exclusive, which kills the S6 belt's
//! split-brain class (two pods silently building rival router islands
//! for one room). Every `media:*` command resolves placement first — a
//! `media:join` only once the participation gate (`media_gate`) admitted it,
//! so a refused join never claims a room:
//!
//! - local room → serve with the local `RoomManager` (unchanged path);
//! - foreign claim → forward the command to the owning pod over the
//!   per-pod bus (`media.cmd`); replies and pushes come back
//!   **connection-addressed** on the global channel (`conn` envelope
//!   field) since the viewer's WS never moves;
//! - no claim → only `media:join` may materialize the room, gated on
//!   Mongo `in_progress` + winning the NX claim;
//! - directory unavailable → the S6 belt verbatim (get-or-create), with
//!   `MEDIA_BELT_FALLBACK_TOTAL` (core metrics) counting the accepted split risk.
//!
//! Claim lifetime: 30 s TTL, 10 s refresh (crash gap ≤30 s; graceful
//! shutdown compare-DELs for a zero-gap deploy handoff). A refresh
//! CONFLICT means a foreign pod owns the key (post-Redis-outage double
//! claim): the **claim-loser folds** — tears down its island and pushes
//! `media:room_closed {reason:"rehomed"}` so participants rejoin via
//! the normal join path, which lands on the surviving owner.

use std::sync::atomic::Ordering;

use bson::oid::ObjectId;
use tracing::{debug, info, warn};

use crate::ConferenceState;
use roomler_core::cluster::bus::BusError;
use roomler_core::cluster::directory::{ClaimOutcome, MEDIA_TTL_SECS, OwnerRecord, media_key};

/// Claim refresh cadence (TTL/3, same ratio as the 90/30 registries).
pub const MEDIA_REFRESH_SECS: u64 = 10;

/// Where a media command executes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaRoute {
    /// Serve with the local room manager. `create` = the caller may
    /// materialize the room (claim won, or belt fallback with a live
    /// conference in Mongo).
    Local { create: bool },
    /// The named pod owns the room — forward over the bus.
    Remote(String),
}

/// Resolve placement for one room. `is_join` gates materialization —
/// only the join path (WS `media:join` / HTTP `call/start`) may create.
pub async fn resolve_media_route(
    state: &ConferenceState,
    rid: &ObjectId,
    is_join: bool,
) -> MediaRoute {
    match try_resolve_media_route(state, rid, is_join).await {
        Ok(route) => route,
        Err(e) => {
            warn!(room = %rid, %e, "media directory read failed");
            belt_fallback(state, rid, is_join).await
        }
    }
}

/// [`resolve_media_route`], with a directory that cannot be read answered as
/// the error it is instead of the belt: a close must tell "no pod serves this
/// room" from "could not tell which pod does" (`close_room_everywhere`).
async fn try_resolve_media_route(
    state: &ConferenceState,
    rid: &ObjectId,
    is_join: bool,
) -> Result<MediaRoute, String> {
    if state.room_manager.has_room(rid) {
        return Ok(MediaRoute::Local { create: false });
    }
    let Some(dir) = state.cluster_directory.clone() else {
        return Ok(belt_fallback(state, rid, is_join).await);
    };
    let key = media_key(&rid.to_hex());
    Ok(match dir.get(&key).await.map_err(|e| e.to_string())? {
        Some(raw) => {
            if !dir.is_foreign(&raw) {
                // Ours (this pod + epoch) with no local room: trust it only
                // while we still hold the token — otherwise a lazy release
                // is racing us and the claim must be re-won from scratch.
                if state
                    .media_claim_tokens
                    .get(rid)
                    .map(|t| *t == raw)
                    .unwrap_or(false)
                {
                    return Ok(MediaRoute::Local { create: is_join });
                }
                return Ok(claim_missing(state, &dir, rid, is_join).await);
            }
            match OwnerRecord::parse(&raw) {
                Some(rec) if rec.pod_id == state.pod.pod_id => {
                    // Our pod's PREVIOUS process (stale epoch, e.g. after a
                    // crash-restart): prune the corpse and re-claim.
                    let _ = dir.release(&key, &raw).await;
                    claim_missing(state, &dir, rid, is_join).await
                }
                Some(rec) => MediaRoute::Remote(rec.pod_id),
                None => {
                    let _ = dir.release(&key, &raw).await;
                    claim_missing(state, &dir, rid, is_join).await
                }
            }
        }
        None => claim_missing(state, &dir, rid, is_join).await,
    })
}

/// No claim exists: joins may materialize (Mongo-gated + NX-claimed);
/// everything else serves locally where the handler produces the natural
/// "room does not exist" outcome.
async fn claim_missing(
    state: &ConferenceState,
    dir: &roomler_core::cluster::directory::OwnershipDirectory,
    rid: &ObjectId,
    is_join: bool,
) -> MediaRoute {
    if !is_join {
        return MediaRoute::Local { create: false };
    }
    if !call_in_progress(state, rid).await {
        return MediaRoute::Local { create: false };
    }
    let token = dir.owner_token("media");
    match dir
        .claim_nx(&media_key(&rid.to_hex()), &token, MEDIA_TTL_SECS)
        .await
    {
        Ok(ClaimOutcome::Won) => {
            state.media_claim_tokens.insert(*rid, token);
            info!(room = %rid, "media claim won — materializing room on this pod");
            MediaRoute::Local { create: true }
        }
        Ok(ClaimOutcome::Foreign(raw)) => match OwnerRecord::parse(&raw) {
            Some(rec) if rec.pod_id != state.pod.pod_id => MediaRoute::Remote(rec.pod_id),
            // Our own stale record won the race window — rare; the next
            // attempt (UI retry / rejoin) prunes it via resolve.
            _ => MediaRoute::Local { create: false },
        },
        Err(e) => {
            warn!(room = %rid, %e, "media claim_nx failed");
            belt_fallback(state, rid, true).await
        }
    }
}

async fn call_in_progress(state: &ConferenceState, rid: &ObjectId) -> bool {
    state
        .rooms
        .base
        .find_by_id(*rid)
        .await
        .map(|r| r.conference_status.as_deref() == Some("in_progress"))
        .unwrap_or(false)
}

/// Directory unavailable — the S6 belt verbatim (Mongo-gated local
/// get-or-create), counted: each entry accepted split-brain risk for
/// the duration of the Redis outage.
async fn belt_fallback(state: &ConferenceState, rid: &ObjectId, is_join: bool) -> MediaRoute {
    if !is_join {
        return MediaRoute::Local { create: false };
    }
    let create = call_in_progress(state, rid).await;
    if create {
        let total = roomler_core::cluster::metrics::MEDIA_BELT_FALLBACK_TOTAL
            .fetch_add(1, Ordering::Relaxed)
            + 1;
        warn!(
            room = %rid,
            media_belt_fallback_total = total,
            "media belt fallback — directory unavailable, serving get-or-create locally"
        );
    }
    MediaRoute::Local { create }
}

/// Forward one viewer command to the owning pod. Sends the viewer a
/// `media:error` on terminal failure. Returns whether the owner acked.
pub async fn forward_media_cmd(
    state: &ConferenceState,
    owner_pod: &str,
    user_id: &ObjectId,
    connection_id: &str,
    msg_type: &str,
    data: Option<&serde_json::Value>,
) -> bool {
    let Some(bus) = state.cluster_bus.clone() else {
        media_error(
            state,
            connection_id,
            "Conference is hosted on another server",
        )
        .await;
        return false;
    };
    let body = serde_json::json!({
        "user_id": user_id.to_hex(),
        "connection_id": connection_id,
        "msg_type": msg_type,
        "data": data,
    });
    // Router/transport-creating ops get the long deadline.
    let deadline = if msg_type == "media:join" {
        std::time::Duration::from_secs(5)
    } else {
        roomler_core::cluster::bus::RPC_DEADLINE
    };
    // FR-127 P1 — a forwarded `media:view` is BEST-EFFORT. The report is
    // idempotent and the next one (a tile change, the client's periodic
    // re-send) heals a lost one, so a bus hiccup must neither tell the viewer
    // the conference broke nor prune a LIVE owner's claim — which the
    // deadline branch below does for every other command, and which a viewer
    // scrolling on the non-owner pod would otherwise trip on every report.
    let best_effort = msg_type == "media:view";
    match bus
        .request_with_deadline(owner_pod, "media.cmd", body.clone(), deadline)
        .await
    {
        Ok(_) => true,
        // The presumed owner disclaims — its claim moved. Retry ONCE
        // toward the pod it names, then give up (bounded chain).
        Err(BusError::Nack(reason)) if reason.starts_with("not_owner:") => {
            let next = reason.trim_start_matches("not_owner:").to_string();
            if next.is_empty() || next == owner_pod {
                if best_effort {
                    debug!(%owner_pod, "media:view forward: owner moved, report dropped");
                    return false;
                }
                media_error(state, connection_id, "Conference moved — please rejoin").await;
                return false;
            }
            match bus
                .request_with_deadline(&next, "media.cmd", body, deadline)
                .await
            {
                Ok(_) => true,
                Err(e) => {
                    debug!(%e, "media.cmd retry toward re-declared owner failed");
                    if !best_effort {
                        media_error(state, connection_id, "Conference moved — please rejoin").await;
                    }
                    false
                }
            }
        }
        Err(BusError::Nack(reason)) => {
            if best_effort {
                debug!(%owner_pod, %reason, "media:view forward refused, report dropped");
                return false;
            }
            media_error(state, connection_id, &format!("Conference error: {reason}")).await;
            false
        }
        // Deadline/Redis — the owner is presumed dead: prune the record
        // we acted on (compare-DEL against what we observed) so the next
        // join re-claims on a live pod. Never for a view: a dropped report
        // is no evidence that the owner is dead.
        Err(e) if best_effort => {
            debug!(%owner_pod, %e, "media:view forward failed, report dropped (no prune)");
            false
        }
        Err(e) => {
            warn!(%owner_pod, %e, "media.cmd RPC failed — pruning presumed-dead owner record");
            prune_owner_record(state, connection_id, user_id, msg_type, data, owner_pod).await
        }
    }
}

/// RPC-failure fallback: compare-DEL the dead owner's record; a JOIN then
/// re-resolves immediately (typically winning the claim locally) so the
/// viewer heals without a manual retry.
async fn prune_owner_record(
    state: &ConferenceState,
    connection_id: &str,
    user_id: &ObjectId,
    msg_type: &str,
    data: Option<&serde_json::Value>,
    owner_pod: &str,
) -> bool {
    let rid = data
        .and_then(|d| d.get("room_id"))
        .and_then(|v| v.as_str())
        .and_then(|s| ObjectId::parse_str(s).ok());
    if let (Some(dir), Some(rid)) = (state.cluster_directory.clone(), rid) {
        let key = media_key(&rid.to_hex());
        if let Ok(Some(raw)) = dir.get(&key).await
            && OwnerRecord::parse(&raw).map(|r| r.pod_id) == Some(owner_pod.to_string())
        {
            let _ = dir.release(&key, &raw).await;
        }
        if msg_type == "media:join" {
            // One re-resolve: with the corpse pruned this usually claims
            // locally and serves the join in-place.
            if let MediaRoute::Local { create } = resolve_media_route(state, &rid, true).await {
                crate::ws_media::dispatch_media_local(
                    state,
                    user_id,
                    connection_id,
                    msg_type,
                    data,
                    create,
                )
                .await;
                return true;
            }
        }
    }
    media_error(
        state,
        connection_id,
        "Conference owner unreachable — please rejoin",
    )
    .await;
    false
}

async fn media_error(state: &ConferenceState, connection_id: &str, message: &str) {
    let msg = serde_json::json!({
        "type": "media:error",
        "data": { "message": message }
    });
    roomler_core::ws::dispatcher::send_to_connection_routed(
        &state.ws_storage,
        &state.redis_pubsub,
        connection_id,
        &msg,
    )
    .await;
}

/// Why a room's media closes, which decides who tells its people what.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseReason {
    /// The call ended (`call/end`, the last one out): the caller tells the
    /// room's members `room:call_ended` and closes the call's stats record.
    Ended,
    /// The room was deleted. Its members and its call id went with it, so
    /// the pod serving the call tells each connection in it and closes the
    /// call's stats record by the room (`revoke::deleted_rooms_call_ended`).
    RoomDeleted,
}

impl CloseReason {
    /// The `reason` a `media.close_room` request carries: none for an ended
    /// call (the body every pod has always sent), `room_deleted` for a
    /// deleted room.
    fn wire(self) -> Option<&'static str> {
        match self {
            Self::Ended => None,
            Self::RoomDeleted => Some(crate::revoke::ROOM_DELETED),
        }
    }

    /// The reason a `media.close_room` request names. One this pod does not
    /// know closes the room as an ended call, as it did before requests named
    /// any.
    fn from_wire(reason: Option<&str>) -> Self {
        match reason {
            Some(crate::revoke::ROOM_DELETED) => Self::RoomDeleted,
            _ => Self::Ended,
        }
    }
}

/// What closing a room's media wherever it lives came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoomClose {
    /// This pod served it: these connections' seats closed here.
    Here(Vec<String>),
    /// The pod serving it closed it when asked, and named this many seats
    /// (none from a pod whose code predates naming them, and none when its
    /// answer came too late but its claim is gone with the media).
    Owner { pod: String, seats: usize },
    /// No pod serves its media: there was nothing to close.
    Nothing,
    /// Which pod serves it, or that pod itself, could not be reached, so its
    /// media may still run there. For a deleted room that pod ends the call on
    /// its own once the room's document is gone ([`end_calls_of_rooms_gone`]).
    NotReached(String),
}

/// Close a room's media island wherever it lives (call_end / last-leave
/// auto-end / a deleted room's call): locally = [`close_room_here`]; foreign =
/// `media.close_room` RPC to the owner, naming the reason, so the owner closes
/// it the same way. A directory that cannot be read, or an owner that does not
/// answer, is [`RoomClose::NotReached`], never "nothing to close".
pub async fn close_room_everywhere(
    state: &ConferenceState,
    rid: &ObjectId,
    reason: CloseReason,
) -> RoomClose {
    if state.room_manager.has_room(rid) {
        return RoomClose::Here(close_room_here(state, rid, reason).await);
    }
    let route = match try_resolve_media_route(state, rid, false).await {
        Ok(route) => route,
        Err(e) => {
            warn!(room = %rid, %e, "media directory read failed");
            release_media_claim(state, rid).await;
            return RoomClose::NotReached(format!("the media directory: {e}"));
        }
    };
    match route {
        MediaRoute::Remote(pod) => {
            let Some(bus) = state.cluster_bus.clone() else {
                return RoomClose::NotReached(format!("pod {pod}: no bus to reach it"));
            };
            let mut body = serde_json::json!({ "room_id": rid.to_hex() });
            if let Some(why) = reason.wire() {
                body["reason"] = serde_json::Value::from(why);
            }
            match bus.request(&pod, "media.close_room", body).await {
                Ok(reply) => RoomClose::Owner {
                    seats: reply["closed"].as_array().map_or(0, Vec::len),
                    pod,
                },
                // No answer in time: read the claim again. Gone, it went with
                // the media (the owner closed it and answered late, or its
                // record ran out with it): there is nothing left to close. Still
                // naming that pod, it is pruned, and the media may still run
                // there.
                Err(e) => {
                    let Some(dir) = state.cluster_directory.clone() else {
                        return RoomClose::NotReached(format!("pod {pod}: {e}"));
                    };
                    let key = media_key(&rid.to_hex());
                    match dir.get(&key).await {
                        Ok(None) => {
                            info!(room = %rid, %pod, %e, "media.close_room had no answer in time, and the claim is gone with the media");
                            RoomClose::Owner { pod, seats: 0 }
                        }
                        Ok(Some(raw)) => {
                            if OwnerRecord::parse(&raw).map(|r| r.pod_id).as_ref() == Some(&pod) {
                                warn!(room = %rid, %pod, %e, "media.close_room RPC failed — pruning record");
                                let _ = dir.release(&key, &raw).await;
                            }
                            RoomClose::NotReached(format!("pod {pod}: {e}"))
                        }
                        Err(again) => RoomClose::NotReached(format!(
                            "pod {pod}: {e}; the media directory: {again}"
                        )),
                    }
                }
            }
        }
        MediaRoute::Local { .. } => {
            release_media_claim(state, rid).await;
            RoomClose::Nothing
        }
    }
}

/// Close a room's media on this pod, the one serving it: every seat's
/// transports, producers and consumers and the router go, and the claim is
/// released. For a deleted room, each connection that was seated in it hears
/// so and the call's stats record closes. A deleted room's call ends here
/// whichever path found it, so they cannot drift: the room delete's hook on
/// the pod serving the call, the `media.close_room` request it sends that pod
/// from another, the reconcile ([`end_calls_of_rooms_gone`]), and the guards
/// of a join or a start that raced the delete. Returns the connections that
/// were seated in it.
pub(crate) async fn close_room_here(
    state: &ConferenceState,
    rid: &ObjectId,
    reason: CloseReason,
) -> Vec<String> {
    let closed = state.room_manager.remove_room(rid);
    release_media_claim(state, rid).await;
    if reason == CloseReason::RoomDeleted {
        crate::revoke::deleted_rooms_call_ended(state, *rid, &closed).await;
    }
    closed
}

/// Release our claim for one room (compare-DEL via the held token).
pub async fn release_media_claim(state: &ConferenceState, rid: &ObjectId) {
    if let Some((_, token)) = state.media_claim_tokens.remove(rid)
        && let Some(dir) = state.cluster_directory.clone()
        && let Err(e) = dir.release(&media_key(&rid.to_hex()), &token).await
    {
        debug!(room = %rid, %e, "media claim release failed (TTL is the backstop)");
    }
}

/// Tell a remote owner one user left the call (HTTP `call/leave` served
/// on a non-owner pod): the owner drops the participant's transports and
/// broadcasts `media:peer_left` itself. `connection_id: Some(_)` scopes the
/// close to that one connection (multi-device: the user's other browsers
/// keep their media); `None` is the legacy close-all-of-user path, kept as
/// the mixed-version fallback during rolls.
pub async fn rpc_leave_user(
    state: &ConferenceState,
    owner_pod: &str,
    rid: &ObjectId,
    user_id: &ObjectId,
    connection_id: Option<&str>,
) {
    let mut body = serde_json::json!({ "room_id": rid.to_hex(), "user_id": user_id.to_hex() });
    if let Some(cid) = connection_id {
        body["connection_id"] = serde_json::Value::String(cid.to_string());
    }
    if let Some(bus) = state.cluster_bus.clone()
        && let Err(e) = bus.request(owner_pod, "media.leave_user", body).await
    {
        debug!(room = %rid, %owner_pod, %e, "media.leave_user RPC failed");
    }
}

/// Best-effort leave for a REMOTE participant whose WS just closed on
/// this pod: the owner still holds their transports.
pub async fn forward_close_leave(state: &ConferenceState, user_id: &ObjectId, connection_id: &str) {
    let Some((_, rid)) = state.remote_media_conns.remove(connection_id) else {
        return;
    };
    let data = serde_json::json!({ "room_id": rid.to_hex() });
    match resolve_media_route(state, &rid, false).await {
        MediaRoute::Remote(pod) => {
            if let Some(bus) = state.cluster_bus.clone() {
                let body = serde_json::json!({
                    "user_id": user_id.to_hex(),
                    "connection_id": connection_id,
                    "msg_type": "media:leave",
                    "data": data,
                });
                if let Err(e) = bus.request(&pod, "media.cmd", body).await {
                    debug!(room = %rid, %pod, %e, "close-time media:leave forward failed");
                }
            }
        }
        // Folded home meanwhile — the plain local leave covers it.
        MediaRoute::Local { .. } => {
            crate::ws_media::dispatch_media_local(
                state,
                user_id,
                connection_id,
                "media:leave",
                Some(&data),
                false,
            )
            .await;
        }
    }
}

/// The claim-loser rule: tear down the local island and push a rejoin
/// signal; participants re-enter via the normal join path, which routes
/// to the surviving owner.
pub async fn fold_media_room(state: &ConferenceState, rid: &ObjectId, reason: &str) {
    warn!(room = %rid, reason, "folding local media-room island");
    roomler_core::cluster::metrics::bump(&roomler_core::cluster::metrics::MEDIA_FOLD_TOTAL);
    // The key belongs to the foreign owner now — drop the token WITHOUT
    // a release.
    state.media_claim_tokens.remove(rid);
    let conns: Vec<String> = state
        .room_manager
        .rooms_ref()
        .get(rid)
        .map(|room| room.participants.iter().map(|p| p.key().clone()).collect())
        .unwrap_or_default();
    state.room_manager.remove_room(rid);
    let event = serde_json::json!({
        "type": "media:room_closed",
        "data": { "room_id": rid.to_hex(), "reason": "rehomed" }
    });
    for conn in &conns {
        roomler_core::ws::dispatcher::send_to_connection_routed(
            &state.ws_storage,
            &state.redis_pubsub,
            conn,
            &event,
        )
        .await;
    }
}

/// One claim-maintenance pass: lazy-release tokens whose room is gone,
/// refresh the rest, fold on CONFLICT. Public so tests drive it directly.
pub async fn refresh_media_claims_once(state: &ConferenceState) {
    let Some(dir) = state.cluster_directory.clone() else {
        return;
    };
    let held: Vec<(ObjectId, String)> = state
        .media_claim_tokens
        .iter()
        .map(|e| (*e.key(), e.value().clone()))
        .collect();
    for (rid, token) in held {
        let key = media_key(&rid.to_hex());
        if !state.room_manager.has_room(&rid) {
            // Room closed (call ended / island folded elsewhere): release
            // lazily so the next join can claim fresh.
            if state.media_claim_tokens.remove(&rid).is_some() {
                let _ = dir.release(&key, &token).await;
            }
            continue;
        }
        match dir.refresh_if_mine(&key, &token, MEDIA_TTL_SECS).await {
            Ok(true) => {}
            Ok(false) => fold_media_room(state, &rid, "claim held by a foreign pod").await,
            // Redis flap: never fold on an ERROR — the 30 s TTL is the
            // backstop and the next pass re-asserts.
            Err(e) => debug!(room = %rid, %e, "media claim refresh failed"),
        }
    }

    // Adopt-or-fold for TOKENLESS local rooms (belt-created while the
    // directory was down, or a claim lost to TTL during a flap): on
    // recovery each holder races the NX claim — the winner adopts its
    // island, the loser folds. This is what auto-resolves a belt-era
    // split within one beat of Redis returning.
    let orphans: Vec<ObjectId> = state
        .room_manager
        .rooms_ref()
        .iter()
        .map(|e| *e.key())
        .filter(|rid| !state.media_claim_tokens.contains_key(rid))
        .collect();
    for rid in orphans {
        let key = media_key(&rid.to_hex());
        let token = dir.owner_token("media");
        match dir.claim_nx(&key, &token, MEDIA_TTL_SECS).await {
            Ok(ClaimOutcome::Won) => {
                info!(room = %rid, "adopted tokenless local media room (claim re-won)");
                state.media_claim_tokens.insert(rid, token);
            }
            Ok(ClaimOutcome::Foreign(raw)) => match OwnerRecord::parse(&raw) {
                // Our own stale-epoch corpse: prune, then retry once.
                Some(rec) if rec.pod_id == state.pod.pod_id && dir.is_foreign(&raw) => {
                    let _ = dir.release(&key, &raw).await;
                    if let Ok(ClaimOutcome::Won) = dir.claim_nx(&key, &token, MEDIA_TTL_SECS).await
                    {
                        info!(room = %rid, "adopted tokenless local media room (stale epoch pruned)");
                        state.media_claim_tokens.insert(rid, token);
                    }
                }
                // Same-process record without a token (release race):
                // leave it — the next pass settles.
                Some(_) if !dir.is_foreign(&raw) => {}
                _ => {
                    fold_media_room(state, &rid, "belt-era split — foreign pod holds the claim")
                        .await
                }
            },
            Err(e) => debug!(room = %rid, %e, "orphan media room claim attempt failed"),
        }
    }
}

/// How many reads in a row must find no document for a room before the pod
/// serving its media ends its call. Two, a reconcile pass apart: one read
/// that misses a document (a secondary that has not caught up) never ends a
/// live call.
pub(crate) const MISSES_TO_END: u8 = 2;

/// How long the reconcile's read of the rooms may take. A slower read counts
/// as failed, and the reconcile goes on to its next pass.
const ROOMS_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// From which failed read in a row the reconcile warns: a backstop that
/// cannot read is seen in the log, not only at debug.
const FAILED_READS_TO_WARN: u32 = 3;

/// The reconcile, one pass. It runs every [`MEDIA_REFRESH_SECS`] on every pod,
/// in a loop of its own (`wire_media_cluster`), so a slow claim refresh never
/// holds it up, nor it the refresh. Public so tests drive it directly.
///
/// A room whose document is gone while its media runs here was deleted, and
/// no word of it ended its call here: the delete's hook failed or never ran,
/// its request to this pod was lost, or a pod whose code ends no call served
/// the delete. This pod ends the call itself, the way the room delete ends it
/// ([`close_room_here`]), once [`MISSES_TO_END`] reads in a row found no
/// document. "No document" is literal: a room's delete removes it, and a
/// legacy room that still carries `deleted_at` HAS a document, so the
/// reconcile leaves its call alone. A read that fails, breaks off or is late
/// changes nothing, and from the [`FAILED_READS_TO_WARN`]th in a row each one
/// is a warning.
pub async fn end_calls_of_rooms_gone(state: &ConferenceState) {
    let served: Vec<ObjectId> = state
        .room_manager
        .rooms_ref()
        .iter()
        .map(|e| *e.key())
        .collect();
    let found = if served.is_empty() {
        Ok(std::collections::HashSet::new())
    } else {
        rooms_with_a_document(state, &served).await
    };
    match &found {
        Ok(_) => state
            .media_reconcile_failed_reads
            .store(0, Ordering::Relaxed),
        Err(e) => {
            let failed = state
                .media_reconcile_failed_reads
                .fetch_add(1, Ordering::Relaxed)
                .saturating_add(1);
            if failed >= FAILED_READS_TO_WARN {
                warn!(
                    rooms = served.len(), failed_reads = failed, %e,
                    "media reconcile: the rooms read keeps failing; no deleted room's call ends here until it reads again"
                );
            } else {
                debug!(rooms = served.len(), %e, "media reconcile: the rooms read failed; nothing changes");
            }
        }
    }
    let gone = {
        let mut misses = state
            .media_room_misses
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        rooms_gone(&mut misses, &served, found.as_ref().ok())
    };
    for rid in gone {
        warn!(room = %rid, "media reconcile: the room of a call served here is gone, ending its call");
        close_room_here(state, &rid, CloseReason::RoomDeleted).await;
    }
}

/// The rooms among `ids` that have a document, read by `_id` in one query.
/// Only a read that completes counts: a query error, a cursor that breaks off
/// mid-way and a read slower than [`ROOMS_READ_TIMEOUT`] are each an error,
/// never "absent".
async fn rooms_with_a_document(
    state: &ConferenceState,
    ids: &[ObjectId],
) -> Result<std::collections::HashSet<ObjectId>, String> {
    use futures::TryStreamExt;
    let ids: Vec<bson::Bson> = ids.iter().copied().map(bson::Bson::ObjectId).collect();
    let read = async {
        let cursor = state
            .db
            .collection::<bson::Document>("rooms")
            .find(bson::doc! { "_id": { "$in": ids } })
            .projection(bson::doc! { "_id": 1 })
            .await
            .map_err(|e| e.to_string())?;
        let docs: Vec<bson::Document> = cursor.try_collect().await.map_err(|e| e.to_string())?;
        docs.iter()
            .map(|d| d.get_object_id("_id").map_err(|e| e.to_string()))
            .collect::<Result<std::collections::HashSet<ObjectId>, String>>()
    };
    tokio::time::timeout(ROOMS_READ_TIMEOUT, read)
        .await
        .map_err(|_| format!("no answer within {ROOMS_READ_TIMEOUT:?}"))?
}

/// One read's verdict on the rooms whose media this pod serves, `found` being
/// those it found a document for, or `None` when the read failed: a failed
/// read changes nothing. A room the read found starts again from zero misses;
/// one it did not counts one more, and is gone (returned) on its
/// [`MISSES_TO_END`]th miss in a row. A room no longer served here is
/// forgotten.
pub(crate) fn rooms_gone(
    misses: &mut std::collections::HashMap<ObjectId, u8>,
    served: &[ObjectId],
    found: Option<&std::collections::HashSet<ObjectId>>,
) -> Vec<ObjectId> {
    let Some(found) = found else {
        return Vec::new();
    };
    misses.retain(|rid, _| served.contains(rid));
    let mut gone = Vec::new();
    for rid in served {
        if found.contains(rid) {
            misses.remove(rid);
            continue;
        }
        let count = misses.entry(*rid).or_insert(0);
        *count = count.saturating_add(1);
        if *count >= MISSES_TO_END {
            misses.remove(rid);
            gone.push(*rid);
        }
    }
    gone
}

/// Wire the C-4 machinery: bus handlers (owner side) + the claim
/// heartbeat. Called once at the end of `ConferenceState::new` (the bus
/// handlers need the fully-built state).
/// Graceful shutdown: compare-DEL every media claim this pod holds so a
/// deploy hands each room off with a ZERO-length ownerless window instead
/// of waiting out the 30 s TTL. Conference's `Module::shutdown`; the host's
/// `shutdown_cleanup` used to do this inline (C-4).
pub async fn release_all_claims(state: &ConferenceState) {
    let Some(dir) = &state.cluster_directory else {
        return;
    };
    let held: Vec<(ObjectId, String)> = state
        .media_claim_tokens
        .iter()
        .map(|e| (*e.key(), e.value().clone()))
        .collect();
    for (rid, token) in held {
        state.media_claim_tokens.remove(&rid);
        if let Err(e) = dir.release(&media_key(&rid.to_hex()), &token).await {
            debug!(room = %rid, %e, "shutdown: media claim release failed");
        }
    }
}

pub fn wire_media_cluster(state: &ConferenceState) {
    if let Some(bus) = &state.cluster_bus {
        // media.cmd — execute one viewer command against the local room.
        let st = state.clone();
        bus.register("media.cmd", move |body| {
            let st = st.clone();
            Box::pin(async move { handle_media_cmd_rpc(st, body).await })
        });

        // media.leave_user — HTTP call/leave served on a non-owner pod.
        // With `connection_id` the close is scoped to that one connection
        // (same-user other devices keep their transports); without it we
        // keep the legacy close-all-of-user behaviour (mixed-version rolls).
        let st = state.clone();
        bus.register("media.leave_user", move |body| {
            let st = st.clone();
            Box::pin(async move {
                let rid = parse_oid(&body, "room_id")?;
                let uid = parse_oid(&body, "user_id")?;
                let conn_id = body.get("connection_id").and_then(|v| v.as_str());
                let closed = match conn_id {
                    Some(cid) => st.room_manager.close_participant_of_user(&rid, cid, &uid),
                    None => {
                        st.room_manager.close_participant_by_user(&rid, &uid);
                        true
                    }
                };
                let remaining = st.room_manager.get_participant_user_ids(&rid);
                if closed && !remaining.is_empty() {
                    let mut data =
                        serde_json::json!({ "room_id": rid.to_hex(), "user_id": uid.to_hex() });
                    if let Some(cid) = conn_id {
                        data["connection_id"] = serde_json::Value::String(cid.to_string());
                    }
                    let event = serde_json::json!({ "type": "media:peer_left", "data": data });
                    roomler_core::ws::dispatcher::broadcast_with_redis(
                        &st.ws_storage,
                        &st.redis_pubsub,
                        &remaining,
                        &event,
                    )
                    .await;
                }
                Ok(serde_json::json!({ "ok": true }))
            })
        });

        // media.withdraw_seats — a membership ended, served on another pod:
        // close the person's seats in this pod's room (`revoke`). Replies
        // with the connections it closed.
        let st = state.clone();
        bus.register(crate::revoke::WITHDRAW_SEATS, move |body| {
            let st = st.clone();
            Box::pin(async move {
                let rid = parse_oid(&body, "room_id")?;
                let uid = parse_oid(&body, "user_id")?;
                let closed = crate::revoke::withdraw_seats_here(&st, rid, uid).await;
                Ok(serde_json::json!({ "closed": closed }))
            })
        });

        // media.close_room — call_end/auto-end, or a deleted room's call,
        // served on a non-owner pod. With `reason: "room_deleted"` this pod
        // ends the call as the room delete ends it and tells its own people
        // (`close_room_here`); without one, as an ended call. Replies with the
        // connections it closed.
        let st = state.clone();
        bus.register("media.close_room", move |body| {
            let st = st.clone();
            Box::pin(async move {
                let rid = parse_oid(&body, "room_id")?;
                let reason = CloseReason::from_wire(body.get("reason").and_then(|v| v.as_str()));
                let conns = close_room_here(&st, &rid, reason).await;
                if reason == CloseReason::Ended {
                    let event = serde_json::json!({
                        "type": "media:room_closed",
                        "data": { "room_id": rid.to_hex(), "reason": "ended" }
                    });
                    for conn in &conns {
                        roomler_core::ws::dispatcher::send_to_connection_routed(
                            &st.ws_storage,
                            &st.redis_pubsub,
                            conn,
                            &event,
                        )
                        .await;
                    }
                }
                Ok(serde_json::json!({ "ok": true, "closed": conns }))
            })
        });
    }

    if state.cluster_directory.is_some() {
        let st = state.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(MEDIA_REFRESH_SECS)).await;
                refresh_media_claims_once(&st).await;
            }
        });
    }

    // The reconcile that ends a deleted room's call, in a loop of its own: it
    // runs on every pod, a directory or none, and neither it nor the claim
    // refresh waits for the other.
    let st = state.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(MEDIA_REFRESH_SECS)).await;
            end_calls_of_rooms_gone(&st).await;
        }
    });
}

/// Owner-side execution of a forwarded command. Re-resolves placement
/// (the claim may have moved since the caller looked): still ours →
/// dispatch against the local handlers, whose replies/pushes ride the
/// conn-addressed channel back; moved → structured NACK naming the new
/// owner so the caller can retry once. The participation gate runs here,
/// where the seats live: a join is admitted before placement and before this
/// pod seats the connection, and every other command needs its seat
/// (`dispatch_media_local`).
async fn handle_media_cmd_rpc(
    state: ConferenceState,
    body: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let uid = parse_oid(&body, "user_id")?;
    let conn = body
        .get("connection_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing connection_id".to_string())?
        .to_string();
    let msg_type = body
        .get("msg_type")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing msg_type".to_string())?
        .to_string();
    let data = body.get("data").cloned().filter(|d| !d.is_null());
    let rid = data
        .as_ref()
        .and_then(|d| d.get("room_id"))
        .and_then(|v| v.as_str())
        .and_then(|s| ObjectId::parse_str(s).ok())
        .ok_or_else(|| "missing room_id".to_string())?;

    // A forwarded join is admitted here as well, and before placement, so
    // one this pod refuses (a pod that did not gate it forwarded it) claims
    // nothing. The refusal reaches the viewer conn-addressed, and the RPC
    // still answers ok: the viewer's pod adds no error of its own.
    let pass = if msg_type == "media:join" {
        match crate::media_gate::admit_join(&state, uid, &conn, rid).await {
            Ok(pass) => Some(pass),
            Err(refusal) => {
                crate::ws_media::refuse_join(&state, &uid, &conn, rid, &refusal).await;
                return Ok(serde_json::json!({ "ok": true }));
            }
        }
    } else {
        None
    };

    match resolve_media_route(&state, &rid, pass.is_some()).await {
        MediaRoute::Remote(pod) => Err(format!("not_owner:{pod}")),
        MediaRoute::Local { create } => {
            match (&pass, data.as_ref()) {
                (Some(pass), Some(frame)) => {
                    crate::ws_media::handle_media_join(&state, pass, frame, create).await;
                }
                _ => {
                    crate::ws_media::dispatch_media_local(
                        &state,
                        &uid,
                        &conn,
                        &msg_type,
                        data.as_ref(),
                        create,
                    )
                    .await;
                }
            }
            Ok(serde_json::json!({ "ok": true }))
        }
    }
}

fn parse_oid(body: &serde_json::Value, field: &str) -> Result<ObjectId, String> {
    body.get(field)
        .and_then(|v| v.as_str())
        .and_then(|s| ObjectId::parse_str(s).ok())
        .ok_or_else(|| format!("missing {field}"))
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};

    use bson::oid::ObjectId;

    use super::{CloseReason, MISSES_TO_END, rooms_gone};

    /// The reconcile's verdict, read by read: a room is gone on its second
    /// miss in a row and not before; a read that finds the room starts its
    /// count again; a failed read changes nothing, a count included; a room
    /// no longer served is forgotten; a room that is found is never gone.
    #[test]
    fn a_room_is_gone_on_its_second_miss_in_a_row_and_a_failed_read_changes_nothing() {
        assert_eq!(MISSES_TO_END, 2);
        let (live, deleted) = (ObjectId::new(), ObjectId::new());
        let served = [live, deleted];
        let found_live: HashSet<ObjectId> = HashSet::from([live]);
        let found_both: HashSet<ObjectId> = HashSet::from([live, deleted]);
        let mut misses = HashMap::new();

        // A first miss is not enough.
        assert!(rooms_gone(&mut misses, &served, Some(&found_live)).is_empty());
        assert_eq!(misses.get(&deleted), Some(&1));
        assert_eq!(misses.get(&live), None);

        // A failed read changes nothing: no verdict, the count stays.
        assert!(rooms_gone(&mut misses, &served, None).is_empty());
        assert_eq!(misses.get(&deleted), Some(&1));

        // A read that finds the room starts it again from zero.
        assert!(rooms_gone(&mut misses, &served, Some(&found_both)).is_empty());
        assert_eq!(misses.get(&deleted), None);

        // Two misses in a row: gone, and its count goes with it.
        assert!(rooms_gone(&mut misses, &served, Some(&found_live)).is_empty());
        assert_eq!(
            rooms_gone(&mut misses, &served, Some(&found_live)),
            vec![deleted]
        );
        assert_eq!(misses.get(&deleted), None);

        // A missed room that is no longer served here is forgotten.
        assert!(rooms_gone(&mut misses, &served, Some(&found_live)).is_empty());
        assert_eq!(misses.get(&deleted), Some(&1));
        assert!(rooms_gone(&mut misses, &[live], Some(&found_live)).is_empty());
        assert!(misses.is_empty(), "{misses:?}");

        // A room every read finds is never gone.
        for _ in 0..5 {
            assert!(rooms_gone(&mut misses, &[live], Some(&found_live)).is_empty());
        }
        assert!(misses.is_empty(), "{misses:?}");
    }

    /// The `reason` on the bus: an ended call's request carries none, as every
    /// pod has always sent it, and a reason this pod does not know closes the
    /// room as an ended call.
    #[test]
    fn a_close_request_names_a_deleted_room_and_nothing_else() {
        assert_eq!(CloseReason::Ended.wire(), None);
        assert_eq!(CloseReason::RoomDeleted.wire(), Some("room_deleted"));
        assert_eq!(
            CloseReason::from_wire(Some("room_deleted")),
            CloseReason::RoomDeleted
        );
        assert_eq!(CloseReason::from_wire(None), CloseReason::Ended);
        assert_eq!(CloseReason::from_wire(Some("rehomed")), CloseReason::Ended);
    }
}
