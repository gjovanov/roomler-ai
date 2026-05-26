//! Wire protocol for the `rc:*` WebSocket namespace.
//!
//! Both the agent and the controller browser speak the same envelope shape;
//! they're distinguished by which JWT audience their connection authenticated
//! with. See `signaling::Role`.
//!
//! Every message is a JSON object with a `t` discriminator. We use serde's
//! `tag = "t"` adjacent encoding so the wire is small and stable.
//!
//! **ObjectId fields are serialised as raw hex strings, not bson-extended
//! JSON (`{"$oid":"…"}`).** This matches the REST responses and is what
//! the browser / native agent clients actually produce. See
//! [`serde_helpers`] for the pinning shims; a regression test in that
//! module locks the format.

use bson::oid::ObjectId;
use serde::{Deserialize, Serialize};

use crate::models::{AgentCaps, DisplayInfo, EndReason, OsKind};
use crate::permissions::Permissions;
use crate::serde_helpers::{oid_hex, option_oid_hex};

/// Which side of the connection sent / receives a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Agent,
    Controller,
}

// ────────────────────────────────────────────────────────────────────────────
// Tunnel supporting types
// ────────────────────────────────────────────────────────────────────────────

/// Role advertised in `rc:tunnel.hello`. Distinguishes the
/// `roomler-tunnel` CLI (which initiates forwards) from the agent
/// (which serves them). Wire form: `"client"` | `"agent"`.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TunnelRole {
    Client,
    Agent,
}

/// Why a `TcpForwardRequest` was rejected. The discriminator drives
/// the CLI's exit-code mapping + the audit log row's `kind`.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RejectKind {
    /// Tenant of the requesting client ≠ tenant of the target agent.
    /// Server-side gate (plan §"Multi-tenancy gotcha"); never reached
    /// after the WS handshake's tenant_id check, but locked here as
    /// defence-in-depth.
    CrossTenant,
    /// Tenant policy denies this (subject, agent, dst) tuple.
    AclDenied,
    /// Agent dialed dst and got a hard failure (connection refused,
    /// dst unreachable, dns failure).
    DialFailed,
    /// Per-session concurrent-flow ceiling reached.
    RateLimited,
    /// Per-peer concurrent-flow ceiling reached (default 256 per plan
    /// "Structural issues" #1a — bounds the leak risk under JDBC churn).
    TooManyFlows,
    /// Catch-all for agent-side errors that don't fit above.
    AgentError,
}

/// Half-close direction in `TcpHalfClose`. SMTP / HTTP-1.1-long-poll /
/// some legacy protocols rely on this.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    /// Source (client-side listener) has finished writing; reads still
    /// alive. Mirrors `TcpStream::shutdown(Shutdown::Write)`.
    SrcToDst,
    /// Destination (agent's dialed dst) has finished writing.
    DstToSrc,
}

/// Why a `TcpClosed` was emitted. Mostly free-form but the common
/// cases are enumerated for the audit log's roll-up.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CloseReason {
    /// Clean EOF from one side.
    Eof,
    /// I/O error on the agent's dst socket or the client's local
    /// socket.
    IoError,
    /// Agent-side allowlist (belt-and-suspenders) denied dst.
    AgentAclDenied,
    /// Client-side `tunnel forward` Ctrl-C / shutdown.
    ClientShutdown,
    /// Server kicked the session (admin terminate, revocation, etc.).
    ServerTerminated,
    /// Idle-timeout (default 5 min — see plan §"Missing pieces").
    IdleTimeout,
}

// ────────────────────────────────────────────────────────────────────────────
// Agent connection-level close reasons (rc.53)
// ────────────────────────────────────────────────────────────────────────────

/// Server-initiated close reason for an agent WS connection. Carried
/// by [`ServerMsg::Goodbye`]. Distinct from the session-level
/// [`EndReason`] (which terminates one remote-control session) and
/// from [`CloseReason`] (which terminates one tunnel flow) — this is
/// connection-level.
///
/// `Deserialize` is implemented by hand so that unknown variants
/// (rc.54+ additions) decode to [`PolicyRejected`] rather than
/// serde-failing the whole message. This is the future-compat
/// hatch a fielded rc.53 agent needs to survive a server that
/// learned new reasons.
///
/// [`PolicyRejected`]: AgentCloseReason::PolicyRejected
#[derive(Serialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentCloseReason {
    /// Server-side `agents` row has `deleted_at != null` or is
    /// otherwise refused by the WS handler's lookup. The agent's
    /// stored token is cryptographically valid but useless. Re-enrol
    /// to revive (soft-deleted rows rehydrate on `(tenant_id,
    /// machine_id)` match — `Hub::register_agent` calls `rehydrate()`).
    AgentDeleted,
    /// A newer WS connection presented the SAME `agent_id`; the Hub
    /// kept the new one, dropped this old one. Indicates a duplicate
    /// install somewhere (another physical host with a copy of this
    /// `config.toml`, the tray companion, etc.).
    ReplacedByNewerConnection,
    /// Server-side policy refused (account suspended, tenant
    /// disabled, version too old). Reserved for future use; also the
    /// default the decoder picks for unknown-string variants so
    /// future rc.54+ variants don't hard-fault rc.53 agents in the
    /// field.
    PolicyRejected,
}

impl<'de> Deserialize<'de> for AgentCloseReason {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Ok(match s.as_str() {
            "agent_deleted" => AgentCloseReason::AgentDeleted,
            "replaced_by_newer_connection" => AgentCloseReason::ReplacedByNewerConnection,
            "policy_rejected" => AgentCloseReason::PolicyRejected,
            // Forward-compat: unknown rc.54+ reasons decode to a
            // safe non-panicking default. The agent's handle_server_msg
            // arm treats PolicyRejected as fatal, which matches the
            // semantics of "server told us to stop and we don't know
            // why" better than `ReplacedByNewer` (which is recoverable).
            _ => AgentCloseReason::PolicyRejected,
        })
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Inbound from clients (agent or controller browser)
// ────────────────────────────────────────────────────────────────────────────

/// Messages the server receives.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "t")]
pub enum ClientMsg {
    // ─── agent → server ───────────────────────────────────────────────
    /// Agent announces itself after WS auth.
    #[serde(rename = "rc:agent.hello")]
    AgentHello {
        machine_name: String,
        os: OsKind,
        agent_version: String,
        displays: Vec<DisplayInfo>,
        caps: AgentCaps,
    },

    /// Agent periodic stats.
    #[serde(rename = "rc:agent.heartbeat")]
    AgentHeartbeat {
        rss_mb: u32,
        cpu_pct: f32,
        active_sessions: u8,
    },

    /// Agent answers a controller's offer.
    #[serde(rename = "rc:sdp.answer")]
    SdpAnswer {
        #[serde(with = "oid_hex")]
        session_id: ObjectId,
        sdp: String,
    },

    /// Agent decision on a control request.
    #[serde(rename = "rc:consent")]
    Consent {
        #[serde(with = "oid_hex")]
        session_id: ObjectId,
        granted: bool,
    },

    // ─── controller → server ─────────────────────────────────────────
    /// Controller initiates a session. Server creates the RemoteSession,
    /// notifies the agent, and waits for consent.
    ///
    /// `browser_caps` is the controller's `RTCRtpReceiver.
    /// getCapabilities('video').codecs` filtered to the codecs the
    /// agent's negotiation logic cares about (h264 / h265 / av1 / vp9).
    /// Phase 2 commit 2B.2 uses the intersection of this list with the
    /// agent's `AgentCaps.codecs` to pick the best codec for the
    /// session. Optional + default-empty so older controllers that
    /// don't include it still get an h264 session.
    ///
    /// `preferred_transport` (Phase Y.3) tells the agent which video
    /// transport the controller wants to use. Recognised values match
    /// `AgentCaps.transports`: today only `data-channel-vp9-444` is
    /// defined. `None` / unset means "use the WebRTC video track" —
    /// the legacy default that all in-flight controllers default to.
    /// The agent only honours the request when its own caps advertise
    /// the same transport (browser × agent intersection); otherwise
    /// it ignores the field and falls back to the WebRTC track.
    #[serde(rename = "rc:session.request")]
    SessionRequest {
        #[serde(with = "oid_hex")]
        agent_id: ObjectId,
        permissions: Permissions,
        #[serde(default)]
        browser_caps: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        preferred_transport: Option<String>,
        /// rc.62 — per-session VP9 chroma override. Recognised values:
        /// `"yuv420"` (VP9 profile 0; ~30% lower bandwidth; slight
        /// ClearType softening) and `"yuv444"` (VP9 profile 1; sharpest
        /// text; current default). `None` / unset means "use the
        /// agent's `ROOMLER_AGENT_VP9_CHROMA` env-var default". Only
        /// applies when `preferred_transport` is `data-channel-vp9-444`;
        /// ignored otherwise. Forwarded verbatim to the agent in the
        /// matching server-side [`ServerMsg::SessionRequest`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        chroma_pref: Option<String>,
    },

    /// Controller sends an SDP offer (after consent granted).
    #[serde(rename = "rc:sdp.offer")]
    SdpOffer {
        #[serde(with = "oid_hex")]
        session_id: ObjectId,
        sdp: String,
    },

    // ─── either side → server ────────────────────────────────────────
    /// Trickle ICE candidate. Server forwards to the peer.
    #[serde(rename = "rc:ice")]
    Ice {
        #[serde(with = "oid_hex")]
        session_id: ObjectId,
        candidate: serde_json::Value, // { candidate, sdpMid, sdpMLineIndex, ... }
    },

    /// Either side hangs up.
    #[serde(rename = "rc:terminate")]
    Terminate {
        #[serde(with = "oid_hex")]
        session_id: ObjectId,
        reason: EndReason,
    },

    /// Liveness ping (cheap; the WS handler also has its own ping/pong).
    #[serde(rename = "rc:ping")]
    Ping { id: u32 },

    // ─── tunnel-client / agent → server (rc:tunnel.*) ────────────────
    //
    // Plan v2 §"What changed from v1" #1 + #2:
    //   * Wire types fold into the existing `rc:*` namespace, NOT a
    //     separate `rc-tunnel:*` namespace or WS endpoint.
    //   * Each `roomler-tunnel forward` invocation owns ONE peer; many
    //     TCP flows multiplex onto a fixed DC pool via `flow_id`
    //     framing (see `tunnel-core::mux`). No per-flow DC creation.
    //   * Server is the auth boundary — `TcpForwardRequest` rides the
    //     WS so the server can apply the cross-tenant gate + policy
    //     eval before forwarding to the agent.
    //
    /// Sent right after WS upgrade by either a `roomler-tunnel` client
    /// or an agent that wants to advertise tunnel support. Locks in
    /// the wire transport for the rest of the session.
    ///
    /// `supported_transports` carries strings (not an enum) so a
    /// newer client and an older agent can still negotiate a common
    /// transport. v1 ships `["webrtc-dc-v1"]`; v0.5 adds
    /// `"wireguard-v1"`.
    #[serde(rename = "rc:tunnel.hello")]
    TunnelHello {
        role: TunnelRole,
        version: String,
        supported_transports: Vec<String>,
    },

    /// Client → server: open a tunnel peer-channel to a specific
    /// agent. Server applies the cross-tenant gate (rejects if
    /// `client.tenant_id != agent.tenant_id`), forwards the request
    /// to the agent's WS, and replies with `rc:tunnel.opened` once
    /// the SDP offer/answer + ICE exchange + DC pool negotiation
    /// completes (driven by the existing `rc:sdp.*` + `rc:ice` flow,
    /// keyed by the `session_id` the server assigns).
    #[serde(rename = "rc:tunnel.open")]
    TunnelOpen {
        #[serde(with = "oid_hex")]
        agent_id: ObjectId,
        /// One of `supported_transports` from the client's hello.
        transport: String,
    },

    /// Client → server (forwarded to agent): open one TCP forward.
    /// Server-side ACL gate runs HERE — the cross-tenant check fires
    /// first, then `tunnel_policies` is evaluated, then either a
    /// `TcpForwardReject` is sent back to the client OR the request
    /// is forwarded to the agent for the actual dial.
    ///
    /// `flow_id` is client-chosen + monotonic per `session_id`; it
    /// prefixes every DC message belonging to this flow (see
    /// `tunnel-core::mux::encode`). Server treats `flow_id` as opaque
    /// — only the client and agent demux on it.
    #[serde(rename = "rc:tunnel.tcp.request")]
    TcpForwardRequest {
        #[serde(with = "oid_hex")]
        session_id: ObjectId,
        flow_id: u32,
        dst_host: String,
        dst_port: u16,
    },

    /// Agent → server: agent dialed `dst_host:dst_port` for the flow
    /// and is ready to pump bytes. Server relays the accept back to
    /// the client. `dc_index` tells the client which DC in the pool
    /// has been assigned for this flow.
    #[serde(rename = "rc:tunnel.tcp.accept")]
    TcpForwardAccept {
        #[serde(with = "oid_hex")]
        session_id: ObjectId,
        flow_id: u32,
        dc_index: u8,
    },

    /// Agent → server (or server-generated on ACL deny): the flow is
    /// rejected. Server relays to the client. Servers MAY synthesise
    /// this with `RejectKind::CrossTenant` or `AclDenied` without
    /// touching the agent.
    #[serde(rename = "rc:tunnel.tcp.reject")]
    TcpForwardReject {
        #[serde(with = "oid_hex")]
        session_id: ObjectId,
        flow_id: u32,
        kind: RejectKind,
        reason: String,
    },

    /// Either side announces half-close on a flow. Carried over WS
    /// (rather than the DC) because the close needs to drive
    /// audit-log accounting in addition to the actual socket
    /// shutdown.
    #[serde(rename = "rc:tunnel.tcp.half_close")]
    TcpHalfClose {
        #[serde(with = "oid_hex")]
        session_id: ObjectId,
        flow_id: u32,
        direction: Direction,
    },

    /// Either side closes a flow (clean EOF or error). Server relays
    /// to the peer and appends to `tunnel_audit`.
    #[serde(rename = "rc:tunnel.tcp.closed")]
    TcpClosed {
        #[serde(with = "oid_hex")]
        session_id: ObjectId,
        flow_id: u32,
        reason: CloseReason,
    },

    /// Either side tears down the whole peer (Ctrl-C on the CLI,
    /// agent shutdown, etc.). Server cleans up state + audits.
    #[serde(rename = "rc:tunnel.terminate")]
    TunnelTerminate {
        #[serde(with = "oid_hex")]
        session_id: ObjectId,
        reason: CloseReason,
    },

    /// Tunnel-client → server → agent: SDP offer for the WebRTC peer
    /// negotiation. Distinct discriminator from the remote-control
    /// `rc:sdp.offer` so the server's session_id namespaces don't
    /// have to overlap. Carries no `ice_servers` (already delivered
    /// in `TunnelOpened`).
    #[serde(rename = "rc:tunnel.sdp.offer")]
    TunnelSdpOffer {
        #[serde(with = "oid_hex")]
        session_id: ObjectId,
        sdp: String,
    },

    /// Agent → server → tunnel-client: SDP answer for the WebRTC peer
    /// negotiation. Mirror of `TunnelSdpOffer` on the answerer path.
    #[serde(rename = "rc:tunnel.sdp.answer")]
    TunnelSdpAnswer {
        #[serde(with = "oid_hex")]
        session_id: ObjectId,
        sdp: String,
    },

    /// Either side trickles an ICE candidate for the tunnel peer.
    /// Server relays to the other side. Distinct discriminator from
    /// the remote-control `rc:ice` for the same reason as the SDP
    /// variants above.
    #[serde(rename = "rc:tunnel.ice")]
    TunnelIce {
        #[serde(with = "oid_hex")]
        session_id: ObjectId,
        candidate: serde_json::Value,
    },
}

// ────────────────────────────────────────────────────────────────────────────
// Outbound from server
// ────────────────────────────────────────────────────────────────────────────

/// Messages the server sends.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "t")]
pub enum ServerMsg {
    /// Sent to the controller right after `SessionRequest` so it knows the id.
    #[serde(rename = "rc:session.created")]
    SessionCreated {
        #[serde(with = "oid_hex")]
        session_id: ObjectId,
        #[serde(with = "oid_hex")]
        agent_id: ObjectId,
    },

    /// Sent to the agent when a controller asks for control. The agent prompts
    /// the user (or auto-grants per AccessPolicy) and replies with `Consent`.
    ///
    /// `browser_caps` is forwarded verbatim from the controller's
    /// `rc:session.request` (codec short names like `"h264"`,
    /// `"h265"`, etc.). The agent intersects this with its own
    /// `AgentCaps.codecs` to pick the best codec for the session.
    /// Empty on controllers that don't advertise — the agent then
    /// defaults to H.264.
    ///
    /// `preferred_transport` (Phase Y.3) is also forwarded verbatim.
    /// `None` / unset means "use the WebRTC video track" (legacy
    /// default). Recognised values match `AgentCaps.transports` —
    /// today only `data-channel-vp9-444`. The agent honours the
    /// request when its caps advertise the same transport, else
    /// falls back to the WebRTC track silently.
    #[serde(rename = "rc:request")]
    Request {
        #[serde(with = "oid_hex")]
        session_id: ObjectId,
        #[serde(with = "oid_hex")]
        controller_user_id: ObjectId,
        controller_name: String,
        permissions: Permissions,
        consent_timeout_secs: u32,
        #[serde(default)]
        browser_caps: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        preferred_transport: Option<String>,
        /// rc.62 — per-session VP9 chroma override forwarded verbatim
        /// from the controller's [`ClientMsg::SessionRequest::chroma_pref`].
        /// `None` / unset means "use the agent's
        /// `ROOMLER_AGENT_VP9_CHROMA` env-var default".
        #[serde(default, skip_serializing_if = "Option::is_none")]
        chroma_pref: Option<String>,
    },

    /// Server forwards SDP offer from controller → agent.
    #[serde(rename = "rc:sdp.offer")]
    SdpOffer {
        #[serde(with = "oid_hex")]
        session_id: ObjectId,
        sdp: String,
        ice_servers: Vec<IceServer>,
    },

    /// Server forwards SDP answer from agent → controller.
    #[serde(rename = "rc:sdp.answer")]
    SdpAnswer {
        #[serde(with = "oid_hex")]
        session_id: ObjectId,
        sdp: String,
        ice_servers: Vec<IceServer>,
    },

    /// Forward ICE candidate to the peer.
    #[serde(rename = "rc:ice")]
    Ice {
        #[serde(with = "oid_hex")]
        session_id: ObjectId,
        candidate: serde_json::Value,
    },

    /// Sent to the controller after the agent has consented and is ready for
    /// the SDP offer. Controller now creates its PeerConnection.
    #[serde(rename = "rc:ready")]
    Ready {
        #[serde(with = "oid_hex")]
        session_id: ObjectId,
        ice_servers: Vec<IceServer>,
    },

    /// Either peer is gone, or admin terminated, or consent denied.
    #[serde(rename = "rc:terminate")]
    Terminate {
        #[serde(with = "oid_hex")]
        session_id: ObjectId,
        reason: EndReason,
    },

    /// Reply to `Ping`.
    #[serde(rename = "rc:pong")]
    Pong { id: u32 },

    /// Generic error pushed to the client.
    #[serde(rename = "rc:error")]
    Error {
        #[serde(with = "option_oid_hex")]
        session_id: Option<ObjectId>,
        code: String,
        message: String,
    },

    /// Server-initiated close of an agent WS connection (rc.53).
    /// Sent immediately before the WS Close frame so the agent can
    /// surface a useful reason in its log + decide whether to
    /// reconnect or stop.
    ///
    /// Emitted at three sites:
    ///   * `crates/api/src/ws/handler.rs:189` — agent row is
    ///     deleted/quarantined, `reason = AgentDeleted`.
    ///   * `crates/remote_control/src/hub.rs::register_agent`
    ///     displacement path, `reason = ReplacedByNewerConnection`.
    ///   * (future) policy gate (account suspended, etc.),
    ///     `reason = PolicyRejected`.
    ///
    /// Pre-rc.53 agents hit their existing `Err(e) => debug!`
    /// decoder branch and ignore the message; the subsequent WS
    /// Close still fires, so no regression. Pre-rc.53 server +
    /// rc.53 agent: agent never receives Goodbye; raw close
    /// is treated as transient (no fatal exit). Both directions
    /// covered by Phase 4 back-compat tests.
    #[serde(rename = "rc:goodbye")]
    Goodbye {
        reason: AgentCloseReason,
        /// Human-readable, operator-targeted. Used verbatim in the
        /// agent's `needs-attention.txt` sentinel + the
        /// `tracing::error!` line that surfaces the close to the
        /// operator.
        message: String,
    },

    // ─── server → tunnel-client / agent (rc:tunnel.*) ────────────────
    /// Server → client: peer-channel is up. `dc_pool_size` confirms
    /// the negotiated DC pool size (8 in v1) so the client knows
    /// which `dc_index` values are valid. `sctp_rwnd_bytes` reports
    /// the advertised SCTP receive window for diagnostics — useful
    /// when verifying the vendored `webrtc-0.12.0` patch took effect
    /// at runtime (default upstream = 1 MiB, tuned native↔native =
    /// 8 MiB).
    #[serde(rename = "rc:tunnel.opened")]
    TunnelOpened {
        #[serde(with = "oid_hex")]
        session_id: ObjectId,
        transport: String,
        dc_pool_size: u8,
        sctp_rwnd_bytes: u32,
        ice_servers: Vec<IceServer>,
    },

    /// Server → agent: a tunnel-client wants to open this TCP
    /// forward; the server has already passed the cross-tenant gate
    /// and the tenant policy. Agent dials and replies with
    /// `TcpForwardAccept` or `TcpForwardReject`. Distinct
    /// discriminator from the client-side `rc:tunnel.tcp.request` —
    /// makes the agent handler's match exhaustive without ambiguity.
    #[serde(rename = "rc:tunnel.tcp.forward")]
    TcpForwardForward {
        #[serde(with = "oid_hex")]
        session_id: ObjectId,
        flow_id: u32,
        dst_host: String,
        dst_port: u16,
        /// User on whose behalf the forward is being opened. Recorded
        /// in `tunnel_audit` rows from the agent side too.
        #[serde(with = "oid_hex")]
        owner_user_id: ObjectId,
    },

    /// Server → client: relays the agent's `TcpForwardAccept`.
    #[serde(rename = "rc:tunnel.tcp.accept")]
    TcpForwardAccept {
        #[serde(with = "oid_hex")]
        session_id: ObjectId,
        flow_id: u32,
        dc_index: u8,
    },

    /// Server → client: either relays the agent's reject OR
    /// synthesises one from the server-side ACL gate.
    #[serde(rename = "rc:tunnel.tcp.reject")]
    TcpForwardReject {
        #[serde(with = "oid_hex")]
        session_id: ObjectId,
        flow_id: u32,
        kind: RejectKind,
        reason: String,
    },

    /// Server → peer: relays a half-close from the other side.
    #[serde(rename = "rc:tunnel.tcp.half_close")]
    TcpHalfClose {
        #[serde(with = "oid_hex")]
        session_id: ObjectId,
        flow_id: u32,
        direction: Direction,
    },

    /// Server → peer: relays a flow close.
    #[serde(rename = "rc:tunnel.tcp.closed")]
    TcpClosed {
        #[serde(with = "oid_hex")]
        session_id: ObjectId,
        flow_id: u32,
        reason: CloseReason,
    },

    /// Server → either peer: the whole peer is being torn down.
    /// Carries the same `CloseReason` taxonomy as flow close.
    #[serde(rename = "rc:tunnel.terminate")]
    TunnelTerminate {
        #[serde(with = "oid_hex")]
        session_id: ObjectId,
        reason: CloseReason,
    },

    /// Server → client: status changed mid-session (admin set
    /// `Quarantined`, soft-deleted the row, etc.). The WS will be
    /// closed immediately after. Mirrors the T1 stub frame the
    /// revocation re-check task already emits in `ws/tunnel.rs`.
    #[serde(rename = "rc:tunnel.revoked")]
    TunnelRevoked { reason: String },

    /// Server → agent: relays a tunnel-client's SDP offer. Distinct
    /// discriminator from `rc:sdp.offer` so the agent doesn't
    /// confuse this with a remote-control session offer.
    #[serde(rename = "rc:tunnel.sdp.offer")]
    TunnelSdpOffer {
        #[serde(with = "oid_hex")]
        session_id: ObjectId,
        sdp: String,
    },

    /// Server → tunnel-client: relays the agent's SDP answer.
    #[serde(rename = "rc:tunnel.sdp.answer")]
    TunnelSdpAnswer {
        #[serde(with = "oid_hex")]
        session_id: ObjectId,
        sdp: String,
    },

    /// Server → either peer: relays a tunnel ICE candidate. Mirror of
    /// `ClientMsg::TunnelIce`.
    #[serde(rename = "rc:tunnel.ice")]
    TunnelIce {
        #[serde(with = "oid_hex")]
        session_id: ObjectId,
        candidate: serde_json::Value,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct IceServer {
    pub urls: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_msg_roundtrip() {
        let m = ClientMsg::Ping { id: 42 };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains(r#""t":"rc:ping""#));
        let back: ClientMsg = serde_json::from_str(&s).unwrap();
        assert!(matches!(back, ClientMsg::Ping { id: 42 }));
    }

    #[test]
    fn ice_server_minimal() {
        let s = IceServer {
            urls: vec!["stun:stun.l.google.com:19302".into()],
            username: None,
            credential: None,
        };
        let j = serde_json::to_string(&s).unwrap();
        assert!(!j.contains("username"));
    }

    #[test]
    fn object_ids_serialise_as_raw_hex_on_wire() {
        // Lock-in: no `$oid` wrapping anywhere in the WS protocol envelope.
        let session_id = ObjectId::parse_str("507f1f77bcf86cd799439011").unwrap();
        let agent_id = ObjectId::parse_str("507f1f77bcf86cd799439012").unwrap();

        let created = ServerMsg::SessionCreated {
            session_id,
            agent_id,
        };
        let s = serde_json::to_string(&created).unwrap();
        assert!(
            !s.contains("$oid"),
            "extended JSON leaked into wire format: {s}"
        );
        assert!(s.contains("\"session_id\":\"507f1f77bcf86cd799439011\""));
        assert!(s.contains("\"agent_id\":\"507f1f77bcf86cd799439012\""));

        let req = ClientMsg::SessionRequest {
            agent_id,
            permissions: Permissions::VIEW | Permissions::INPUT,
            browser_caps: vec!["h264".into(), "h265".into()],
            preferred_transport: None,
        };
        let s = serde_json::to_string(&req).unwrap();
        assert!(!s.contains("$oid"));
        assert!(s.contains("\"agent_id\":\"507f1f77bcf86cd799439012\""));
        assert!(s.contains("\"browser_caps\":[\"h264\",\"h265\"]"));
        // Default None must NOT serialise — keeps the wire compatible
        // with controllers that don't know about the field at all.
        assert!(
            !s.contains("preferred_transport"),
            "None should be skipped via skip_serializing_if"
        );

        // With a value set, the field appears
        let req_with_t = ClientMsg::SessionRequest {
            agent_id,
            permissions: Permissions::VIEW,
            browser_caps: vec![],
            preferred_transport: Some("data-channel-vp9-444".into()),
        };
        let s = serde_json::to_string(&req_with_t).unwrap();
        assert!(s.contains("\"preferred_transport\":\"data-channel-vp9-444\""));
    }

    #[test]
    fn agent_heartbeat_round_trips_with_stable_field_names() {
        // Wire-format lock for Phase 7 (heartbeat telemetry). The agent
        // emits this every 30 s and the server uses it to refresh
        // `agents.last_seen_at`. Field names match the JS controllers'
        // expectations; renaming any of them is a wire break that needs
        // a coordinated agent + server release.
        let m = ClientMsg::AgentHeartbeat {
            rss_mb: 142,
            cpu_pct: 3.25,
            active_sessions: 2,
        };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains(r#""t":"rc:agent.heartbeat""#));
        assert!(s.contains(r#""rss_mb":142"#));
        assert!(s.contains(r#""cpu_pct":3.25"#));
        assert!(s.contains(r#""active_sessions":2"#));

        let back: ClientMsg = serde_json::from_str(&s).unwrap();
        match back {
            ClientMsg::AgentHeartbeat {
                rss_mb,
                cpu_pct,
                active_sessions,
            } => {
                assert_eq!(rss_mb, 142);
                assert!((cpu_pct - 3.25).abs() < f32::EPSILON);
                assert_eq!(active_sessions, 2);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn session_request_browser_caps_default_empty_for_back_compat() {
        // A pre-2B.1 controller that doesn't include browser_caps
        // must still parse — the agent will fall back to h264-only
        // negotiation in that case.
        let json = r#"{"t":"rc:session.request","agent_id":"507f1f77bcf86cd799439012","permissions":"VIEW"}"#;
        let m: ClientMsg = serde_json::from_str(json).unwrap();
        match m {
            ClientMsg::SessionRequest { browser_caps, .. } => {
                assert!(
                    browser_caps.is_empty(),
                    "missing field must default to empty"
                );
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn accepts_extended_json_for_backward_compat() {
        // A client still sending extended JSON parses fine — eases rollout.
        let json = r#"{"t":"rc:session.request","agent_id":{"$oid":"507f1f77bcf86cd799439012"},"permissions":"VIEW | INPUT"}"#;
        let m: ClientMsg = serde_json::from_str(json).unwrap();
        assert!(matches!(m, ClientMsg::SessionRequest { .. }));
    }

    #[test]
    fn error_msg_omits_null_session_id_is_ok() {
        let e = ServerMsg::Error {
            session_id: None,
            code: "x".into(),
            message: "y".into(),
        };
        let s = serde_json::to_string(&e).unwrap();
        // None → null, not omitted.
        assert!(s.contains("\"session_id\":null"));
    }

    // ─── AgentCloseReason / ServerMsg::Goodbye wire-format locks (rc.53) ───

    #[test]
    fn agent_close_reason_serialises_snake_case() {
        // Lock-in: every variant rides as its canonical snake_case
        // wire name. The agent's `handle_server_msg` match arm + the
        // server's emit sites both pivot on these strings; renaming
        // any is a wire break that strands fielded agents.
        for (variant, expected) in [
            (AgentCloseReason::AgentDeleted, "agent_deleted"),
            (
                AgentCloseReason::ReplacedByNewerConnection,
                "replaced_by_newer_connection",
            ),
            (AgentCloseReason::PolicyRejected, "policy_rejected"),
        ] {
            let m = ServerMsg::Goodbye {
                reason: variant,
                message: "x".into(),
            };
            let s = serde_json::to_string(&m).unwrap();
            assert!(
                s.contains(&format!("\"reason\":\"{expected}\"")),
                "variant {variant:?} did not serialise to {expected:?} in: {s}"
            );
            assert!(s.contains(r#""t":"rc:goodbye""#));
        }
    }

    #[test]
    fn goodbye_round_trips() {
        let m = ServerMsg::Goodbye {
            reason: AgentCloseReason::AgentDeleted,
            message: "re-enrol required".into(),
        };
        let s = serde_json::to_string(&m).unwrap();
        let back: ServerMsg = serde_json::from_str(&s).unwrap();
        match back {
            ServerMsg::Goodbye { reason, message } => {
                assert_eq!(reason, AgentCloseReason::AgentDeleted);
                assert_eq!(message, "re-enrol required");
            }
            other => panic!("expected Goodbye, got {other:?}"),
        }
    }

    #[test]
    fn pre_rc53_server_msg_rejects_goodbye_so_agent_err_arm_fires() {
        // Phase 4 back-compat lock (rc.53 plan v2).
        //
        // A pre-rc.53 agent's `ServerMsg` enum does NOT have the
        // `Goodbye` variant. When the rc.53 server emits an
        // `rc:goodbye` frame, the old agent's `serde_json::from_str`
        // returns `Err` and the existing `Err(e) => debug!(…,
        // "ignoring non-rc:* frame")` arm at
        // `agents/roomler-agent/src/signaling.rs:333` swallows it
        // silently — no panic, no fatal exit.
        //
        // We simulate the rc.52 ServerMsg shape via a stripped local
        // enum + the same `#[serde(tag = "t")]` attribute the real
        // enum uses. If serde ever started succeeding here (e.g.
        // because someone added `#[serde(other)]` to `ServerMsg` or
        // changed the tag scheme), this test fires and rc.52 hosts
        // would start panicking on the new variant.

        #[derive(Deserialize, Debug)]
        #[serde(tag = "t")]
        #[allow(dead_code)]
        enum Rc52ServerMsg {
            #[serde(rename = "rc:pong")]
            Pong { id: u32 },
            #[serde(rename = "rc:error")]
            Error { code: String },
        }

        let goodbye_json = r#"{"t":"rc:goodbye","reason":"agent_deleted","message":"x"}"#;
        let result: Result<Rc52ServerMsg, _> = serde_json::from_str(goodbye_json);
        assert!(
            result.is_err(),
            "pre-rc.53 ServerMsg shape must Err on the new Goodbye discriminator \
             (rc.52 agents rely on the `Err(_) => debug!(…)` arm to absorb it)"
        );
    }

    #[test]
    fn goodbye_with_unknown_reason_decodes_to_policy_rejected_default() {
        // Forward-compat: a fielded rc.53 agent that receives a
        // future rc.54+ `reason` string it doesn't know MUST NOT
        // panic / hard-fault. The custom `Deserialize` for
        // `AgentCloseReason` rounds unknown strings to
        // `PolicyRejected` (which the agent treats as fatal —
        // semantically "server told us to stop and we don't know
        // why" matches PolicyRejected better than ReplacedByNewer
        // which would invite a reconnect loop).
        let json = r#"{"t":"rc:goodbye","reason":"xyzzy_brand_new","message":"hi"}"#;
        let back: ServerMsg = serde_json::from_str(json).unwrap();
        match back {
            ServerMsg::Goodbye { reason, message } => {
                assert_eq!(reason, AgentCloseReason::PolicyRejected);
                assert_eq!(message, "hi");
            }
            other => panic!("expected Goodbye, got {other:?}"),
        }
    }

    // ─── rc:tunnel.* wire-format locks (T2.1) ─────────────────────────
    //
    // Every new variant gets a roundtrip test AND a discriminator-pin
    // assertion. Multi-tenant tunneling is a security boundary —
    // renaming a discriminator without coordinating client + server +
    // agent is a wire break that strands enrolled clients in the
    // field.

    #[test]
    fn tunnel_hello_roundtrip() {
        let m = ClientMsg::TunnelHello {
            role: TunnelRole::Client,
            version: "0.4.0".into(),
            supported_transports: vec!["webrtc-dc-v1".into()],
        };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains(r#""t":"rc:tunnel.hello""#));
        assert!(s.contains(r#""role":"client""#));
        assert!(s.contains(r#""supported_transports":["webrtc-dc-v1"]"#));
        let back: ClientMsg = serde_json::from_str(&s).unwrap();
        match back {
            ClientMsg::TunnelHello {
                role,
                version,
                supported_transports,
            } => {
                assert_eq!(role, TunnelRole::Client);
                assert_eq!(version, "0.4.0");
                assert_eq!(supported_transports, vec!["webrtc-dc-v1".to_string()]);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn tunnel_open_uses_raw_hex_agent_id() {
        let agent_id = ObjectId::parse_str("507f1f77bcf86cd799439012").unwrap();
        let m = ClientMsg::TunnelOpen {
            agent_id,
            transport: "webrtc-dc-v1".into(),
        };
        let s = serde_json::to_string(&m).unwrap();
        assert!(!s.contains("$oid"), "extended JSON leaked: {s}");
        assert!(s.contains(r#""agent_id":"507f1f77bcf86cd799439012""#));
        assert!(s.contains(r#""t":"rc:tunnel.open""#));
    }

    #[test]
    fn tcp_forward_request_roundtrip() {
        let session_id = ObjectId::parse_str("507f1f77bcf86cd799439011").unwrap();
        let m = ClientMsg::TcpForwardRequest {
            session_id,
            flow_id: 42,
            dst_host: "db.intranet".into(),
            dst_port: 5432,
        };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains(r#""t":"rc:tunnel.tcp.request""#));
        assert!(s.contains(r#""flow_id":42"#));
        assert!(s.contains(r#""dst_host":"db.intranet""#));
        assert!(s.contains(r#""dst_port":5432"#));
        let back: ClientMsg = serde_json::from_str(&s).unwrap();
        assert!(matches!(
            back,
            ClientMsg::TcpForwardRequest {
                flow_id: 42,
                dst_port: 5432,
                ..
            }
        ));
    }

    #[test]
    fn tcp_forward_reject_kind_serialises_snake_case() {
        // Reject taxonomy drives the audit log roll-up — locking the
        // wire form so a kind:"AclDenied" never sneaks past a
        // case-sensitive matcher.
        let session_id = ObjectId::new();
        let m = ClientMsg::TcpForwardReject {
            session_id,
            flow_id: 7,
            kind: RejectKind::AclDenied,
            reason: "no policy matches".into(),
        };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains(r#""kind":"acl_denied""#));

        let cross_tenant = ClientMsg::TcpForwardReject {
            session_id,
            flow_id: 7,
            kind: RejectKind::CrossTenant,
            reason: "x".into(),
        };
        let s = serde_json::to_string(&cross_tenant).unwrap();
        assert!(s.contains(r#""kind":"cross_tenant""#));
    }

    #[test]
    fn tcp_half_close_direction_roundtrip() {
        let m = ClientMsg::TcpHalfClose {
            session_id: ObjectId::new(),
            flow_id: 1,
            direction: Direction::SrcToDst,
        };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains(r#""direction":"src_to_dst""#));
        let back: ClientMsg = serde_json::from_str(&s).unwrap();
        assert!(matches!(
            back,
            ClientMsg::TcpHalfClose {
                direction: Direction::SrcToDst,
                ..
            }
        ));
    }

    #[test]
    fn tcp_closed_reason_roundtrip() {
        // Locks every CloseReason variant — the audit log roll-up
        // (T2.2) pivots on these strings; renaming any is a wire +
        // dashboard break.
        for r in [
            CloseReason::Eof,
            CloseReason::IoError,
            CloseReason::AgentAclDenied,
            CloseReason::ClientShutdown,
            CloseReason::ServerTerminated,
            CloseReason::IdleTimeout,
        ] {
            let m = ClientMsg::TcpClosed {
                session_id: ObjectId::new(),
                flow_id: 1,
                reason: r,
            };
            let s = serde_json::to_string(&m).unwrap();
            let back: ClientMsg = serde_json::from_str(&s).unwrap();
            match back {
                ClientMsg::TcpClosed { reason, .. } => assert_eq!(reason, r),
                _ => panic!("wrong variant"),
            }
        }
    }

    #[test]
    fn server_tunnel_opened_carries_diagnostics() {
        // dc_pool_size + sctp_rwnd_bytes are critical for the CLI's
        // diagnose subcommand — verifying the vendored webrtc patch
        // took effect at runtime needs sctp_rwnd_bytes ≥ 1 MiB.
        let session_id = ObjectId::new();
        let m = ServerMsg::TunnelOpened {
            session_id,
            transport: "webrtc-dc-v1".into(),
            dc_pool_size: 8,
            sctp_rwnd_bytes: 8 * 1024 * 1024,
            ice_servers: vec![],
        };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains(r#""t":"rc:tunnel.opened""#));
        assert!(s.contains(r#""dc_pool_size":8"#));
        assert!(s.contains(r#""sctp_rwnd_bytes":8388608"#));
    }

    #[test]
    fn server_tunnel_revoked_round_trips() {
        // Promoted from the T1 stub plain-JSON frame in
        // crates/api/src/ws/tunnel.rs. Reason field is human-readable;
        // discriminator is what handlers gate on.
        let m = ServerMsg::TunnelRevoked {
            reason: "status changed to Quarantined".into(),
        };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains(r#""t":"rc:tunnel.revoked""#));
        let back: ServerMsg = serde_json::from_str(&s).unwrap();
        assert!(matches!(back, ServerMsg::TunnelRevoked { .. }));
    }

    #[test]
    fn server_tcp_forward_forward_has_distinct_discriminator() {
        // ServerMsg uses a different variant name + discriminator
        // (`rc:tunnel.tcp.forward`) than the client-side
        // `rc:tunnel.tcp.request` so the agent's match is exhaustive
        // without an ambiguous `t` shared across enums.
        let m = ServerMsg::TcpForwardForward {
            session_id: ObjectId::new(),
            flow_id: 1,
            dst_host: "h".into(),
            dst_port: 1,
            owner_user_id: ObjectId::new(),
        };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains(r#""t":"rc:tunnel.tcp.forward""#));
        assert!(!s.contains("rc:tunnel.tcp.request"));
    }

    #[test]
    fn tunnel_terminate_uses_close_reason() {
        // Re-uses the CloseReason taxonomy from per-flow closes — one
        // taxonomy means one audit dashboard, no double maintenance.
        let m = ClientMsg::TunnelTerminate {
            session_id: ObjectId::new(),
            reason: CloseReason::ClientShutdown,
        };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains(r#""reason":"client_shutdown""#));
    }

    #[test]
    fn tunnel_sdp_offer_uses_distinct_discriminator() {
        // `rc:tunnel.sdp.offer` is distinct from the remote-control
        // `rc:sdp.offer` so the server can route by session-namespace
        // without ambiguity.
        let m = ClientMsg::TunnelSdpOffer {
            session_id: ObjectId::new(),
            sdp: "v=0\r\n".into(),
        };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains(r#""t":"rc:tunnel.sdp.offer""#));
        assert!(!s.contains(r#""t":"rc:sdp.offer""#));
        let back: ClientMsg = serde_json::from_str(&s).unwrap();
        assert!(matches!(back, ClientMsg::TunnelSdpOffer { .. }));
    }

    #[test]
    fn server_tunnel_sdp_answer_round_trips() {
        let m = ServerMsg::TunnelSdpAnswer {
            session_id: ObjectId::new(),
            sdp: "v=0\r\n".into(),
        };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains(r#""t":"rc:tunnel.sdp.answer""#));
        let back: ServerMsg = serde_json::from_str(&s).unwrap();
        assert!(matches!(back, ServerMsg::TunnelSdpAnswer { .. }));
    }

    #[test]
    fn tunnel_ice_carries_arbitrary_json_candidate() {
        // Candidates ride as opaque JSON so we don't have to mirror
        // the webrtc-rs RTCIceCandidateInit shape in this crate.
        let candidate = serde_json::json!({
            "candidate": "candidate:1 1 udp 2122252543 192.0.2.1 12345 typ host",
            "sdpMid": "0",
            "sdpMLineIndex": 0,
        });
        let m = ClientMsg::TunnelIce {
            session_id: ObjectId::new(),
            candidate: candidate.clone(),
        };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains(r#""t":"rc:tunnel.ice""#));
        let back: ClientMsg = serde_json::from_str(&s).unwrap();
        match back {
            ClientMsg::TunnelIce { candidate: c2, .. } => assert_eq!(c2, candidate),
            other => panic!("expected TunnelIce, got {other:?}"),
        }
    }
}
