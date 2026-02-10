use bson::oid::ObjectId;
use dashmap::DashMap;
use mediasoup::prelude::*;
use mediasoup::webrtc_transport::{
    WebRtcTransportListenInfos, WebRtcTransportOptions,
    WebRtcTransportRemoteParameters,
};
use roomler2_config::MediasoupSettings;
use serde::{Deserialize, Serialize};
use std::net::IpAddr;
use std::num::NonZero;
use std::str::FromStr;
use std::sync::Arc;
use tracing::{debug, info};

use super::worker_pool::WorkerPool;

/// A media room backed by a mediasoup Router.
pub struct MediaRoom {
    pub router: Router,
    /// Keyed by connection_id (UUID per WebSocket connection) so the same user
    /// can join from multiple tabs/devices without overwriting state.
    pub participants: DashMap<String, ParticipantMedia>,
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
}

/// Manages mediasoup rooms and their media state.
pub struct RoomManager {
    rooms: DashMap<ObjectId, MediaRoom>,
    /// Tracks which conference each connection is in (connection_id -> conference_id).
    connection_rooms: DashMap<String, ObjectId>,
    worker_pool: Arc<WorkerPool>,
    listen_ip: IpAddr,
    announced_ip: Option<String>,
}

impl RoomManager {
    pub fn new(worker_pool: Arc<WorkerPool>, settings: &MediasoupSettings) -> Self {
        let listen_ip: IpAddr = settings
            .listen_ip
            .parse()
            .unwrap_or_else(|_| "0.0.0.0".parse().unwrap());

        let announced_ip = if settings.announced_ip.is_empty() {
            None
        } else {
            Some(settings.announced_ip.clone())
        };

        Self {
            rooms: DashMap::new(),
            connection_rooms: DashMap::new(),
            worker_pool,
            listen_ip,
            announced_ip,
        }
    }

    /// Creates a mediasoup Router for a conference and stores it.
    /// Returns the router's RTP capabilities (serialized).
    pub async fn create_room(
        &self,
        conference_id: ObjectId,
    ) -> anyhow::Result<serde_json::Value> {
        if self.rooms.contains_key(&conference_id) {
            let room = self.rooms.get(&conference_id).unwrap();
            let caps = room.router.rtp_capabilities().clone();
            return Ok(serde_json::to_value(caps)?);
        }

        let worker = self.worker_pool.get_worker();

        let media_codecs = media_codecs();
        let router_options = RouterOptions::new(media_codecs);
        let router = worker
            .create_router(router_options)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to create router: {}", e))?;

        let caps = router.rtp_capabilities().clone();
        info!(?conference_id, "mediasoup room created");

        self.rooms.insert(
            conference_id,
            MediaRoom {
                router,
                participants: DashMap::new(),
            },
        );

        Ok(serde_json::to_value(caps)?)
    }

    /// Removes a room and all its media state.
    pub fn remove_room(&self, conference_id: &ObjectId) -> bool {
        if let Some((_, room)) = self.rooms.remove(conference_id) {
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
            info!(?conference_id, "mediasoup room removed");
            true
        } else {
            false
        }
    }

    pub fn has_room(&self, conference_id: &ObjectId) -> bool {
        self.rooms.contains_key(conference_id)
    }

    pub fn room_count(&self) -> usize {
        self.rooms.len()
    }

    /// Returns a reference to the rooms DashMap (for WS handler to read router capabilities).
    pub fn rooms_ref(&self) -> &DashMap<ObjectId, MediaRoom> {
        &self.rooms
    }

    /// Creates send + recv WebRtcTransport pair for a participant.
    pub async fn create_transports(
        &self,
        conference_id: ObjectId,
        user_id: ObjectId,
        connection_id: String,
    ) -> anyhow::Result<TransportPair> {
        let room = self
            .rooms
            .get(&conference_id)
            .ok_or_else(|| anyhow::anyhow!("Room not found"))?;

        let send_transport = self.create_webrtc_transport(&room.router).await?;
        let recv_transport = self.create_webrtc_transport(&room.router).await?;

        let send_opts = transport_to_options(&send_transport);
        let recv_opts = transport_to_options(&recv_transport);

        room.participants.insert(
            connection_id.clone(),
            ParticipantMedia {
                user_id,
                send_transport,
                recv_transport,
                producers: Vec::new(),
                consumers: Vec::new(),
            },
        );

        self.connection_rooms.insert(connection_id.clone(), conference_id);

        debug!(?conference_id, ?user_id, %connection_id, "transports created");

        Ok(TransportPair {
            send_transport: send_opts,
            recv_transport: recv_opts,
        })
    }

    /// Connects a transport with remote DTLS parameters.
    pub async fn connect_transport(
        &self,
        conference_id: &ObjectId,
        connection_id: &str,
        transport_id: &str,
        dtls_parameters: DtlsParameters,
    ) -> anyhow::Result<()> {
        let room = self
            .rooms
            .get(conference_id)
            .ok_or_else(|| anyhow::anyhow!("Room not found"))?;

        let participant = room
            .participants
            .get(connection_id)
            .ok_or_else(|| anyhow::anyhow!("Participant not found"))?;

        let tid = TransportId::from_str(transport_id)
            .map_err(|e| anyhow::anyhow!("Invalid transport_id: {}", e))?;

        let remote_params = WebRtcTransportRemoteParameters { dtls_parameters };

        if participant.send_transport.id() == tid {
            participant
                .send_transport
                .connect(remote_params)
                .await
                .map_err(|e| anyhow::anyhow!("Failed to connect send transport: {}", e))?;
        } else if participant.recv_transport.id() == tid {
            participant
                .recv_transport
                .connect(remote_params)
                .await
                .map_err(|e| anyhow::anyhow!("Failed to connect recv transport: {}", e))?;
        } else {
            return Err(anyhow::anyhow!("Transport not found for this participant"));
        }

        debug!(?conference_id, %connection_id, transport_id, "transport connected");
        Ok(())
    }

    /// Creates a Producer on the participant's send transport.
    pub async fn produce(
        &self,
        conference_id: &ObjectId,
        connection_id: &str,
        kind: MediaKind,
        rtp_parameters: RtpParameters,
        source: String,
    ) -> anyhow::Result<ProducerId> {
        let room = self
            .rooms
            .get(conference_id)
            .ok_or_else(|| anyhow::anyhow!("Room not found"))?;

        let mut participant = room
            .participants
            .get_mut(connection_id)
            .ok_or_else(|| anyhow::anyhow!("Participant not found"))?;

        let producer_options = ProducerOptions::new(kind, rtp_parameters);
        let producer = participant
            .send_transport
            .produce(producer_options)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to produce: {}", e))?;

        let producer_id = producer.id();
        participant.producers.push(ProducerEntry { producer, source: source.clone() });

        debug!(?conference_id, %connection_id, %producer_id, ?kind, %source, "producer created");
        Ok(producer_id)
    }

    /// Creates a Consumer on the participant's recv transport for a given producer.
    pub async fn consume(
        &self,
        conference_id: &ObjectId,
        connection_id: &str,
        producer_id: ProducerId,
        rtp_capabilities: &RtpCapabilities,
    ) -> anyhow::Result<ConsumerInfo> {
        let room = self
            .rooms
            .get(conference_id)
            .ok_or_else(|| anyhow::anyhow!("Room not found"))?;

        // Check if the router can consume this producer
        if !room.router.can_consume(&producer_id, rtp_capabilities) {
            return Err(anyhow::anyhow!("Cannot consume: incompatible capabilities"));
        }

        let mut participant = room
            .participants
            .get_mut(connection_id)
            .ok_or_else(|| anyhow::anyhow!("Participant not found"))?;

        let consumer_options = ConsumerOptions::new(producer_id, rtp_capabilities.clone());
        let consumer = participant
            .recv_transport
            .consume(consumer_options)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to consume: {}", e))?;

        let info = ConsumerInfo {
            id: consumer.id().to_string(),
            producer_id: consumer.producer_id().to_string(),
            kind: match consumer.kind() {
                MediaKind::Audio => "audio".to_string(),
                MediaKind::Video => "video".to_string(),
            },
            rtp_parameters: serde_json::to_value(consumer.rtp_parameters())?,
        };

        participant.consumers.push(consumer);

        debug!(
            ?conference_id,
            %connection_id,
            consumer_id = %info.id,
            %producer_id,
            "consumer created"
        );
        Ok(info)
    }

    /// Closes a specific producer by ID.
    pub fn close_producer(
        &self,
        conference_id: &ObjectId,
        connection_id: &str,
        producer_id: &ProducerId,
    ) -> bool {
        if let Some(room) = self.rooms.get(conference_id) {
            if let Some(mut participant) = room.participants.get_mut(connection_id) {
                let before = participant.producers.len();
                participant
                    .producers
                    .retain(|pe| &pe.producer.id() != producer_id);
                return participant.producers.len() < before;
            }
        }
        false
    }

    /// Removes a participant's media state from a room.
    pub fn close_participant(&self, conference_id: &ObjectId, connection_id: &str) {
        if let Some(room) = self.rooms.get(conference_id) {
            // Dropping the ParticipantMedia closes transports/producers/consumers
            room.participants.remove(connection_id);
        }
        self.connection_rooms.remove(connection_id);
        debug!(?conference_id, %connection_id, "participant media closed");
    }

    /// Removes ALL participant entries for a given user_id from a room.
    /// Used by HTTP leave endpoint which doesn't have a connection_id.
    pub fn close_participant_by_user(&self, conference_id: &ObjectId, user_id: &ObjectId) {
        if let Some(room) = self.rooms.get(conference_id) {
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
        debug!(?conference_id, ?user_id, "participant media closed (by user_id)");
    }

    /// Returns all producer IDs in a room except those belonging to the given connection.
    pub fn get_producer_ids(
        &self,
        conference_id: &ObjectId,
        exclude_connection_id: &str,
    ) -> Vec<(ObjectId, String, ProducerId, MediaKind, String)> {
        let mut result = Vec::new();
        if let Some(room) = self.rooms.get(conference_id) {
            for entry in room.participants.iter() {
                if entry.key() != exclude_connection_id {
                    let uid = entry.value().user_id;
                    let conn_id = entry.key().clone();
                    for pe in &entry.value().producers {
                        result.push((uid, conn_id.clone(), pe.producer.id(), pe.producer.kind(), pe.source.clone()));
                    }
                }
            }
        }
        result
    }

    /// Returns unique participant user IDs in a room.
    pub fn get_participant_user_ids(&self, conference_id: &ObjectId) -> Vec<ObjectId> {
        self.rooms
            .get(conference_id)
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
        conference_id: &ObjectId,
        exclude_connection_id: &str,
    ) -> Vec<ObjectId> {
        self.rooms
            .get(conference_id)
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
        conference_id: &ObjectId,
        exclude_connection_id: &str,
    ) -> Vec<String> {
        self.rooms
            .get(conference_id)
            .map(|room| {
                room.participants
                    .iter()
                    .filter(|e| e.key() != exclude_connection_id)
                    .map(|e| e.key().clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Returns the conference ID that a connection is currently in, if any.
    pub fn get_connection_conference(&self, connection_id: &str) -> Option<ObjectId> {
        self.connection_rooms.get(connection_id).map(|v| *v)
    }

    /// Helper: creates a single WebRtcTransport on the given router.
    async fn create_webrtc_transport(
        &self,
        router: &Router,
    ) -> anyhow::Result<WebRtcTransport> {
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

/// Standard SFU media codecs: opus audio + VP8/H264 video.
fn media_codecs() -> Vec<RtpCodecCapability> {
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
