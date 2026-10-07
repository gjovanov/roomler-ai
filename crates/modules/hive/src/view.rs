// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P0d-2b — the viewer peer, server side: the `hive:view.*` namespace on
//! the user socket, and the grants it mints.
//!
//! A member of a session's room asks to read it; the server checks the asker
//! (in the org, in the session's room — anyone else is told `not_found`, the
//! answer a bogus id gets), the device (online here, advertising
//! `hive-view`), and a rate; mints a GRANT and pushes it to the device. The
//! browser is told `ready` — with the ICE servers minted for this peer — only
//! after the DEVICE confirmed the grant (FR-83): before that it has nothing to
//! dial. Then the server relays the WebRTC handshake, and never sees what
//! flows over the peer.
//!
//! # The browser's frames (`{type, data}` on the user socket)
//!
//! | in | out |
//! |---|---|
//! | `hive:view.open {session_id, ref?}` | `hive:view.ready {ref, grant_id, session_id, ice_servers, ttl_secs, may_prompt}` or `hive:view.refused {ref, session_id, reason, message?}` |
//! | `hive:view.offer {grant_id, sdp}` | `hive:view.answer {grant_id, sdp}` |
//! | `hive:view.ice {grant_id, candidate}` | `hive:view.ice {grant_id, candidate}` |
//! | `hive:view.renew {grant_id}` | `hive:view.renewed {grant_id, ttl_secs}` |
//! | `hive:view.close {grant_id}` | `hive:view.closed {grant_id, reason}` (also unasked) |
//!
//! # Whose grant it is
//!
//! A grant belongs to ONE browser connection and ONE device. A frame naming it
//! from any other connection, user or device moves nothing — grant ids are
//! ObjectIds, structured, not secret. The table is pod-local, like the start
//! waiters: the grant is pushed only when the device's socket is on this pod,
//! its answers arrive on that socket, and replies to the browser are routed
//! (`send_to_connection_routed`) wherever its socket is.
//!
//! The grant ends when the browser closes it or its socket closes, when the
//! device reports it closed, when a renewal finds the viewer no longer in the
//! room, or when the viewer's membership, the device or the org is removed.

use std::time::Duration;

use async_trait::async_trait;
use bson::oid::ObjectId;
use dashmap::DashMap;
use roomler_ai_remote_control::hive::view_limits;
use roomler_ai_remote_control::signaling::{ClientMsg, IceServer, ServerMsg};
use roomler_ai_remote_control::turn_creds::ice_servers_for_session;
use roomler_core::{WsCtx, WsHandler};
use serde_json::{Value, json};
use tracing::{debug, info, warn};

use crate::HiveState;
use crate::model::AgentSession;
use crate::routes::Audit;

/// Open grants one browser connection may hold.
const MAX_PER_CONNECTION: usize = 4;
/// The largest SDP or candidate a browser may send through.
const MAX_SDP_BYTES: usize = 64 * 1024;
const MAX_CANDIDATE_BYTES: usize = 4 * 1024;
/// A browser's `ref`, echoed back, capped.
const MAX_REF: usize = 64;

struct Grant {
    tenant_id: ObjectId,
    session_id: ObjectId,
    device_id: ObjectId,
    user_id: ObjectId,
    connection_id: String,
    may_prompt: bool,
    /// The device confirmed it: the browser may dial.
    ready: bool,
    /// Minted once, the same for both ends.
    ice_servers: Vec<IceServer>,
    reference: Option<String>,
}

/// The view grants this pod holds.
#[derive(Default)]
pub struct ViewGrants {
    map: DashMap<ObjectId, Grant>,
}

impl ViewGrants {
    pub fn new() -> Self {
        Self::default()
    }

    /// Grants held now. Tests and diagnostics only.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    fn take_where(&self, keep: impl Fn(&Grant) -> bool) -> Vec<(ObjectId, Grant)> {
        let ids: Vec<ObjectId> = self
            .map
            .iter()
            .filter(|e| !keep(e.value()))
            .map(|e| *e.key())
            .collect();
        ids.into_iter()
            .filter_map(|id| self.map.remove(&id))
            .collect()
    }
}

/// The `hive` namespace on the user socket.
pub struct HiveView {
    pub state: HiveState,
}

#[async_trait]
impl WsHandler for HiveView {
    async fn handle(&self, ctx: &WsCtx, msg: Value) -> anyhow::Result<()> {
        let kind = msg.get("type").and_then(Value::as_str).unwrap_or("");
        let data = msg.get("data").cloned().unwrap_or(Value::Null);
        match kind {
            "hive:view.open" => open(&self.state, ctx, &data).await,
            "hive:view.offer" => offer(&self.state, ctx, &data),
            "hive:view.ice" => ice(&self.state, ctx, &data),
            "hive:view.renew" => renew(&self.state, ctx, &data).await,
            "hive:view.close" => close(&self.state, ctx, &data).await,
            other => debug!(user = %ctx.principal, kind = other, "hive: an unknown view frame"),
        }
        Ok(())
    }

    /// The browser is gone: so is every grant it held.
    async fn closed(&self, ctx: &WsCtx) {
        let gone = self
            .state
            .view_grants
            .take_where(|g| g.connection_id != ctx.connection_id);
        for (grant_id, g) in gone {
            tell_device_closed(&self.state, grant_id, &g, "viewer_left");
        }
    }
}

fn grant_id_of(data: &Value) -> Option<ObjectId> {
    data.get("grant_id")
        .and_then(Value::as_str)
        .and_then(|s| ObjectId::parse_str(s).ok())
}

/// Send `{type, data}` to one browser connection, wherever its socket is.
async fn to_browser(state: &HiveState, connection_id: &str, kind: &str, data: Value) {
    roomler_core::ws::dispatcher::send_to_connection_routed(
        &state.core.ws_storage,
        &state.core.redis_pubsub,
        connection_id,
        &json!({ "type": kind, "data": data }),
    )
    .await;
}

fn tell_device_closed(state: &HiveState, grant_id: ObjectId, g: &Grant, reason: &str) {
    let msg = ServerMsg::HiveViewClose {
        grant_id,
        reason: reason.to_string(),
    };
    if let Err(e) = state
        .fleet
        .rc_hub
        .push_hive_view(g.device_id, g.tenant_id, msg)
    {
        debug!(grant = %grant_id, %e, "hive: a view close did not reach the device");
    }
}

/// May `user` read `s`? In the org, and in the session's room — chat's own
/// membership rule. A session from before rooms (P0b/P0c) is its owner's.
async fn may_read(state: &HiveState, s: &AgentSession, user: ObjectId) -> bool {
    if !matches!(state.tenants.is_member(s.tenant_id, user).await, Ok(true)) {
        return false;
    }
    match s.room_id {
        Some(room) => matches!(
            state.chat.is_member(s.tenant_id, room, user).await,
            Ok(true)
        ),
        None => s.owner_id == user,
    }
}

async fn open(state: &HiveState, ctx: &WsCtx, data: &Value) {
    let reference: Option<String> = data
        .get("ref")
        .and_then(Value::as_str)
        .map(|r| r.chars().take(MAX_REF).collect());
    let session_hex = data
        .get("session_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let refuse = |reason: &'static str, message: Option<String>| {
        let data = json!({
            "ref": reference,
            "session_id": session_hex,
            "reason": reason,
            "message": message,
        });
        async move { to_browser(state, &ctx.connection_id, "hive:view.refused", data).await }
    };

    let user = ctx.principal;
    let session = match ObjectId::parse_str(&session_hex) {
        Ok(sid) => state.sessions.find(sid).await.ok().flatten(),
        Err(_) => None,
    };
    // Not readable and not there answer alike: a non-member learns nothing.
    let s = match session {
        Some(s) if may_read(state, &s, user).await => s,
        _ => return refuse("not_found", None).await,
    };
    let Some(sid) = s.id else {
        return refuse("not_found", None).await;
    };
    let device_id = s.location.device_id;
    let audit = |outcome: &'static str, reason: Option<&'static str>| Audit {
        tenant_id: s.tenant_id,
        user_id: user,
        device_id,
        session_id: Some(sid),
        action: "view",
        outcome,
        reason,
    };

    let refused_by_server = match state.fleet.rc_hub.agent_supports_hive_view(device_id) {
        None => Some("device_offline"),
        Some(false) => Some("device_unsupported"),
        Some(true) => None,
    }
    .or_else(|| {
        let held = state
            .view_grants
            .map
            .iter()
            .filter(|g| g.connection_id == ctx.connection_id)
            .count();
        (held >= MAX_PER_CONNECTION).then_some("too_many_views")
    })
    .or_else(|| {
        (!state
            .view_limiter
            .check(user, sid, view_limits::OPEN_RATE_PER_MINUTE))
        .then_some("rate_limited")
    });
    if let Some(reason) = refused_by_server {
        audit("refused", Some(reason)).write(state).await;
        return refuse(reason, None).await;
    }

    // The starter drives a live session; everyone else reads (P1: drivers).
    let may_prompt = user == s.owner_id && !s.status.is_terminal();
    let user_name = match state.users.base.find_by_id(user).await {
        Ok(u) => u.display_name,
        Err(_) => String::new(),
    };
    let grant_id = ObjectId::new();
    let ice_servers = ice_servers_for_session(
        &user.to_hex(),
        &grant_id.to_hex(),
        state.core.turn_map.cfg_for(None),
    );
    // The grant BEFORE the push: the device's answer, however fast, must find
    // it.
    state.view_grants.map.insert(
        grant_id,
        Grant {
            tenant_id: s.tenant_id,
            session_id: sid,
            device_id,
            user_id: user,
            connection_id: ctx.connection_id.clone(),
            may_prompt,
            ready: false,
            ice_servers,
            reference: reference.clone(),
        },
    );
    let push = ServerMsg::HiveViewGrant {
        grant_id,
        session_id: sid,
        user_id: user,
        user_name,
        may_prompt,
        ttl_secs: view_limits::GRANT_TTL_SECS,
    };
    if let Err(e) = state
        .fleet
        .rc_hub
        .push_hive_view(device_id, s.tenant_id, push)
    {
        state.view_grants.map.remove(&grant_id);
        let reason = match e {
            roomler_ai_remote_control::error::Error::ExecUnsupported(_) => "device_unsupported",
            _ => "device_offline",
        };
        audit("refused", Some(reason)).write(state).await;
        return refuse(reason, None).await;
    }
    audit("sent", None).write(state).await;
    info!(grant = %grant_id, session = %sid, device = %device_id, user = %user, may_prompt, "hive: view granted");

    // A device that never answers: the browser is told, and the device told
    // to drop the grant should its answer arrive after all.
    let state = state.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(view_limits::GRANT_ACK_TIMEOUT_SECS)).await;
        let Some((_, g)) = state.view_grants.map.remove_if(&grant_id, |_, g| !g.ready) else {
            return;
        };
        tell_device_closed(&state, grant_id, &g, "no_answer");
        to_browser(
            &state,
            &g.connection_id,
            "hive:view.refused",
            json!({
                "ref": g.reference,
                "session_id": g.session_id.to_hex(),
                "reason": "no_answer",
                "message": "the device did not answer in time",
            }),
        )
        .await;
    });
}

/// The grant `data` names, if THIS connection's user holds it and the device
/// confirmed it.
fn ready_grant_of(
    state: &HiveState,
    ctx: &WsCtx,
    data: &Value,
) -> Option<(ObjectId, ObjectId, ObjectId)> {
    let grant_id = grant_id_of(data)?;
    let g = state.view_grants.map.get(&grant_id)?;
    (g.connection_id == ctx.connection_id && g.user_id == ctx.principal && g.ready).then_some((
        grant_id,
        g.device_id,
        g.tenant_id,
    ))
}

fn offer(state: &HiveState, ctx: &WsCtx, data: &Value) {
    let Some((grant_id, device_id, tenant_id)) = ready_grant_of(state, ctx, data) else {
        return debug!(user = %ctx.principal, "hive: a view offer for no ready grant of this connection");
    };
    let Some(sdp) = data.get("sdp").and_then(Value::as_str) else {
        return;
    };
    if sdp.len() > MAX_SDP_BYTES {
        return warn!(grant = %grant_id, bytes = sdp.len(), "hive: an oversized view offer — dropped");
    }
    let ice_servers = state
        .view_grants
        .map
        .get(&grant_id)
        .map(|g| g.ice_servers.clone())
        .unwrap_or_default();
    let msg = ServerMsg::HiveViewOffer {
        grant_id,
        sdp: sdp.to_string(),
        ice_servers,
    };
    if let Err(e) = state.fleet.rc_hub.push_hive_view(device_id, tenant_id, msg) {
        debug!(grant = %grant_id, %e, "hive: a view offer did not reach the device");
    }
}

fn ice(state: &HiveState, ctx: &WsCtx, data: &Value) {
    let Some((grant_id, device_id, tenant_id)) = ready_grant_of(state, ctx, data) else {
        return;
    };
    let Some(candidate) = data.get("candidate") else {
        return;
    };
    if candidate.to_string().len() > MAX_CANDIDATE_BYTES {
        return;
    }
    let msg = ServerMsg::HiveViewIce {
        grant_id,
        candidate: candidate.clone(),
    };
    if let Err(e) = state.fleet.rc_hub.push_hive_view(device_id, tenant_id, msg) {
        debug!(grant = %grant_id, %e, "hive: a view candidate did not reach the device");
    }
}

/// A renewal re-checks the viewer's right to read: one no longer in the
/// room loses the view now, not when the TTL runs out.
async fn renew(state: &HiveState, ctx: &WsCtx, data: &Value) {
    let Some((grant_id, device_id, tenant_id)) = ready_grant_of(state, ctx, data) else {
        return;
    };
    let session_id = match state.view_grants.map.get(&grant_id) {
        Some(g) => g.session_id,
        None => return,
    };
    let still = match state.sessions.find(session_id).await {
        Ok(Some(s)) => may_read(state, &s, ctx.principal).await,
        _ => false,
    };
    if !still {
        if let Some((_, g)) = state.view_grants.map.remove(&grant_id) {
            tell_device_closed(state, grant_id, &g, "not_a_member");
        }
        return to_browser(
            state,
            &ctx.connection_id,
            "hive:view.closed",
            json!({"grant_id": grant_id.to_hex(), "reason": "not_a_member"}),
        )
        .await;
    }
    let msg = ServerMsg::HiveViewRenew {
        grant_id,
        ttl_secs: view_limits::GRANT_TTL_SECS,
    };
    if let Err(e) = state.fleet.rc_hub.push_hive_view(device_id, tenant_id, msg) {
        debug!(grant = %grant_id, %e, "hive: a view renewal did not reach the device");
        return;
    }
    to_browser(
        state,
        &ctx.connection_id,
        "hive:view.renewed",
        json!({"grant_id": grant_id.to_hex(), "ttl_secs": view_limits::GRANT_TTL_SECS}),
    )
    .await;
}

async fn close(state: &HiveState, ctx: &WsCtx, data: &Value) {
    let Some(grant_id) = grant_id_of(data) else {
        return;
    };
    let Some((_, g)) = state
        .view_grants
        .map
        .remove_if(&grant_id, |_, g| g.connection_id == ctx.connection_id)
    else {
        return;
    };
    tell_device_closed(state, grant_id, &g, "viewer_closed");
    to_browser(
        state,
        &ctx.connection_id,
        "hive:view.closed",
        json!({"grant_id": grant_id.to_hex(), "reason": "viewer_closed"}),
    )
    .await;
}

/// A device's view frame, in the order its connection sent it. Applied only
/// to a grant pushed to THAT device.
pub(crate) async fn on_device_frame(state: &HiveState, device_id: ObjectId, msg: ClientMsg) {
    match msg {
        ClientMsg::HiveViewGrantAck {
            grant_id,
            refused,
            detail,
        } => {
            let reply = {
                let Some(mut g) = state.view_grants.map.get_mut(&grant_id) else {
                    return debug!(grant = %grant_id, "hive: a view ack for no grant here");
                };
                if g.device_id != device_id || g.ready {
                    return;
                }
                match refused {
                    None => {
                        g.ready = true;
                        Ok((
                            g.connection_id.clone(),
                            json!({
                                "ref": g.reference,
                                "grant_id": grant_id.to_hex(),
                                "session_id": g.session_id.to_hex(),
                                "ice_servers": g.ice_servers,
                                "ttl_secs": view_limits::GRANT_TTL_SECS,
                                "may_prompt": g.may_prompt,
                            }),
                        ))
                    }
                    Some(word) => Err(word),
                }
            };
            match reply {
                Ok((conn, data)) => to_browser(state, &conn, "hive:view.ready", data).await,
                Err(word) => {
                    let Some((_, g)) = state.view_grants.map.remove(&grant_id) else {
                        return;
                    };
                    info!(grant = %grant_id, refused = word.as_str(), "hive: the device refused a view");
                    let message = crate::agent_socket::clamp_words(
                        detail,
                        roomler_ai_remote_control::hive::hive_limits::MAX_DETAIL_LEN,
                    );
                    to_browser(
                        state,
                        &g.connection_id,
                        "hive:view.refused",
                        json!({
                            "ref": g.reference,
                            "session_id": g.session_id.to_hex(),
                            "reason": word.as_str(),
                            "message": message,
                        }),
                    )
                    .await;
                }
            }
        }
        ClientMsg::HiveViewAnswer { grant_id, sdp } => {
            if sdp.len() > MAX_SDP_BYTES {
                return;
            }
            if let Some(conn) = ready_for_device(state, grant_id, device_id) {
                to_browser(
                    state,
                    &conn,
                    "hive:view.answer",
                    json!({"grant_id": grant_id.to_hex(), "sdp": sdp}),
                )
                .await;
            }
        }
        ClientMsg::HiveViewIce {
            grant_id,
            candidate,
        } => {
            if candidate.to_string().len() > MAX_CANDIDATE_BYTES {
                return;
            }
            if let Some(conn) = ready_for_device(state, grant_id, device_id) {
                to_browser(
                    state,
                    &conn,
                    "hive:view.ice",
                    json!({"grant_id": grant_id.to_hex(), "candidate": candidate}),
                )
                .await;
            }
        }
        ClientMsg::HiveViewClosed { grant_id, reason } => {
            let Some((_, g)) = state
                .view_grants
                .map
                .remove_if(&grant_id, |_, g| g.device_id == device_id)
            else {
                return;
            };
            let reason: String = reason.chars().take(64).collect();
            to_browser(
                state,
                &g.connection_id,
                "hive:view.closed",
                json!({"grant_id": grant_id.to_hex(), "reason": reason}),
            )
            .await;
        }
        other => debug!(?other, "hive: not a view frame"),
    }
}

/// The browser connection of a CONFIRMED grant pushed to `device_id`.
fn ready_for_device(state: &HiveState, grant_id: ObjectId, device_id: ObjectId) -> Option<String> {
    let g = state.view_grants.map.get(&grant_id)?;
    (g.device_id == device_id && g.ready).then(|| g.connection_id.clone())
}

/// End every grant `which` selects: the device is told (unless it is the
/// one going away) and so is the browser.
pub(crate) async fn end_grants(
    state: &HiveState,
    which: impl Fn(&ObjectId, &ObjectId, &ObjectId) -> bool,
    reason: &str,
    tell_device: bool,
) -> usize {
    let gone = state
        .view_grants
        .take_where(|g| !which(&g.tenant_id, &g.user_id, &g.device_id));
    let n = gone.len();
    for (grant_id, g) in gone {
        if tell_device {
            tell_device_closed(state, grant_id, &g, reason);
        }
        to_browser(
            state,
            &g.connection_id,
            "hive:view.closed",
            json!({"grant_id": grant_id.to_hex(), "reason": reason}),
        )
        .await;
    }
    n
}
