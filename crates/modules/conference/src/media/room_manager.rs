// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
use bson::oid::ObjectId;
use dashmap::DashMap;
use mediasoup::prelude::*;
use mediasoup::types::data_structures::DtlsState;
use mediasoup::webrtc_transport::{
    WebRtcTransportListenInfos, WebRtcTransportOptions, WebRtcTransportRemoteParameters,
};
use roomler_ai_config::MediasoupSettings;
use serde::{Deserialize, Serialize};
use std::net::IpAddr;
use std::num::NonZero;
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::view::{self, ViewState};
use super::worker_pool::WorkerPool;

/// Holds the DirectTransport + Consumer for an RTP tap (transcription).
struct RtpTap {
    _direct_transport: DirectTransport,
    _consumer: Consumer,
}

/// A media room backed by a mediasoup Router.
pub struct MediaRoom {
    pub router: Router,
    /// Keyed by connection_id (UUID per WebSocket connection) so the same user
    /// can join from multiple tabs/devices without overwriting state.
    pub participants: DashMap<String, ParticipantMedia>,
    /// RTP taps for transcription, keyed by producer_id string.
    rtp_taps: DashMap<String, RtpTap>,
}

/// A producer with its source label (e.g. "camera", "screen", "audio").
pub struct ProducerEntry {
    pub producer: Producer,
    pub source: String,
}

/// Media state for a single participant (one WebSocket connection).
pub struct ParticipantMedia {
    pub user_id: ObjectId,
    pub send_transport: WebRtcTransport,
    pub recv_transport: WebRtcTransport,
    pub producers: Vec<ProducerEntry>,
    pub consumers: Vec<Consumer>,
    /// FR-127 P1 — what this viewer renders, from its `media:view` reports.
    /// ⚠️ `None` is a legacy client that never declared `caps: ["view"]`:
    /// every consumer flows, as before. `Some(empty)` is a view client that
    /// shows nothing: every video consumer is paused. Never collapse them
    /// (`media/view.rs`).
    pub view: Option<ViewState>,
    /// FR-127 P1.1 — orders this participant's operations that span a worker
    /// round trip and read or write its view: `consume` decides a consumer's
    /// flag before creating it and records it after, `apply_view` plans every
    /// consumer's flag and then sets them. A report applied in between would
    /// be lost to the consumer being created, so each holds this for its whole
    /// run. It is an async mutex, so a waiter yields its thread, and it also
    /// names the participant: a re-join on the same connection gets a new one,
    /// and an operation that finds another one recorded stops there.
    ///
    /// ⚠️ Never a DashMap guard across an await instead (see `tests` below).
    ops: Arc<tokio::sync::Mutex<()>>,
}

/// FR-127 P1 — what `RoomManager::apply_view` did with a report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewOutcome {
    /// The report is now the participant's view; this many consumers changed.
    Applied { resumed: usize, paused: usize },
    /// Older than the report held: dropped, nothing changed.
    Stale,
    /// The participant never declared `caps: ["view"]`: ignored, everything
    /// keeps flowing.
    Legacy,
    /// No such room or participant.
    NotFound,
}

/// FR-127 P1 — one consumer's flags as plain data, for the tests and the
/// gauges: `paused` is the VIEWER's own flag (`Consumer::paused`),
/// `producer_paused` the OWNER's (FR-30, `Consumer::producer_paused`). Two
/// fields because they are two states inside the worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsumerSnapshot {
    pub producer_id: String,
    pub kind: &'static str,
    pub paused: bool,
    pub producer_paused: bool,
}

/// FR-127 P1 — one producer's flags as plain data; `paused` is the owner's
/// FR-30 pause.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProducerSnapshot {
    pub producer_id: String,
    pub kind: &'static str,
    pub source: String,
    pub paused: bool,
}

fn kind_str(kind: MediaKind) -> &'static str {
    match kind {
        MediaKind::Audio => "audio",
        MediaKind::Video => "video",
    }
}

/// Transport connection details sent to the client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransportOptions {
    pub id: String,
    pub ice_parameters: serde_json::Value,
    pub ice_candidates: serde_json::Value,
    pub dtls_parameters: serde_json::Value,
}

/// Pair of transport options (send + recv).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransportPair {
    pub send_transport: TransportOptions,
    pub recv_transport: TransportOptions,
}

/// Consumer details sent to the client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsumerInfo {
    pub id: String,
    pub producer_id: String,
    pub kind: String,
    pub rtp_parameters: serde_json::Value,
    /// FR-30 P4 — is the SOURCE producer paused right now?
    ///
    /// The pause EVENT only reaches whoever was in the room when it happened.
    /// Without this, someone who joins a call after you muted sees no badge
    /// until your next toggle — measured in the field 2026-08-29. The state
    /// has to travel with the subscription, not only as a transition.
    #[serde(default)]
    pub producer_paused: bool,
}

/// Manages mediasoup rooms and their media state.
pub struct RoomManager {
    rooms: DashMap<ObjectId, MediaRoom>,
    /// Tracks which room each connection is in (connection_id -> room_id).
    connection_rooms: DashMap<String, ObjectId>,
    worker_pool: Arc<WorkerPool>,
    listen_ip: IpAddr,
    announced_ip: Option<String>,
}

/// Stats PR-2 — one participant's cloned transport handles:
/// `(user_id, send_transport, recv_transport)`.
pub type ParticipantTransports = (ObjectId, WebRtcTransport, WebRtcTransport);
/// One room's sampled topology: `(room_id, participants)`.
pub type RoomTransportSnapshot = (ObjectId, Vec<ParticipantTransports>);

impl RoomManager {
    /// Stats PR-2 — snapshot the live conference topology for the media
    /// sampler: per room, per participant, CLONED transport handles
    /// (cheap `Arc` clones). The clones are taken under the DashMap shard
    /// guards and the guards are dropped before the caller awaits
    /// `get_stats()` — holding a guard across an await can deadlock
    /// against join/leave writers on the same shard.
    pub fn sample_transports(&self) -> Vec<RoomTransportSnapshot> {
        self.rooms
            .iter()
            .map(|room| {
                let parts = room
                    .participants
                    .iter()
                    .map(|p| {
                        (
                            p.user_id,
                            p.send_transport.clone(),
                            p.recv_transport.clone(),
                        )
                    })
                    .collect();
                (*room.key(), parts)
            })
            .collect()
    }

    pub fn new(worker_pool: Arc<WorkerPool>, settings: &MediasoupSettings) -> Self {
        let listen_ip: IpAddr = settings
            .listen_ip
            .parse()
            .unwrap_or_else(|_| "0.0.0.0".parse().unwrap());

        // S6 — multi-pod: each pod may need a different public announced
        // IP (it rides its node's DNAT). `ROOMLER__POD_HOST_IP` is
        // injected via the Downward API (status.hostIP); single-pod
        // deployments without the map behave exactly as before.
        let pod_host_ip = std::env::var("ROOMLER__POD_HOST_IP").ok();
        let announced_ip = settings.resolve_announced_ip(pod_host_ip.as_deref());
        if let Some(ip) = &announced_ip {
            tracing::info!(announced_ip = %ip, pod_host_ip = ?pod_host_ip, "mediasoup announced IP resolved");
        }

        Self {
            rooms: DashMap::new(),
            connection_rooms: DashMap::new(),
            worker_pool,
            listen_ip,
            announced_ip,
        }
    }

    /// Creates a mediasoup Router for a room and stores it.
    /// Returns the router's RTP capabilities (serialized).
    pub async fn create_room(&self, room_id: ObjectId) -> anyhow::Result<serde_json::Value> {
        let existing = self
            .rooms
            .get(&room_id)
            .map(|room| room.router.rtp_capabilities().clone());
        if let Some(caps) = existing {
            return Ok(serde_json::to_value(caps)?);
        }

        let worker = self.worker_pool.get_worker();

        let media_codecs = media_codecs();
        let router_options = RouterOptions::new(media_codecs);
        let router = worker
            .create_router(router_options)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to create router: {}", e))?;

        // FR-127 P1.1 — two joins can both find no room above and both build a
        // router across the await. The first to get here keeps the room; the
        // other answers with that room's capabilities and drops its router
        // unused. An `insert` would replace a room that may already hold the
        // first joiner's transports.
        let caps = match self.rooms.entry(room_id) {
            dashmap::mapref::entry::Entry::Occupied(room) => {
                room.get().router.rtp_capabilities().clone()
            }
            dashmap::mapref::entry::Entry::Vacant(slot) => {
                let caps = router.rtp_capabilities().clone();
                slot.insert(MediaRoom {
                    router,
                    participants: DashMap::new(),
                    rtp_taps: DashMap::new(),
                });
                info!(?room_id, "mediasoup room created");
                caps
            }
        };

        Ok(serde_json::to_value(caps)?)
    }

    /// Removes a room and all its media state.
    pub fn remove_room(&self, room_id: &ObjectId) -> bool {
        if let Some((_, room)) = self.rooms.remove(room_id) {
            // Clean up connection_rooms mappings
            let conn_ids: Vec<String> = room
                .participants
                .iter()
                .map(|entry| entry.key().clone())
                .collect();
            for cid in conn_ids {
                self.connection_rooms.remove(&cid);
            }
            // Dropping the room closes the router and all transports/producers/consumers
            info!(?room_id, "mediasoup room removed");
            true
        } else {
            false
        }
    }

    pub fn has_room(&self, room_id: &ObjectId) -> bool {
        self.rooms.contains_key(room_id)
    }

    pub fn room_count(&self) -> usize {
        self.rooms.len()
    }

    /// Returns a reference to the rooms DashMap (for WS handler to read router capabilities).
    ///
    /// FR-127 P1.1 — crate-private, so every caller sits behind this crate's
    /// await gate (`clippy.toml`). ⚠️ The gate sees only a BARE guard held across
    /// an await; it misses one inside an `Option`, a tuple or an iterator
    /// adapter. Read what you need into owned values in a block that ends
    /// before the await.
    pub(crate) fn rooms_ref(&self) -> &DashMap<ObjectId, MediaRoom> {
        &self.rooms
    }

    /// Creates send + recv WebRtcTransport pair for a participant.
    ///
    /// FR-127 P1 — `view_capable`: the client declared `caps: ["view"]` on
    /// its join (and the lever is on), so it starts with an empty view and its
    /// video consumers are created paused until a `media:view` names them. A
    /// legacy client starts with no view at all, and everything flows.
    pub async fn create_transports(
        &self,
        room_id: ObjectId,
        user_id: ObjectId,
        connection_id: String,
        view_capable: bool,
    ) -> anyhow::Result<TransportPair> {
        // FR-127 P1.1 — the router is a cheap handle: clone it and release the
        // rooms guard before the two worker round trips (see `tests`).
        let router = self
            .rooms
            .get(&room_id)
            .map(|room| room.router.clone())
            .ok_or_else(|| anyhow::anyhow!("Room not found"))?;

        let send_transport = self.create_webrtc_transport(&router).await?;
        let recv_transport = self.create_webrtc_transport(&router).await?;

        let send_opts = transport_to_options(&send_transport);
        let recv_opts = transport_to_options(&recv_transport);

        {
            // The room may have closed meanwhile, or closed and been created
            // again with another router (a fold, then a rejoin): transports
            // made on the old router are dropped here, which closes them.
            let room = self
                .rooms
                .get(&room_id)
                .filter(|room| room.router.id() == router.id())
                .ok_or_else(|| anyhow::anyhow!("Room not found"))?;
            room.participants.insert(
                connection_id.clone(),
                ParticipantMedia {
                    user_id,
                    send_transport,
                    recv_transport,
                    producers: Vec::new(),
                    consumers: Vec::new(),
                    view: view_capable.then(ViewState::default),
                    ops: Arc::default(),
                },
            );
            // Still under the room's guard, so a `remove_room` cannot run
            // between the two inserts and leave this mapping behind.
            self.connection_rooms.insert(connection_id.clone(), room_id);
        }

        debug!(?room_id, ?user_id, %connection_id, "transports created");

        Ok(TransportPair {
            send_transport: send_opts,
            recv_transport: recv_opts,
        })
    }

    /// Connects a transport with remote DTLS parameters.
    pub async fn connect_transport(
        &self,
        room_id: &ObjectId,
        connection_id: &str,
        transport_id: &str,
        dtls_parameters: DtlsParameters,
    ) -> anyhow::Result<()> {
        // Connect on a cloned handle with the shard guards released — holding
        // a DashMap guard across an await can deadlock against join/leave
        // writers on the same shard (see `tests`). FR-127 P1.1 — the guards
        // live in this block rather than being `drop`ped: a local that was
        // borrowed stays in the future's state until its scope ends, so the
        // await gate (`clippy.toml`) cannot see an explicit drop.
        let transport = {
            let room = self
                .rooms
                .get(room_id)
                .ok_or_else(|| anyhow::anyhow!("Room not found"))?;

            let participant = room
                .participants
                .get(connection_id)
                .ok_or_else(|| anyhow::anyhow!("Participant not found"))?;

            let tid = TransportId::from_str(transport_id)
                .map_err(|e| anyhow::anyhow!("Invalid transport_id: {}", e))?;

            if participant.send_transport.id() == tid {
                participant.send_transport.clone()
            } else if participant.recv_transport.id() == tid {
                participant.recv_transport.clone()
            } else {
                return Err(anyhow::anyhow!("Transport not found for this participant"));
            }
        };

        let remote_params = WebRtcTransportRemoteParameters { dtls_parameters };

        transport
            .connect(remote_params)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to connect transport: {}", e))?;

        debug!(?room_id, %connection_id, transport_id, "transport connected");
        self.spawn_media_path_watchdog(*room_id, connection_id.to_string(), &transport);
        Ok(())
    }

    /// `connect_transport` only records the client's DTLS parameters — it says
    /// nothing about packets. When the announced-IP path is dead (host DNAT /
    /// firewall / RTC port range), every signalling step still succeeds and the
    /// only symptom is black video, which is exactly how the missing RTC-range
    /// DNAT on one node shipped broken and stayed invisible for weeks. This
    /// watchdog turns that state into an operator-actionable log line: if DTLS
    /// still hasn't established shortly after the client said it was dialing,
    /// the media path — not the client — is the suspect.
    fn spawn_media_path_watchdog(
        &self,
        room_id: ObjectId,
        connection_id: String,
        transport: &WebRtcTransport,
    ) {
        const DTLS_ESTABLISH_DEADLINE: std::time::Duration = std::time::Duration::from_secs(15);
        // Weak handle: the watchdog must not extend the transport's lifetime
        // past the participant's leave.
        let weak = transport.downgrade();
        let announced_ip = self.announced_ip.clone();
        tokio::spawn(async move {
            tokio::time::sleep(DTLS_ESTABLISH_DEADLINE).await;
            let Some(transport) = weak.upgrade() else {
                return; // participant already left — nothing to report
            };
            let dtls = transport.dtls_state();
            if matches!(dtls, DtlsState::Connected | DtlsState::Closed) {
                return;
            }
            tracing::warn!(
                ?room_id,
                %connection_id,
                transport_id = %transport.id(),
                ?dtls,
                ice = ?transport.ice_state(),
                announced_ip = ?announced_ip,
                "media transport has no DTLS {}s after connect_transport — signalling is healthy but RTP cannot flow; check the announced-IP path (host DNAT / firewall / RTC port range) toward this pod",
                DTLS_ESTABLISH_DEADLINE.as_secs()
            );
        });
    }

    /// The announced IP this pod resolved for its ICE candidates (S6 per-node
    /// map), which is what clients actually dial — the static settings value
    /// can differ on a multi-pod deployment.
    pub fn announced_ip(&self) -> Option<&str> {
        self.announced_ip.as_deref()
    }

    /// Creates a Producer on the participant's send transport.
    pub async fn produce(
        &self,
        room_id: &ObjectId,
        connection_id: &str,
        kind: MediaKind,
        rtp_parameters: RtpParameters,
        source: String,
    ) -> anyhow::Result<ProducerId> {
        // FR-127 P1.1 — produce on a cloned handle with every guard released,
        // then record the producer under a short guard (see `tests`).
        let (send_transport, ops) = {
            let room = self
                .rooms
                .get(room_id)
                .ok_or_else(|| anyhow::anyhow!("Room not found"))?;
            let participant = room
                .participants
                .get(connection_id)
                .ok_or_else(|| anyhow::anyhow!("Participant not found"))?;
            (participant.send_transport.clone(), participant.ops.clone())
        };

        let producer_options = ProducerOptions::new(kind, rtp_parameters);
        let producer = send_transport
            .produce(producer_options)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to produce: {}", e))?;

        let producer_id = producer.id();
        // The participant may have left meanwhile, or left and joined again
        // on this connection. A producer of the old one is dropped
        // unrecorded, which closes it.
        let recorded = self
            .participant_mut(room_id, connection_id, |p| {
                if !Arc::ptr_eq(&p.ops, &ops) {
                    return false;
                }
                p.producers.push(ProducerEntry {
                    producer,
                    source: source.clone(),
                });
                true
            })
            .unwrap_or(false);
        if !recorded {
            return Err(anyhow::anyhow!("Participant not found"));
        }

        debug!(?room_id, %connection_id, %producer_id, ?kind, %source, "producer created");
        Ok(producer_id)
    }

    /// Creates a Consumer on the participant's recv transport for a given producer.
    ///
    /// FR-127 P1.1 — decided under short guards, created with every guard
    /// released, then recorded under a short guard again, all inside the
    /// participant's `ops` turn, so a view report cannot land between the
    /// decision and the record. This method once held the participant's entry
    /// across both worker round trips and froze a pod (see `tests`).
    pub async fn consume(
        &self,
        room_id: &ObjectId,
        connection_id: &str,
        producer_id: ProducerId,
        rtp_capabilities: &RtpCapabilities,
    ) -> anyhow::Result<ConsumerInfo> {
        let ops = {
            let room = self
                .rooms
                .get(room_id)
                .ok_or_else(|| anyhow::anyhow!("Room not found"))?;

            // Check if the router can consume this producer
            if !room.router.can_consume(&producer_id, rtp_capabilities) {
                return Err(anyhow::anyhow!("Cannot consume: incompatible capabilities"));
            }

            room.participants
                .get(connection_id)
                .map(|p| p.ops.clone())
                .ok_or_else(|| anyhow::anyhow!("Participant not found"))?
        };
        let _turn = ops.lock().await;

        let (recv_transport, source, wanted) = {
            let room = self
                .rooms
                .get(room_id)
                .ok_or_else(|| anyhow::anyhow!("Room not found"))?;

            // FR-30 P4 — read the source producer's pause state (and, FR-127
            // P1, its kind) HERE, before this participant's entry is taken.
            // ⚠️ Iterating the map while holding one of its entries deadlocks
            // it: the iterator waits on the entry's shard for good.
            let source = room
                .participants
                .iter()
                .flat_map(|entry| {
                    entry
                        .value()
                        .producers
                        .iter()
                        .find(|pe| pe.producer.id() == producer_id)
                        .map(|pe| (pe.producer.paused(), pe.producer.kind()))
                })
                .next();

            let participant = room
                .participants
                .get(connection_id)
                .filter(|p| Arc::ptr_eq(&p.ops, &ops))
                .ok_or_else(|| anyhow::anyhow!("Participant not found"))?;

            // FR-127 P1 — a video consumer a view client does not show starts
            // paused, and resumes when a `media:view` names its producer (a
            // report that named it before this consume counts). Audio, and
            // everything for a legacy client, flows as before. A producer the
            // scan did not find (it is not a participant's — nothing in P1) is
            // treated as wanted: the budget is a load lever, never a reason to
            // withhold a picture.
            let wanted = match source {
                Some((_, kind)) => view::wants(participant.view.as_ref(), kind, &producer_id),
                None => true,
            };
            (participant.recv_transport.clone(), source, wanted)
        };
        let producer_paused = source.map(|(paused, _)| paused).unwrap_or(false);

        let mut consumer_options = ConsumerOptions::new(producer_id, rtp_capabilities.clone());
        // ⚠️ `ConsumerOptions.paused` defaults to FALSE (mediasoup 0.20.0): a
        // consumer is created resumed unless asked otherwise. The crate's own
        // doc recommends `paused: true` for video so the first key frame does
        // not race the client's consumer — which is what a view client gets.
        consumer_options.paused = !wanted;
        let consumer = recv_transport
            .consume(consumer_options)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to consume: {}", e))?;

        // Resumed on creation, as before this phase, for everything a view
        // client shows and for all of a legacy client's consumers.
        if wanted {
            consumer
                .resume()
                .await
                .map_err(|e| anyhow::anyhow!("Failed to resume consumer: {}", e))?;
        }

        let info = ConsumerInfo {
            id: consumer.id().to_string(),
            producer_id: consumer.producer_id().to_string(),
            kind: match consumer.kind() {
                MediaKind::Audio => "audio".to_string(),
                MediaKind::Video => "video".to_string(),
            },
            rtp_parameters: serde_json::to_value(consumer.rtp_parameters())?,
            producer_paused,
        };

        // The participant may have left meanwhile, or left and joined again on
        // this connection. A consumer of the old one is dropped unrecorded,
        // which closes it.
        let recorded = self
            .participant_mut(room_id, connection_id, |p| {
                if !Arc::ptr_eq(&p.ops, &ops) {
                    return false;
                }
                p.consumers.push(consumer);
                true
            })
            .unwrap_or(false);
        if !recorded {
            return Err(anyhow::anyhow!("Participant not found"));
        }

        debug!(
            ?room_id,
            %connection_id,
            consumer_id = %info.id,
            %producer_id,
            wanted,
            "consumer created"
        );
        Ok(info)
    }

    /// FR-127 P1 — apply a viewer's `media:view` report: the WHOLE set of
    /// video producers it renders, with `seq` ordering the reports. A video
    /// consumer the report names is resumed, every other video consumer is
    /// paused, audio is never touched (`media/view.rs`).
    ///
    /// Decided under the DashMap guards, acted on after them: the consumer
    /// handles are cloned out and every guard dropped before an await, the
    /// pattern `set_producer_paused` established. A consumer that closed
    /// meanwhile (its producer left) answers an error, which is logged and
    /// skipped — the next report will not name it. FR-127 P1.1 — all inside
    /// the participant's `ops` turn, so a `consume` cannot decide a new
    /// consumer's flag from the view this report is replacing.
    pub async fn apply_view(
        &self,
        room_id: &ObjectId,
        connection_id: &str,
        seq: u64,
        tiles: Vec<(ProducerId, (u32, u32))>,
        max: usize,
    ) -> ViewOutcome {
        let Some(ops) = self.participant(room_id, connection_id, |p| p.ops.clone()) else {
            return ViewOutcome::NotFound;
        };
        let _turn = ops.lock().await;

        let changes: Vec<(Consumer, view::Change)> = {
            let Some(room) = self.rooms.get(room_id) else {
                return ViewOutcome::NotFound;
            };
            let Some(mut participant) = room
                .participants
                .get_mut(connection_id)
                .filter(|p| Arc::ptr_eq(&p.ops, &ops))
            else {
                return ViewOutcome::NotFound;
            };
            let ParticipantMedia {
                view, consumers, ..
            } = &mut *participant;
            // ⚠️ `None` is a legacy client: its consumers all flow, and a
            // report from it (a client that never declared `caps`) is ignored
            // rather than read as "show nothing".
            let Some(view) = view.as_mut() else {
                return ViewOutcome::Legacy;
            };
            if !view.apply(seq, tiles, max) {
                return ViewOutcome::Stale;
            }
            // A consumer the worker closed (its producer left, or its owner
            // closed it) keeps its handle in this Vec: `close_producer` drops
            // only the OWNER's entry. Planning it would ask the worker to
            // pause or resume a dead consumer on every report, for the
            // participant's lifetime. Dropped here, the one place the Vec is
            // held mutably on the view path.
            consumers.retain(|c| !c.closed());
            let lines: Vec<view::ConsumerLine> = consumers
                .iter()
                .map(|c| view::ConsumerLine {
                    producer_id: c.producer_id(),
                    kind: c.kind(),
                    paused: c.paused(),
                })
                .collect();
            view::plan(view, &lines)
                .into_iter()
                .map(|(i, change)| (consumers[i].clone(), change))
                .collect()
        };

        // All the flag changes at once, not one round trip after another: the
        // turn is held until they land, and a `media:consume` forwarded from
        // the other pod for this viewer waits on it within the bus deadline
        // (`RPC_DEADLINE`, 2 s). One at a time, a report flipping 49 tiles on a
        // busy worker could outlast that, and a missed deadline prunes a live
        // owner's claim. The requests touch different consumers, so their order
        // does not matter.
        let results =
            futures::future::join_all(changes.into_iter().map(|(consumer, change)| async move {
                let result = match change {
                    view::Change::Resume => consumer.resume().await,
                    view::Change::Pause => consumer.pause().await,
                };
                (consumer, change, result)
            }))
            .await;
        let (mut resumed, mut paused) = (0, 0);
        for (consumer, change, result) in results {
            match result {
                Ok(()) if change == view::Change::Resume => resumed += 1,
                Ok(()) => paused += 1,
                Err(e) => debug!(
                    ?room_id,
                    %connection_id,
                    consumer_id = %consumer.id(),
                    ?change,
                    error = %e,
                    "view: consumer flag not changed (closed meanwhile?)"
                ),
            }
        }
        debug!(?room_id, %connection_id, seq, resumed, paused, "view applied");
        ViewOutcome::Applied { resumed, paused }
    }

    /// FR-127 P1.1 — `f` on one participant, under DashMap guards that are
    /// released before this returns. What `f` returns is owned (a cloned
    /// handle, plain data), so a caller that awaits next holds no guard.
    fn participant<T>(
        &self,
        room_id: &ObjectId,
        connection_id: &str,
        f: impl FnOnce(&ParticipantMedia) -> T,
    ) -> Option<T> {
        let room = self.rooms.get(room_id)?;
        let participant = room.participants.get(connection_id)?;
        Some(f(&participant))
    }

    /// FR-127 P1.1 — [`Self::participant`], mutably.
    fn participant_mut<T>(
        &self,
        room_id: &ObjectId,
        connection_id: &str,
        f: impl FnOnce(&mut ParticipantMedia) -> T,
    ) -> Option<T> {
        let room = self.rooms.get(room_id)?;
        let mut participant = room.participants.get_mut(connection_id)?;
        Some(f(&mut participant))
    }

    /// FR-127 P1 — one participant's LIVE consumers, as plain data (the tests
    /// and the gauges read this; it names no worker type). A handle the
    /// worker closed is left out; `consumer_handles` counts it.
    pub fn consumer_states(
        &self,
        room_id: &ObjectId,
        connection_id: &str,
    ) -> Vec<ConsumerSnapshot> {
        self.rooms
            .get(room_id)
            .and_then(|room| {
                room.participants.get(connection_id).map(|p| {
                    p.consumers
                        .iter()
                        .filter(|c| !c.closed())
                        .map(|c| ConsumerSnapshot {
                            producer_id: c.producer_id().to_string(),
                            kind: kind_str(c.kind()),
                            paused: c.paused(),
                            producer_paused: c.producer_paused(),
                        })
                        .collect()
                })
            })
            .unwrap_or_default()
    }

    /// FR-127 P1 — how many consumer HANDLES a participant holds, closed ones
    /// included: the tests read it to show a dead handle lingering after its
    /// producer closed, and gone after the next report.
    pub fn consumer_handles(&self, room_id: &ObjectId, connection_id: &str) -> usize {
        self.rooms
            .get(room_id)
            .and_then(|room| {
                room.participants
                    .get(connection_id)
                    .map(|p| p.consumers.len())
            })
            .unwrap_or(0)
    }

    /// FR-127 P1 — one participant's producers, as plain data.
    pub fn producer_states(
        &self,
        room_id: &ObjectId,
        connection_id: &str,
    ) -> Vec<ProducerSnapshot> {
        self.rooms
            .get(room_id)
            .and_then(|room| {
                room.participants.get(connection_id).map(|p| {
                    p.producers
                        .iter()
                        .map(|pe| ProducerSnapshot {
                            producer_id: pe.producer.id().to_string(),
                            kind: kind_str(pe.producer.kind()),
                            source: pe.source.clone(),
                            paused: pe.producer.paused(),
                        })
                        .collect()
                })
            })
            .unwrap_or_default()
    }

    /// Closes a specific producer by ID.
    /// FR-30 — pause or resume ONE of a participant's producers.
    ///
    /// ⚠️ Deliberately NOT a sibling of `close_producer` below, which is sync.
    /// `Producer::pause()` is async, and holding a DashMap guard across an
    /// await is how you deadlock the whole room registry — so the handle is
    /// CLONED out first and every guard dropped before we await. A mediasoup
    /// `Producer` is a cheap cloneable handle, so this costs nothing.
    ///
    /// Pausing server-side is half the point on its own: it stops forwarding
    /// to every consumer, so a participant with their camera off stops
    /// shipping black frames to everyone in the room.
    pub async fn set_producer_paused(
        &self,
        room_id: &ObjectId,
        connection_id: &str,
        producer_id: &ProducerId,
        paused: bool,
    ) -> bool {
        let producer = {
            let Some(room) = self.rooms.get(room_id) else {
                return false;
            };
            let Some(participant) = room.participants.get(connection_id) else {
                return false;
            };
            participant
                .producers
                .iter()
                .find(|pe| &pe.producer.id() == producer_id)
                .map(|pe| pe.producer.clone())
        };

        let Some(producer) = producer else {
            return false;
        };

        let result = if paused {
            producer.pause().await
        } else {
            producer.resume().await
        };

        match result {
            Ok(()) => true,
            Err(e) => {
                warn!(?room_id, %connection_id, %paused, error = %e, "failed to set producer paused");
                false
            }
        }
    }

    pub fn close_producer(
        &self,
        room_id: &ObjectId,
        connection_id: &str,
        producer_id: &ProducerId,
    ) -> bool {
        if let Some(room) = self.rooms.get(room_id)
            && let Some(mut participant) = room.participants.get_mut(connection_id)
        {
            let before = participant.producers.len();
            participant
                .producers
                .retain(|pe| &pe.producer.id() != producer_id);
            return participant.producers.len() < before;
        }
        false
    }

    /// Removes a participant's media state from a room.
    pub fn close_participant(&self, room_id: &ObjectId, connection_id: &str) {
        if let Some(room) = self.rooms.get(room_id) {
            // Dropping the ParticipantMedia closes transports/producers/consumers
            room.participants.remove(connection_id);
        }
        self.connection_rooms.remove(connection_id);
        debug!(?room_id, %connection_id, "participant media closed");
    }

    /// Removes ONE connection's media state, but only when that connection
    /// belongs to `user_id`. The connection id may come from an HTTP body, so
    /// a room member must not be able to close another participant's media by
    /// guessing a UUID. Returns whether an entry was removed.
    pub fn close_participant_of_user(
        &self,
        room_id: &ObjectId,
        connection_id: &str,
        user_id: &ObjectId,
    ) -> bool {
        let mut removed = false;
        if let Some(room) = self.rooms.get(room_id) {
            let owned = room
                .participants
                .get(connection_id)
                .map(|p| &p.user_id == user_id)
                .unwrap_or(false);
            if owned {
                room.participants.remove(connection_id);
                removed = true;
            }
        }
        if removed {
            self.connection_rooms.remove(connection_id);
            debug!(?room_id, %connection_id, "participant media closed (ownership-checked)");
        }
        removed
    }

    /// Removes ALL participant entries for a given user_id from a room.
    /// Used by HTTP leave endpoint which doesn't have a connection_id.
    pub fn close_participant_by_user(&self, room_id: &ObjectId, user_id: &ObjectId) {
        if let Some(room) = self.rooms.get(room_id) {
            let conn_ids: Vec<String> = room
                .participants
                .iter()
                .filter(|e| &e.value().user_id == user_id)
                .map(|e| e.key().clone())
                .collect();
            for cid in conn_ids {
                room.participants.remove(&cid);
                self.connection_rooms.remove(&cid);
            }
        }
        debug!(?room_id, ?user_id, "participant media closed (by user_id)");
    }

    /// Returns all producer IDs in a room except those belonging to the given connection.
    pub fn get_producer_ids(
        &self,
        room_id: &ObjectId,
        exclude_connection_id: &str,
    ) -> Vec<(ObjectId, String, ProducerId, MediaKind, String)> {
        let mut result = Vec::new();
        if let Some(room) = self.rooms.get(room_id) {
            for entry in room.participants.iter() {
                if entry.key() != exclude_connection_id {
                    let uid = entry.value().user_id;
                    let conn_id = entry.key().clone();
                    for pe in &entry.value().producers {
                        result.push((
                            uid,
                            conn_id.clone(),
                            pe.producer.id(),
                            pe.producer.kind(),
                            pe.source.clone(),
                        ));
                    }
                }
            }
        }
        result
    }

    /// Returns unique participant user IDs in a room.
    pub fn get_participant_user_ids(&self, room_id: &ObjectId) -> Vec<ObjectId> {
        self.rooms
            .get(room_id)
            .map(|room| {
                let mut ids: Vec<ObjectId> = room
                    .participants
                    .iter()
                    .map(|e| e.value().user_id)
                    .collect();
                ids.sort();
                ids.dedup();
                ids
            })
            .unwrap_or_default()
    }

    /// Returns user IDs of all participants except those with the given connection_id.
    pub fn get_other_participant_user_ids(
        &self,
        room_id: &ObjectId,
        exclude_connection_id: &str,
    ) -> Vec<ObjectId> {
        self.rooms
            .get(room_id)
            .map(|room| {
                let mut ids: Vec<ObjectId> = room
                    .participants
                    .iter()
                    .filter(|e| e.key() != exclude_connection_id)
                    .map(|e| e.value().user_id)
                    .collect();
                ids.sort();
                ids.dedup();
                ids
            })
            .unwrap_or_default()
    }

    /// Returns connection IDs of all participants except the given connection_id.
    /// Unlike get_other_participant_user_ids, this returns connection-level granularity
    /// so broadcasts don't leak to same-user connections in the same room.
    pub fn get_other_connection_ids(
        &self,
        room_id: &ObjectId,
        exclude_connection_id: &str,
    ) -> Vec<String> {
        self.rooms
            .get(room_id)
            .map(|room| {
                room.participants
                    .iter()
                    .filter(|e| e.key() != exclude_connection_id)
                    .map(|e| e.key().clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Returns the room ID that a connection is currently in, if any.
    pub fn get_connection_room(&self, connection_id: &str) -> Option<ObjectId> {
        self.connection_rooms.get(connection_id).map(|v| *v)
    }

    /// Creates a DirectTransport consumer that taps into a producer's RTP stream.
    ///
    /// Returns an mpsc receiver that yields raw RTP packets. The DirectTransport
    /// and Consumer are stored internally and cleaned up when the tap is removed.
    pub async fn create_rtp_tap(
        &self,
        room_id: &ObjectId,
        producer_id: ProducerId,
    ) -> anyhow::Result<mpsc::Receiver<Vec<u8>>> {
        // FR-127 P1.1 — on a cloned router, with the rooms guard released
        // before the worker round trips (see `tests`).
        let router = self
            .rooms
            .get(room_id)
            .map(|room| room.router.clone())
            .ok_or_else(|| anyhow::anyhow!("Room not found"))?;

        let direct_transport = router
            .create_direct_transport(DirectTransportOptions::default())
            .await
            .map_err(|e| anyhow::anyhow!("Failed to create DirectTransport: {}", e))?;

        // Convert RtpCapabilitiesFinalized → RtpCapabilities via serde (same JSON schema)
        let caps_finalized = router.rtp_capabilities();
        let rtp_capabilities: RtpCapabilities = serde_json::from_value(
            serde_json::to_value(caps_finalized)
                .map_err(|e| anyhow::anyhow!("Failed to serialize capabilities: {}", e))?,
        )
        .map_err(|e| anyhow::anyhow!("Failed to deserialize capabilities: {}", e))?;

        let consumer_options = ConsumerOptions::new(producer_id, rtp_capabilities);
        let consumer = direct_transport
            .consume(consumer_options)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to consume on DirectTransport: {}", e))?;

        // mediasoup consumers are created paused — must resume to receive RTP
        consumer
            .resume()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to resume DirectTransport consumer: {}", e))?;

        let (tx, rx) = mpsc::channel(512);

        // Register RTP callback; detach so it lives as long as the Consumer
        consumer
            .on_rtp(move |data: &[u8]| {
                let _ = tx.try_send(data.to_vec());
            })
            .detach();

        // Store to keep alive, in the room this router still serves: if the
        // room closed meanwhile (or closed and was created again), the tap is
        // dropped here, which closes it and ends the receiver.
        self.rooms
            .get(room_id)
            .filter(|room| room.router.id() == router.id())
            .ok_or_else(|| anyhow::anyhow!("Room not found"))?
            .rtp_taps
            .insert(
                producer_id.to_string(),
                RtpTap {
                    _direct_transport: direct_transport,
                    _consumer: consumer,
                },
            );

        debug!(?room_id, %producer_id, "RTP tap created and resumed");
        Ok(rx)
    }

    /// Removes an RTP tap for a producer (stops the DirectTransport consumer).
    pub fn remove_rtp_tap(&self, room_id: &ObjectId, producer_id: &str) {
        if let Some(room) = self.rooms.get(room_id)
            && room.rtp_taps.remove(producer_id).is_some()
        {
            debug!(?room_id, %producer_id, "RTP tap removed");
        }
    }

    /// Helper: creates a single WebRtcTransport on the given router.
    async fn create_webrtc_transport(&self, router: &Router) -> anyhow::Result<WebRtcTransport> {
        let udp_info = ListenInfo {
            protocol: Protocol::Udp,
            ip: self.listen_ip,
            announced_address: self.announced_ip.clone(),
            port: None,
            port_range: None,
            flags: None,
            send_buffer_size: None,
            recv_buffer_size: None,
            expose_internal_ip: false,
        };

        // TCP fallback — essential when wsl-vpnkit or similar networking
        // intercepts UDP but TCP localhost forwarding still works.
        let tcp_info = ListenInfo {
            protocol: Protocol::Tcp,
            ip: self.listen_ip,
            announced_address: self.announced_ip.clone(),
            port: None,
            port_range: None,
            flags: None,
            send_buffer_size: None,
            recv_buffer_size: None,
            expose_internal_ip: false,
        };

        let listen_infos = WebRtcTransportListenInfos::new(udp_info).insert(tcp_info);
        let mut transport_options = WebRtcTransportOptions::new(listen_infos);
        transport_options.enable_udp = true;
        transport_options.enable_tcp = true;
        transport_options.prefer_udp = true;

        let transport = router
            .create_webrtc_transport(transport_options)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to create WebRtcTransport: {}", e))?;

        Ok(transport)
    }
}

/// Extracts transport connection details for the client.
fn transport_to_options(transport: &WebRtcTransport) -> TransportOptions {
    TransportOptions {
        id: transport.id().to_string(),
        ice_parameters: serde_json::to_value(transport.ice_parameters()).unwrap_or_default(),
        ice_candidates: serde_json::to_value(transport.ice_candidates()).unwrap_or_default(),
        dtls_parameters: serde_json::to_value(transport.dtls_parameters()).unwrap_or_default(),
    }
}

/// Standard SFU media codecs: opus audio + VP8/H264 video. Public for the
/// room-capacity bench (FR-115 O4), which must route what production routes.
pub fn media_codecs() -> Vec<RtpCodecCapability> {
    vec![
        // Opus audio
        RtpCodecCapability::Audio {
            mime_type: MimeTypeAudio::Opus,
            preferred_payload_type: Some(111),
            clock_rate: NonZero::new(48000).unwrap(),
            channels: NonZero::new(2).unwrap(),
            parameters: RtpCodecParametersParameters::default(),
            rtcp_feedback: vec![RtcpFeedback::TransportCc],
        },
        // VP8 video
        RtpCodecCapability::Video {
            mime_type: MimeTypeVideo::Vp8,
            preferred_payload_type: Some(96),
            clock_rate: NonZero::new(90000).unwrap(),
            parameters: RtpCodecParametersParameters::default(),
            rtcp_feedback: vec![
                RtcpFeedback::Nack,
                RtcpFeedback::NackPli,
                RtcpFeedback::CcmFir,
                RtcpFeedback::GoogRemb,
                RtcpFeedback::TransportCc,
            ],
        },
        // H264 video
        RtpCodecCapability::Video {
            mime_type: MimeTypeVideo::H264,
            preferred_payload_type: Some(125),
            clock_rate: NonZero::new(90000).unwrap(),
            parameters: RtpCodecParametersParameters::from([
                ("level-asymmetry-allowed", 1_u32.into()),
                ("packetization-mode", 1_u32.into()),
                ("profile-level-id", "42e01f".into()),
            ]),
            rtcp_feedback: vec![
                RtcpFeedback::Nack,
                RtcpFeedback::NackPli,
                RtcpFeedback::CcmFir,
                RtcpFeedback::GoogRemb,
                RtcpFeedback::TransportCc,
            ],
        },
    ]
}

#[cfg(test)]
mod tests {
    //! FR-127 P1.1 — the meeting that froze a pod (2026-10-27).
    //!
    //! A DashMap guard held across an `.await` does not park the task holding
    //! it: it parks the THREAD of every other task that wants that shard. A
    //! production pod has 2 CPUs, so tokio runs 2 worker threads, and once two
    //! tasks are parked behind a guard whose holder is waiting on its mediasoup
    //! worker, no thread is left to poll the holder. The whole runtime stops,
    //! `/health` with it, until the liveness probe kills the pod. `consume`
    //! held the participant's entry across two worker round trips while every
    //! other `consume` in the room scanned the same map for its producer, so a
    //! few joins at once were enough.
    //!
    //! The storm below is that burst in production's shape: one task per
    //! connection, because the WS loop awaits each message in turn, each one
    //! consuming every producer it does not own and, for a view client,
    //! reporting a view after each consume. It runs on exactly 2 worker
    //! threads, from a plain `#[test]` with a watchdog, so a freeze FAILS the
    //! test instead of hanging the suite.
    use super::*;
    use std::sync::mpsc::RecvTimeoutError;
    use std::time::Duration;

    /// What a 2-CPU pod gives tokio.
    const WORKER_THREADS: usize = 2;
    const PARTICIPANTS: usize = 12;
    /// Participants `0..SENDERS` publish audio and a camera.
    const SENDERS: usize = 4;
    /// The storm takes about a second; a freeze never ends.
    const DEADLINE: Duration = Duration::from_secs(60);

    fn settings() -> MediasoupSettings {
        MediasoupSettings {
            num_workers: 1,
            listen_ip: "127.0.0.1".to_string(),
            announced_ip: String::new(),
            announced_ip_map: None,
            rtc_min_port: 40000,
            rtc_max_port: 49999,
        }
    }

    /// Parameters the worker accepts for a producer on a transport that never
    /// DTLS-connects, as `crates/tests/src/conference_view_tests.rs` sends them.
    fn rtp_parameters(kind: MediaKind, ssrc: u32) -> RtpParameters {
        let codec = match kind {
            MediaKind::Audio => serde_json::json!({
                "mimeType": "audio/opus", "payloadType": 111, "clockRate": 48000,
                "channels": 2, "parameters": {}, "rtcpFeedback": []
            }),
            MediaKind::Video => serde_json::json!({
                "mimeType": "video/VP8", "payloadType": 96, "clockRate": 90000,
                "parameters": {}, "rtcpFeedback": []
            }),
        };
        serde_json::from_value(serde_json::json!({
            "codecs": [codec],
            "headerExtensions": [],
            "encodings": [{ "ssrc": ssrc }],
            "rtcp": { "cname": format!("cname-{ssrc}"), "reducedSize": true }
        }))
        .expect("producer parameters")
    }

    fn conn(i: usize) -> String {
        format!("conn-{i}")
    }

    /// Half the room are view clients (`caps: ["view"]`), half legacy.
    fn view_client(i: usize) -> bool {
        i.is_multiple_of(2)
    }

    /// The cameras participant `i`'s report number `step` names: a set that
    /// shifts with every report, so each report pauses some consumers and
    /// resumes others, and names cameras not consumed yet.
    fn named(i: usize, step: usize, cameras: &[(usize, ProducerId)]) -> Vec<ProducerId> {
        cameras
            .iter()
            .filter(|(sender, _)| *sender != i && (sender + i + step).is_multiple_of(2))
            .map(|(_, producer)| *producer)
            .collect()
    }

    /// A room manager over one production-configured worker.
    async fn manager() -> Result<Arc<RoomManager>, String> {
        let pool = WorkerPool::new(&settings())
            .await
            .map_err(|e| e.to_string())?;
        Ok(Arc::new(RoomManager::new(Arc::new(pool), &settings())))
    }

    /// The router's capabilities as a client sends them back to consume with.
    fn client_caps(rm: &RoomManager, rid: &ObjectId) -> Result<RtpCapabilities, String> {
        let room = rm.rooms.get(rid).ok_or("the room exists")?;
        let finalized =
            serde_json::to_value(room.router.rtp_capabilities()).map_err(|e| e.to_string())?;
        serde_json::from_value(finalized).map_err(|e| e.to_string())
    }

    /// Runs `body` on a runtime of exactly [`WORKER_THREADS`] threads, in a
    /// thread of its own, and fails the test if it has not finished within
    /// [`DEADLINE`]: a freeze FAILS instead of hanging the suite. A body reports
    /// a failure as an `Err`, so its message survives.
    fn on_two_threads<F>(body: impl FnOnce() -> F + Send + 'static)
    where
        F: std::future::Future<Output = Result<(), String>>,
    {
        let (done, outcome) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("two-threads".to_string())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(WORKER_THREADS)
                    .enable_all()
                    .build()
                    .expect("a runtime");
                let _ = done.send(runtime.block_on(body()));
            })
            .expect("the test's thread");
        match outcome.recv_timeout(DEADLINE) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => panic!("{e}"),
            Err(RecvTimeoutError::Timeout) => panic!(
                "the room manager froze: the test did not finish within {} s on {WORKER_THREADS} \
                 worker threads, so a DashMap guard is held across an await",
                DEADLINE.as_secs()
            ),
            Err(RecvTimeoutError::Disconnected) => panic!("the test's thread panicked"),
        }
    }

    async fn storm() -> Result<(), String> {
        let rm = manager().await?;
        let rid = ObjectId::new();
        rm.create_room(rid).await.map_err(|e| e.to_string())?;
        let caps = client_caps(&rm, &rid)?;

        // Everyone joins at once.
        let joins: Vec<_> = (0..PARTICIPANTS)
            .map(|i| {
                let rm = rm.clone();
                tokio::spawn(async move {
                    rm.create_transports(rid, ObjectId::new(), conn(i), view_client(i))
                        .await
                })
            })
            .collect();
        for join in joins {
            join.await
                .map_err(|e| e.to_string())?
                .map_err(|e| format!("join: {e}"))?;
        }

        // The senders publish at once.
        let publishes: Vec<_> = (0..SENDERS)
            .flat_map(|s| [MediaKind::Audio, MediaKind::Video].map(move |kind| (s, kind)))
            .enumerate()
            .map(|(n, (s, kind))| {
                let rm = rm.clone();
                tokio::spawn(async move {
                    let source = match kind {
                        MediaKind::Audio => "audio",
                        MediaKind::Video => "camera",
                    };
                    let parameters = rtp_parameters(kind, 1000 + n as u32);
                    rm.produce(&rid, &conn(s), kind, parameters, source.to_string())
                        .await
                        .map(|producer| (s, kind, producer))
                })
            })
            .collect();
        let mut producers = Vec::new();
        for publish in publishes {
            producers.push(
                publish
                    .await
                    .map_err(|e| e.to_string())?
                    .map_err(|e| format!("produce: {e}"))?,
            );
        }
        let cameras: Vec<(usize, ProducerId)> = producers
            .iter()
            .filter(|(_, kind, _)| *kind == MediaKind::Video)
            .map(|(s, _, producer)| (*s, *producer))
            .collect();

        // The storm: every connection consumes everything it does not own.
        let connections: Vec<_> = (0..PARTICIPANTS)
            .map(|i| {
                let (rm, caps) = (rm.clone(), caps.clone());
                let (producers, cameras) = (producers.clone(), cameras.clone());
                tokio::spawn(async move {
                    let mut step = 0;
                    for (_, _, producer) in producers.iter().filter(|(s, ..)| *s != i) {
                        rm.consume(&rid, &conn(i), *producer, &caps)
                            .await
                            .map_err(|e| format!("{} consume: {e}", conn(i)))?;
                        step += 1;
                        if view_client(i) {
                            let tiles = named(i, step, &cameras)
                                .into_iter()
                                .map(|producer| (producer, (320, 240)))
                                .collect();
                            let outcome =
                                rm.apply_view(&rid, &conn(i), step as u64, tiles, 49).await;
                            if !matches!(outcome, ViewOutcome::Applied { .. }) {
                                return Err(format!("{} report {step}: {outcome:?}", conn(i)));
                            }
                        }
                    }
                    Ok::<_, String>(step)
                })
            })
            .collect();
        let mut last_report = Vec::new();
        for connection in connections {
            last_report.push(connection.await.map_err(|e| e.to_string())??);
        }

        // Every connection holds one live consumer per producer it does not
        // own, and each one's flag is what that connection's last report asked
        // for: a view client's unnamed cameras paused, everything else flowing.
        for (i, last) in last_report.into_iter().enumerate() {
            let states = rm.consumer_states(&rid, &conn(i));
            let expected = producers.iter().filter(|(s, ..)| *s != i).count();
            let handles = rm.consumer_handles(&rid, &conn(i));
            if states.len() != expected || handles != expected {
                return Err(format!(
                    "{}: {} live consumers and {handles} handles, expected {expected}",
                    conn(i),
                    states.len()
                ));
            }
            let shown: Vec<String> = named(i, last, &cameras)
                .iter()
                .map(|producer| producer.to_string())
                .collect();
            for c in &states {
                let paused = view_client(i) && c.kind == "video" && !shown.contains(&c.producer_id);
                if c.paused != paused {
                    return Err(format!(
                        "{}: the {} consumer of {} has paused={}, expected {paused}",
                        conn(i),
                        c.kind,
                        c.producer_id,
                        c.paused
                    ));
                }
            }
        }
        Ok(())
    }

    #[test]
    fn a_burst_of_joins_on_two_worker_threads_does_not_freeze_the_runtime() {
        on_two_threads(storm);
    }

    /// How long a test watches for an operation that must be waiting on a
    /// participant's turn. Any return within it is a failure; a correct one
    /// waits until the turn is released.
    const WAITING: Duration = Duration::from_millis(300);

    /// A room with a sender (`conn(0)`) publishing a camera and a view client
    /// (`conn(1)`) that has not consumed it yet.
    async fn one_camera_one_viewer()
    -> Result<(Arc<RoomManager>, ObjectId, RtpCapabilities, ProducerId), String> {
        let rm = manager().await?;
        let rid = ObjectId::new();
        rm.create_room(rid).await.map_err(|e| e.to_string())?;
        let caps = client_caps(&rm, &rid)?;
        for i in 0..2 {
            rm.create_transports(rid, ObjectId::new(), conn(i), true)
                .await
                .map_err(|e| format!("join: {e}"))?;
        }
        let camera = rm
            .produce(
                &rid,
                &conn(0),
                MediaKind::Video,
                rtp_parameters(MediaKind::Video, 2000),
                "camera".to_string(),
            )
            .await
            .map_err(|e| format!("produce: {e}"))?;
        Ok((rm, rid, caps, camera))
    }

    /// The turn orders one viewer's operations: a report that arrives while a
    /// `consume` holds it waits, rather than landing between that consume's
    /// decision and its record, where the new consumer would miss it. The test
    /// holds the viewer's turn itself, as a consume mid-flight would.
    #[test]
    fn a_view_report_waits_for_the_viewers_turn() {
        on_two_threads(|| async {
            let (rm, rid, _, camera) = one_camera_one_viewer().await?;
            let ops = rm
                .participant(&rid, &conn(1), |p| p.ops.clone())
                .ok_or("the viewer")?;
            let turn = ops.lock().await;
            let report = {
                let rm = rm.clone();
                tokio::spawn(async move {
                    rm.apply_view(&rid, &conn(1), 1, vec![(camera, (320, 240))], 49)
                        .await
                })
            };
            tokio::time::sleep(WAITING).await;
            if report.is_finished() {
                return Err("a view report ran while the viewer's turn was taken".into());
            }
            drop(turn);
            match report.await.map_err(|e| e.to_string())? {
                ViewOutcome::Applied { .. } => Ok(()),
                other => Err(format!("the report, once its turn came: {other:?}")),
            }
        });
    }

    /// The same order from the other side: a consume waits for a report (or
    /// another consume) holding the viewer's turn.
    #[test]
    fn a_consume_waits_for_the_viewers_turn() {
        on_two_threads(|| async {
            let (rm, rid, caps, camera) = one_camera_one_viewer().await?;
            let ops = rm
                .participant(&rid, &conn(1), |p| p.ops.clone())
                .ok_or("the viewer")?;
            let turn = ops.lock().await;
            let consume = {
                let rm = rm.clone();
                tokio::spawn(async move { rm.consume(&rid, &conn(1), camera, &caps).await })
            };
            tokio::time::sleep(WAITING).await;
            if consume.is_finished() {
                return Err("a consume ran while the viewer's turn was taken".into());
            }
            drop(turn);
            consume
                .await
                .map_err(|e| e.to_string())?
                .map_err(|e| format!("the consume, once its turn came: {e}"))?;
            match rm.consumer_handles(&rid, &conn(1)) {
                1 => Ok(()),
                n => Err(format!("{n} consumers recorded, expected 1")),
            }
        });
    }

    /// The turn also names the participant. A consume that started for one
    /// participant and resumes after the connection joined again (a new
    /// participant, a new turn) records nothing: its consumer would belong to
    /// the old participant's closed transport.
    #[test]
    fn a_consume_across_a_rejoin_records_nothing_for_the_new_participant() {
        on_two_threads(|| async {
            let (rm, rid, caps, camera) = one_camera_one_viewer().await?;
            let ops = rm
                .participant(&rid, &conn(1), |p| p.ops.clone())
                .ok_or("the viewer")?;
            let turn = ops.lock().await;
            let consume = {
                let rm = rm.clone();
                tokio::spawn(async move { rm.consume(&rid, &conn(1), camera, &caps).await })
            };
            tokio::time::sleep(WAITING).await;
            if consume.is_finished() {
                return Err("a consume ran while the viewer's turn was taken".into());
            }
            // The viewer joins again on the same connection.
            rm.create_transports(rid, ObjectId::new(), conn(1), true)
                .await
                .map_err(|e| format!("re-join: {e}"))?;
            drop(turn);
            match consume.await.map_err(|e| e.to_string())? {
                Err(e) if e.to_string() == "Participant not found" => {}
                Err(e) => return Err(format!("the old participant's consume: {e}")),
                Ok(_) => return Err("the old participant's consume succeeded".into()),
            }
            match rm.consumer_handles(&rid, &conn(1)) {
                0 => Ok(()),
                n => Err(format!(
                    "the new participant holds {n} consumers, expected 0"
                )),
            }
        });
    }

    /// Two joins that both find no room both build a router; the first to
    /// record its room keeps it. Before P1.1 the second's `insert` replaced a
    /// room that could already hold the first joiner's transports.
    #[test]
    fn two_joins_racing_to_create_a_room_keep_one_room() {
        on_two_threads(|| async {
            let rm = manager().await?;
            for round in 0..50 {
                let rid = ObjectId::new();
                let first = {
                    let rm = rm.clone();
                    tokio::spawn(async move {
                        rm.create_room(rid).await.map_err(|e| e.to_string())?;
                        rm.create_transports(rid, ObjectId::new(), conn(0), true)
                            .await
                            .map_err(|e| e.to_string())?;
                        Ok::<_, String>(())
                    })
                };
                let second = {
                    let rm = rm.clone();
                    tokio::spawn(async move { rm.create_room(rid).await.map(|_| ()) })
                };
                first
                    .await
                    .map_err(|e| e.to_string())?
                    .map_err(|e| format!("round {round}, the first joiner: {e}"))?;
                second
                    .await
                    .map_err(|e| e.to_string())?
                    .map_err(|e| format!("round {round}, the second joiner: {e}"))?;
                let present = rm.get_participant_user_ids(&rid).len();
                if present != 1 {
                    return Err(format!(
                        "round {round}: {present} participants, expected the first joiner's 1"
                    ));
                }
            }
            Ok(())
        });
    }
}
