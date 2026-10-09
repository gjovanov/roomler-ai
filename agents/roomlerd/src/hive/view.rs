// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P0d-2 — the viewer peer: a session's transcript, served to a browser
//! over a data-only WebRTC peer (`docs/roomler-hive-design.md` §5.2).
//!
//! # The handshake
//!
//! The server mints a view GRANT — this user may read this session for a
//! while, and maybe prompt it — and pushes it here as `rc:hive.view.grant`.
//! The device applies its own gates (the primary enrollment, `hive_enabled`,
//! a session it holds, capacity) and answers `rc:hive.view.grant_ack`; only
//! then is the browser told to dial. Its offer arrives as
//! `rc:hive.view.offer`, the answer leaves as `rc:hive.view.answer`, and ICE
//! trickles both ways. The server relays the handshake and never sees what
//! then flows over the peer.
//!
//! # One task per grant
//!
//! Each grant is an actor that OWNS its peer, its timers and its channel, so
//! there is exactly one place the peer is closed — and it always is. A WebRTC
//! peer that is dropped frees nothing (its ICE sockets belong to tasks the
//! stack spawned), and leaked peers once ate a host's whole ephemeral port
//! range while `ping` stayed fast. The WebRTC handlers capture only a channel
//! to the actor, never the peer, so no handler keeps its own connection
//! alive (#1740).
//!
//! The actor ends — closing the peer — when the server closes the grant, when
//! the grant runs out unrenewed, when no offer arrives within
//! [`OFFER_DEADLINE`], when the peer fails or the viewer closes the channel,
//! or when the viewer stops reading for [`SEND_STALL`]. Every end but the
//! server's own is reported back as `rc:hive.view.closed`.
//!
//! # Over the peer
//!
//! One DataChannel, `hive`, opened by the browser. Each message is a JSON
//! object carried in [`super::framing`] frames. The browser asks — `hello`,
//! `page`, `follow`, `unfollow`, `prompt`, `answer` — and the device answers
//! and pushes `events`, `state` and `approvals`. A prompt is taken only from a
//! grant that may prompt, and is attributed to its viewer in the transcript
//! and on the turn's stub; that grant is a DRIVER's, and only a driver
//! answers an approval (P1a). The server names drivers; this device decides
//! itself whether one who did not start the session may act here as its
//! account (P1c-2, [`Supervisor::drives_here`]).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bson::oid::ObjectId;
use bytes::Bytes;
use roomler_ai_remote_control::hive::{HiveRunState, HiveViewRefusal, view_limits};
use roomler_ai_remote_control::signaling::{ClientMsg, IceServer};
use roomler_hive_node::EventEnvelope;
use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc};
use tokio::time::Instant;
use tracing::{debug, info, warn};
use webrtc::api::APIBuilder;
use webrtc::api::setting_engine::SettingEngine;
use webrtc::data_channel::RTCDataChannel;
use webrtc::ice_transport::ice_candidate::{RTCIceCandidate, RTCIceCandidateInit};
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::policy::ice_transport_policy::RTCIceTransportPolicy;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;

use super::framing::{self, Bounds, Reassembler};
use super::supervisor::{Author, Supervisor, global};
use super::toolbelt::Decision;

/// The only DataChannel a viewer peer serves.
pub const CHANNEL: &str = "hive";
/// A confirmed grant with no offer by then holds a slot for nothing.
const OFFER_DEADLINE: Duration = if cfg!(test) {
    Duration::from_secs(2)
} else {
    Duration::from_secs(60)
};
/// A grant's TTL is clamped to this, whatever the server sends.
const MAX_TTL_SECS: u32 = 60 * 60;
/// Bytes queued on the channel before the device waits for the viewer.
const BACKPRESSURE_HIGH: usize = 4 * 1024 * 1024;
/// A viewer that reads nothing for this long is gone.
const SEND_STALL: Duration = Duration::from_secs(10);
/// Events per `page` answer, and per catch-up batch.
const PAGE_MAX: usize = 500;
/// Serialised event bytes per answer before it is cut (at least one event).
const PAGE_BYTES: usize = 1024 * 1024;
/// What a viewer may send: a prompt is the largest thing it has to say.
const INBOUND: Bounds = Bounds {
    max_message_bytes: 512 * 1024,
    max_in_flight: 4,
};
const MAX_PROMPT_BYTES: usize = 256 * 1024;
/// What a driver may tell the model with a denial.
const MAX_ANSWER_MESSAGE: usize = 2000;
/// A viewer's request id echoed back, capped.
const MAX_REQUEST_ID: usize = 64;

/// A view grant, as the device holds it.
#[derive(Debug, Clone)]
pub struct ViewGrant {
    pub grant_id: ObjectId,
    pub session_id: ObjectId,
    pub user_id: ObjectId,
    pub user_name: String,
    /// What the SERVER says; the device may take it away (P1c-2,
    /// [`Supervisor::drives_here`]) and never adds it.
    pub may_prompt: bool,
    /// A driver's address, for the device's own `hive_accounts` (P1c-2).
    pub user_email: Option<String>,
    /// Why THIS device does not let a viewer the server named a driver act
    /// as the session's account — set by the device's gate, never by the
    /// server; the viewer is told in `hello`.
    pub driving_refused: Option<String>,
    pub ttl_secs: u32,
}

/// What the server tells a grant's actor.
enum Cmd {
    Offer {
        sdp: String,
        ice_servers: Vec<IceServer>,
    },
    Ice(Value),
    Renew(u32),
    /// The server ended the grant; nothing is reported back.
    Close(String),
}

/// What the peer's handlers tell the actor — never the peer itself.
enum PeerEvent {
    LocalIce(Value),
    Channel(Arc<RTCDataChannel>),
    Frame(Bytes),
    ChannelClosed,
    Failed,
}

struct Viewer {
    session: ObjectId,
    cmd: mpsc::Sender<Cmd>,
}

/// The device's viewer peers, by grant.
#[derive(Default)]
pub(crate) struct Viewers {
    map: Mutex<HashMap<ObjectId, Viewer>>,
}

impl Viewers {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<ObjectId, Viewer>> {
        self.map.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Grants held now. Tests only: dead code in this module once ICEd
    /// rustc 1.95's deathness pass (see `crate::key_rotation`).
    #[cfg(test)]
    pub(crate) fn count(&self) -> usize {
        self.lock().len()
    }

    fn command(&self, grant: ObjectId, cmd: Cmd) {
        let tx = self.lock().get(&grant).map(|v| v.cmd.clone());
        match tx {
            Some(tx) => {
                if tx.try_send(cmd).is_err() {
                    warn!(%grant, "hive: a viewer's command queue is full — command dropped");
                }
            }
            None => debug!(%grant, "hive: a view frame for a grant this device does not hold"),
        }
    }

    fn remove(&self, grant: ObjectId) {
        self.lock().remove(&grant);
    }
}

/// `rc:hive.view.grant` — answered on `tx`, the connection that asked, from a
/// task of its own (it reads the store).
pub fn handle_view_grant(grant: ViewGrant, is_primary: bool, tx: mpsc::Sender<ClientMsg>) {
    tokio::spawn(async move {
        let grant_id = grant.grant_id;
        let (refused, detail) = match global() {
            Some(sup) => match sup.view_grant(grant, is_primary).await {
                Ok(()) => (None, None),
                Err((r, d)) => (Some(r), Some(d)),
            },
            None => (
                Some(HiveViewRefusal::HiveDisabled),
                Some("agent sessions are not set up on this daemon".to_string()),
            ),
        };
        let _ = tx
            .send(ClientMsg::HiveViewGrantAck {
                grant_id,
                refused,
                detail,
            })
            .await;
    });
}

/// `rc:hive.view.offer`.
pub fn handle_view_offer(
    grant_id: ObjectId,
    sdp: String,
    ice_servers: Vec<IceServer>,
    is_primary: bool,
) {
    if let Some(sup) = primary_global(is_primary) {
        sup.viewers()
            .command(grant_id, Cmd::Offer { sdp, ice_servers });
    }
}

/// `rc:hive.view.ice`.
pub fn handle_view_ice(grant_id: ObjectId, candidate: Value, is_primary: bool) {
    if let Some(sup) = primary_global(is_primary) {
        sup.viewers().command(grant_id, Cmd::Ice(candidate));
    }
}

/// `rc:hive.view.renew`.
pub fn handle_view_renew(grant_id: ObjectId, ttl_secs: u32, is_primary: bool) {
    if let Some(sup) = primary_global(is_primary) {
        sup.viewers().command(grant_id, Cmd::Renew(ttl_secs));
    }
}

/// `rc:hive.view.close`. Idempotent: a grant this device no longer holds is
/// already closed.
pub fn handle_view_close(grant_id: ObjectId, reason: String, is_primary: bool) {
    if let Some(sup) = primary_global(is_primary) {
        sup.viewers().command(grant_id, Cmd::Close(reason));
    }
}

/// View frames count only on the primary enrollment's connection — the only
/// one a grant can have come from.
fn primary_global(is_primary: bool) -> Option<Arc<Supervisor>> {
    if !is_primary {
        debug!("hive: a view frame from a secondary organization — ignored");
        return None;
    }
    global()
}

fn ttl(secs: u32) -> Duration {
    Duration::from_secs(u64::from(secs.clamp(1, MAX_TTL_SECS)))
}

impl Supervisor {
    /// The device's gates for a view grant; on success the grant's actor is
    /// running and waits for the viewer's offer.
    pub(crate) async fn view_grant(
        self: &Arc<Self>,
        grant: ViewGrant,
        is_primary: bool,
    ) -> Result<(), (HiveViewRefusal, String)> {
        if !is_primary {
            return Err((
                HiveViewRefusal::HiveDisabled,
                "agent sessions are served only to this device's primary organization".into(),
            ));
        }
        // P1j — an adopted session is served on `hive_adopt` alone: a device
        // may allow adopting with agent sessions themselves off.
        let adopted = self.adopt_allowed() && self.adopt_holds(grant.session_id);
        if !self.enabled() && !adopted {
            return Err((
                HiveViewRefusal::HiveDisabled,
                "agent sessions are off on this device (hive_enabled)".into(),
            ));
        }
        let store = self
            .store_handle()
            .map_err(|e| (HiveViewRefusal::NoSession, e))?;
        let held = self.holds_live(grant.session_id)
            || store.tip(&grant.session_id.to_hex()).await.is_some();
        if !held {
            return Err((
                HiveViewRefusal::NoSession,
                "this device holds no transcript of that session".into(),
            ));
        }
        let rx = {
            let mut map = self.viewers().lock();
            // Idempotent on the grant id: a re-sent grant is answered again.
            if map.contains_key(&grant.grant_id) {
                return Ok(());
            }
            if map.len() >= view_limits::MAX_PER_DEVICE {
                return Err((
                    HiveViewRefusal::AtCapacity,
                    format!(
                        "this device already serves {} viewers",
                        view_limits::MAX_PER_DEVICE
                    ),
                ));
            }
            let on_session = map
                .values()
                .filter(|v| v.session == grant.session_id)
                .count();
            if on_session >= view_limits::MAX_PER_SESSION {
                return Err((
                    HiveViewRefusal::AtCapacity,
                    format!(
                        "this session already has {} viewers",
                        view_limits::MAX_PER_SESSION
                    ),
                ));
            }
            let (tx, rx) = mpsc::channel(64);
            map.insert(
                grant.grant_id,
                Viewer {
                    session: grant.session_id,
                    cmd: tx,
                },
            );
            rx
        };
        // P1c-2 — the server names drivers; THIS device decides whom it lets
        // act as one of its accounts.
        let mut grant = grant;
        if grant.may_prompt
            && let Err(why) =
                self.drives_here(grant.session_id, grant.user_id, grant.user_email.as_deref())
        {
            info!(
                grant = %grant.grant_id, session = %grant.session_id, user = %grant.user_id,
                %why, "hive: a driver the server named may not drive here — read only"
            );
            grant.may_prompt = false;
            grant.driving_refused = Some(why);
        }
        info!(
            grant = %grant.grant_id, session = %grant.session_id, user = %grant.user_id,
            may_prompt = grant.may_prompt, "hive: view granted"
        );
        tokio::spawn(run(Arc::clone(self), grant, store, rx));
        Ok(())
    }
}

/// How a grant's actor ended.
enum End {
    /// The server closed it — it knows; nothing is reported.
    Server(String),
    /// The device ended it — the server is told why.
    Device(String),
}

/// The grant's actor: owns the peer from offer to close.
async fn run(
    sup: Arc<Supervisor>,
    grant: ViewGrant,
    store: super::store::StoreHandle,
    mut cmds: mpsc::Receiver<Cmd>,
) {
    let gid = grant.grant_id;
    let sid = grant.session_id.to_hex();
    let mut deadline = Instant::now() + ttl(grant.ttl_secs);
    let offer_by = Instant::now() + OFFER_DEADLINE;
    let (events_tx, mut events) = mpsc::channel::<PeerEvent>(256);
    let mut peer: Option<Arc<RTCPeerConnection>> = None;
    let mut channel: Option<Channel> = None;
    // `Some` while the viewer follows the live session: the feed, and the
    // newest `seq` it has been sent.
    let mut follow: Option<(broadcast::Receiver<Arc<EventEnvelope>>, u64)> = None;
    let mut states = sup.subscribe_states();
    let mut approvals = sup.subscribe_approvals();

    let end = loop {
        tokio::select! {
            cmd = cmds.recv() => match cmd {
                None => break End::Server("dropped".into()),
                Some(Cmd::Close(reason)) => break End::Server(reason),
                Some(Cmd::Renew(secs)) => deadline = Instant::now() + ttl(secs),
                Some(Cmd::Offer { sdp, ice_servers }) => {
                    if peer.is_some() {
                        debug!(grant = %gid, "hive: a second offer for one grant — ignored");
                        continue;
                    }
                    match answer(&sdp, &ice_servers, events_tx.clone()).await {
                        Ok((pc, answer_sdp)) => {
                            peer = Some(pc);
                            // Before any local candidate: they queue on
                            // `events` behind this and are sent after it.
                            sup.send(ClientMsg::HiveViewAnswer { grant_id: gid, sdp: answer_sdp });
                        }
                        Err(e) => break End::Device(format!("offer_refused: {e}")),
                    }
                }
                Some(Cmd::Ice(candidate)) => match &peer {
                    Some(pc) => add_remote_ice(gid, pc, candidate).await,
                    None => debug!(grant = %gid, "hive: a viewer candidate before its offer — dropped"),
                },
            },
            _ = tokio::time::sleep_until(deadline) => break End::Device("expired".into()),
            _ = tokio::time::sleep_until(offer_by), if peer.is_none() => {
                break End::Device("no_offer".into());
            }
            ev = events.recv() => match ev {
                // The actor holds a sender: never closed while it runs.
                None => break End::Device("internal".into()),
                Some(PeerEvent::LocalIce(candidate)) => {
                    sup.send(ClientMsg::HiveViewIce { grant_id: gid, candidate });
                }
                Some(PeerEvent::Channel(dc)) => {
                    if channel.is_some() || dc.label() != CHANNEL {
                        debug!(grant = %gid, label = dc.label(), "hive: an extra data channel — closed");
                        let _ = dc.close().await;
                        continue;
                    }
                    channel = Some(Channel::attach(dc));
                }
                Some(PeerEvent::Frame(frame)) => {
                    let Some(ch) = channel.as_mut() else { continue };
                    let message = match ch.reasm.push(&frame) {
                        Ok(Some(m)) => m,
                        Ok(None) => continue,
                        Err(e) => {
                            debug!(grant = %gid, ?e, "hive: a viewer frame was dropped");
                            continue;
                        }
                    };
                    if let Err(e) = on_request(&sup, &grant, &sid, &store, ch, &mut follow, &message).await {
                        break End::Device(e);
                    }
                }
                Some(PeerEvent::ChannelClosed) => break End::Device("viewer_left".into()),
                Some(PeerEvent::Failed) => break End::Device("peer_failed".into()),
            },
            got = next_feed(&mut follow), if follow.is_some() => {
                let Some(ch) = channel.as_mut() else { continue };
                if let Err(e) = on_feed(&store, &sid, ch, &mut follow, got).await {
                    break End::Device(e);
                }
            }
            st = states.recv() => {
                if let Ok((session, state)) = st
                    && session == grant.session_id
                    && let Some(ch) = channel.as_mut()
                    && let Err(e) = ch.send(&json!({"op": "state", "state": state.as_str()})).await
                {
                    break End::Device(e);
                }
            }
            // P1a — which approvals are open: a card is answerable only
            // while its id is in the latest list.
            open = approvals.recv() => {
                let pending = match open {
                    Ok((session, ids)) if session == grant.session_id => Some(ids),
                    Ok(_) => None,
                    // Behind: say what is open NOW rather than nothing.
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        Some(sup.pending_approvals(grant.session_id))
                    }
                    Err(broadcast::error::RecvError::Closed) => None,
                };
                if let Some(ids) = pending
                    && let Some(ch) = channel.as_mut()
                    && let Err(e) = ch.send(&json!({"op": "approvals", "pending": ids})).await
                {
                    break End::Device(e);
                }
            }
        }
    };

    // The one place a viewer peer is torn down.
    sup.viewers().remove(gid);
    drop(channel);
    if let Some(pc) = peer
        && let Err(e) = pc.close().await
    {
        debug!(grant = %gid, %e, "hive: viewer peer close errored");
    }
    match end {
        End::Server(reason) => info!(grant = %gid, %reason, "hive: view closed by the server"),
        End::Device(reason) => {
            info!(grant = %gid, %reason, "hive: view closed");
            sup.send(ClientMsg::HiveViewClosed {
                grant_id: gid,
                reason,
            });
        }
    }
}

/// Build the peer, take the viewer's offer, and return the answer. The peer
/// is configured as remote control's browser-facing one is: the overlay
/// interface kept out of ICE, and `ICE_RELAY_TCP` honoured.
async fn answer(
    offer_sdp: &str,
    ice_servers: &[IceServer],
    events: mpsc::Sender<PeerEvent>,
) -> Result<(Arc<RTCPeerConnection>, String), String> {
    let mut setting = SettingEngine::default();
    setting.set_interface_filter(Box::new(|name: &str| !crate::peer::is_overlay_iface(name)));
    let api = APIBuilder::new().with_setting_engine(setting).build();
    let relay_tcp = tunnel_core::env::node_env("ICE_RELAY_TCP")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    let mut config = RTCConfiguration {
        ice_servers: if relay_tcp {
            crate::peer::map_ice_servers_relay_tcp(ice_servers)
        } else {
            crate::peer::map_ice_servers(ice_servers)
        },
        ..Default::default()
    };
    if relay_tcp {
        config.ice_transport_policy = RTCIceTransportPolicy::Relay;
    }
    let pc = Arc::new(
        api.new_peer_connection(config)
            .await
            .map_err(|e| format!("new peer: {e}"))?,
    );

    {
        let ev = events.clone();
        pc.on_ice_candidate(Box::new(move |c: Option<RTCIceCandidate>| {
            let ev = ev.clone();
            Box::pin(async move {
                let Some(c) = c else { return };
                let Ok(init) = c.to_json() else { return };
                if let Ok(v) = serde_json::to_value(&init) {
                    let _ = ev.send(PeerEvent::LocalIce(v)).await;
                }
            })
        }));
    }
    {
        let ev = events.clone();
        pc.on_peer_connection_state_change(Box::new(move |s: RTCPeerConnectionState| {
            let ev = ev.clone();
            Box::pin(async move {
                // Not `Disconnected`: five seconds of silence a roam or a
                // relay hiccup recovers from on its own.
                if s == RTCPeerConnectionState::Failed {
                    let _ = ev.send(PeerEvent::Failed).await;
                }
            })
        }));
    }
    {
        let ev = events;
        let claimed = Arc::new(AtomicBool::new(false));
        pc.on_data_channel(Box::new(move |dc: Arc<RTCDataChannel>| {
            // The handlers go on HERE, not when the actor gets to the
            // channel: webrtc-rs awaits this callback and only then starts
            // the channel's read loop, which drops a frame that arrives with
            // no handler set — and the browser sends `hello` the moment its
            // side opens. Field, 2026-10-07: a viewer whose `hello` was lost
            // never asked for the history, and a whole turn never reached
            // the panel. Only the first `hive` channel is wired; the actor
            // closes any other.
            if dc.label() == CHANNEL && !claimed.swap(true, AtomicOrdering::AcqRel) {
                Channel::wire(&dc, ev.clone());
            }
            let ev = ev.clone();
            Box::pin(async move {
                let _ = ev.send(PeerEvent::Channel(dc)).await;
            })
        }));
    }

    let negotiated = async {
        let offer = RTCSessionDescription::offer(offer_sdp.to_string())
            .map_err(|e| format!("offer: {e}"))?;
        pc.set_remote_description(offer)
            .await
            .map_err(|e| format!("set remote: {e}"))?;
        let answer = pc
            .create_answer(None)
            .await
            .map_err(|e| format!("answer: {e}"))?;
        pc.set_local_description(answer.clone())
            .await
            .map_err(|e| format!("set local: {e}"))?;
        Ok::<_, String>(answer.sdp)
    }
    .await;
    match negotiated {
        Ok(sdp) => Ok((pc, sdp)),
        Err(e) => {
            // Built but unusable: close it, or its sockets outlive it.
            let _ = pc.close().await;
            Err(e)
        }
    }
}

/// A viewer's candidate. A browser hides its LAN address behind an mDNS
/// `.local` name, which webrtc-ice resolves unreliably; resolve it with the
/// OS resolver as remote control does, off this task.
async fn add_remote_ice(gid: ObjectId, pc: &Arc<RTCPeerConnection>, candidate: Value) {
    let init = match candidate {
        Value::String(s) => RTCIceCandidateInit {
            candidate: s,
            ..Default::default()
        },
        other => match serde_json::from_value::<RTCIceCandidateInit>(other) {
            Ok(i) => i,
            Err(e) => {
                debug!(grant = %gid, %e, "hive: a viewer candidate of the wrong shape");
                return;
            }
        },
    };
    if crate::mdns_resolve::candidate_mdns_name(&init.candidate).is_some() {
        let pc = Arc::clone(pc);
        let mut init = init;
        tokio::spawn(async move {
            if let Some(rewritten) =
                crate::mdns_resolve::resolve_mdns_candidate(&init.candidate).await
            {
                init.candidate = rewritten;
            }
            let _ = pc.add_ice_candidate(init).await;
        });
        return;
    }
    if let Err(e) = pc.add_ice_candidate(init).await {
        debug!(grant = %gid, %e, "hive: a viewer candidate was not added");
    }
}

/// The `hive` channel and what the viewer said on it so far.
struct Channel {
    dc: Arc<RTCDataChannel>,
    next_id: u32,
    reasm: Reassembler,
}

impl Channel {
    /// Send the channel's frames and its close to the actor. Called from
    /// `on_data_channel` itself, before webrtc-rs starts reading the channel.
    fn wire(dc: &Arc<RTCDataChannel>, events: mpsc::Sender<PeerEvent>) {
        {
            let ev = events.clone();
            dc.on_message(Box::new(move |msg| {
                let ev = ev.clone();
                Box::pin(async move {
                    // Awaited, not dropped: a lost frame corrupts its message.
                    let _ = ev.send(PeerEvent::Frame(msg.data)).await;
                })
            }));
        }
        dc.on_close(Box::new(move || {
            let ev = events.clone();
            Box::pin(async move {
                let _ = ev.send(PeerEvent::ChannelClosed).await;
            })
        }));
    }

    /// Take the channel [`Self::wire`] already pointed at the actor.
    fn attach(dc: Arc<RTCDataChannel>) -> Self {
        Self {
            dc,
            next_id: 0,
            reasm: Reassembler::new(INBOUND),
        }
    }

    /// Send one message, waiting while the viewer is behind — but not for
    /// longer than [`SEND_STALL`].
    async fn send(&mut self, message: &Value) -> Result<(), String> {
        let bytes = serde_json::to_vec(message).map_err(|e| format!("encode: {e}"))?;
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        for frame in framing::encode(id, &bytes) {
            let stalled_at = Instant::now() + SEND_STALL;
            while self.dc.buffered_amount().await > BACKPRESSURE_HIGH {
                if Instant::now() >= stalled_at {
                    return Err("viewer_stalled".into());
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            self.dc
                .send(&Bytes::from(frame))
                .await
                .map_err(|_| "viewer_left".to_string())?;
        }
        Ok(())
    }
}

/// One event as the viewer gets it.
fn event_json(env: &EventEnvelope) -> Value {
    let event = serde_json::from_str::<Value>(&env.event_json)
        .unwrap_or_else(|_| json!({"kind": "unreadable"}));
    json!({"seq": env.seq, "ts": env.ts_ms, "fence": env.fence, "event": event})
}

/// Events as one answer: at most [`PAGE_BYTES`] of them (always at least
/// one), and whether the cut left any out.
fn batch(envs: &[EventEnvelope]) -> (Vec<Value>, bool) {
    let mut out = Vec::new();
    let mut bytes = 0;
    for env in envs {
        if !out.is_empty() && bytes + env.event_json.len() > PAGE_BYTES {
            return (out, true);
        }
        bytes += env.event_json.len();
        out.push(event_json(env));
    }
    (out, false)
}

/// One request from the viewer.
async fn on_request(
    sup: &Arc<Supervisor>,
    grant: &ViewGrant,
    sid: &str,
    store: &super::store::StoreHandle,
    ch: &mut Channel,
    follow: &mut Option<(broadcast::Receiver<Arc<EventEnvelope>>, u64)>,
    message: &[u8],
) -> Result<(), String> {
    let Ok(req) = serde_json::from_slice::<Value>(message) else {
        return ch.send(&json!({"op": "error", "error": "not JSON"})).await;
    };
    match req.get("op").and_then(Value::as_str).unwrap_or("") {
        "hello" => {
            let tip = store.tip(sid).await.unwrap_or(0);
            let state = sup.run_state(grant.session_id).map(HiveRunState::as_str);
            ch.send(&json!({
                "op": "hello",
                "v": 1,
                "session": sid,
                "tip": tip,
                "live": sup.holds_live(grant.session_id),
                "state": state,
                "may_prompt": grant.may_prompt,
                // A driver is who answers (design §4.5): the grant that may
                // prompt is the one that may answer.
                "may_answer": grant.may_prompt,
                // P1c-2 — why this device keeps a driver the server named
                // read only; null when it does not.
                "driving_refused": grant.driving_refused,
                "approvals": sup.pending_approvals(grant.session_id),
            }))
            .await
        }
        "page" => {
            let after = req.get("after").and_then(Value::as_u64).unwrap_or(0);
            let limit = req
                .get("limit")
                .and_then(Value::as_u64)
                .map_or(PAGE_MAX, |l| (l as usize).clamp(1, PAGE_MAX));
            let envs = store.page(sid, after, limit).await?;
            let full = envs.len() == limit;
            let (events, cut) = batch(&envs);
            let tip = store.tip(sid).await.unwrap_or(0);
            ch.send(&json!({
                "op": "page",
                "after": after,
                "events": events,
                "more": full || cut,
                "tip": tip,
            }))
            .await
        }
        "follow" => {
            let after = req.get("after").and_then(Value::as_u64).unwrap_or(0);
            // Subscribe FIRST, then catch up from the store: an event that
            // lands in between is in one or the other, and the cursor drops
            // whatever is in both.
            *follow = Some((store.subscribe(), after));
            catch_up(store, sid, ch, follow).await
        }
        "unfollow" => {
            *follow = None;
            Ok(())
        }
        "prompt" => {
            let id: String = req
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .chars()
                .take(MAX_REQUEST_ID)
                .collect();
            let text = req.get("text").and_then(Value::as_str).unwrap_or("");
            let refused = if !grant.may_prompt {
                Some("read_only: this view may read the session, not prompt it".to_string())
            } else if text.trim().is_empty() {
                Some("empty prompt".to_string())
            } else if text.len() > MAX_PROMPT_BYTES {
                Some(format!("a prompt is at most {MAX_PROMPT_BYTES} bytes"))
            } else {
                let author = Author {
                    user_id: grant.user_id,
                    name: grant.user_name.clone(),
                };
                sup.prompt(grant.session_id, Some(author), text.to_string())
                    .err()
            };
            match refused {
                None => {
                    ch.send(&json!({"op": "prompt", "id": id, "ok": true}))
                        .await
                }
                Some(error) => {
                    ch.send(&json!({"op": "prompt", "id": id, "ok": false, "error": error}))
                        .await
                }
            }
        }
        // P1a — a driver's answer to an open approval.
        "answer" => {
            let id: String = req
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .chars()
                .take(MAX_REQUEST_ID)
                .collect();
            let approval: String = req
                .get("approval")
                .and_then(Value::as_str)
                .unwrap_or("")
                .chars()
                .take(MAX_REQUEST_ID)
                .collect();
            let message = req
                .get("message")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|m| !m.is_empty())
                .map(|m| m.chars().take(MAX_ANSWER_MESSAGE).collect::<String>());
            let decision = match req.get("decision").and_then(Value::as_str) {
                Some("allow") => Some(Decision::Allow),
                Some("deny") => Some(Decision::Deny { message }),
                _ => None,
            };
            let refused = if !grant.may_prompt {
                Some("read_only: only a driver answers an approval".to_string())
            } else {
                match decision {
                    None => Some("a decision is allow or deny".to_string()),
                    Some(decision) => {
                        let by = Author {
                            user_id: grant.user_id,
                            name: grant.user_name.clone(),
                        };
                        sup.answer_approval(grant.session_id, &approval, decision, by)
                            .err()
                    }
                }
            };
            match refused {
                None => {
                    ch.send(&json!({"op": "answer", "id": id, "ok": true}))
                        .await
                }
                Some(error) => {
                    ch.send(&json!({"op": "answer", "id": id, "ok": false, "error": error}))
                        .await
                }
            }
        }
        other => {
            let other = other.chars().take(32).collect::<String>();
            ch.send(&json!({"op": "error", "error": format!("unknown op {other:?}")}))
                .await
        }
    }
}

/// Send what the store holds past the follow cursor, in batches.
async fn catch_up(
    store: &super::store::StoreHandle,
    sid: &str,
    ch: &mut Channel,
    follow: &mut Option<(broadcast::Receiver<Arc<EventEnvelope>>, u64)>,
) -> Result<(), String> {
    loop {
        let Some((_, cursor)) = follow.as_ref() else {
            return Ok(());
        };
        let envs = store.page(sid, *cursor, PAGE_MAX).await?;
        if envs.is_empty() {
            return Ok(());
        }
        let (events, cut) = batch(&envs);
        let sent = if cut { events.len() } else { envs.len() };
        ch.send(&json!({"op": "events", "events": events})).await?;
        if let Some((_, cursor)) = follow.as_mut() {
            *cursor = envs[sent - 1].seq;
        }
        if !cut && envs.len() < PAGE_MAX {
            return Ok(());
        }
    }
}

/// The next appended event of any session, or that the feed lagged.
async fn next_feed(
    follow: &mut Option<(broadcast::Receiver<Arc<EventEnvelope>>, u64)>,
) -> Result<Arc<EventEnvelope>, broadcast::error::RecvError> {
    match follow {
        Some((rx, _)) => rx.recv().await,
        None => std::future::pending().await,
    }
}

/// One item from the live feed: this session's next event is sent; a gap —
/// the feed lagged behind, or an event arrived out of step — is filled from
/// the store first.
async fn on_feed(
    store: &super::store::StoreHandle,
    sid: &str,
    ch: &mut Channel,
    follow: &mut Option<(broadcast::Receiver<Arc<EventEnvelope>>, u64)>,
    got: Result<Arc<EventEnvelope>, broadcast::error::RecvError>,
) -> Result<(), String> {
    match got {
        Ok(env) if env.session == sid => {
            let cursor = follow.as_ref().map_or(0, |(_, c)| *c);
            if env.seq <= cursor {
                return Ok(());
            }
            if env.seq > cursor + 1 {
                return catch_up(store, sid, ch, follow).await;
            }
            ch.send(&json!({"op": "events", "events": [event_json(&env)]}))
                .await?;
            if let Some((_, c)) = follow.as_mut() {
                *c = env.seq;
            }
            Ok(())
        }
        Ok(_) => Ok(()),
        Err(broadcast::error::RecvError::Lagged(n)) => {
            debug!(
                lagged = n,
                "hive: a viewer's feed lagged — re-reading the store"
            );
            catch_up(store, sid, ch, follow).await
        }
        Err(broadcast::error::RecvError::Closed) => {
            *follow = None;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests;
