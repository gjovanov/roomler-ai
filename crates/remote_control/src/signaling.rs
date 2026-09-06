// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
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

use crate::models::{AgentCaps, ConsentMode, DisplayInfo, EndReason, OsKind};
use crate::permissions::Permissions;
use crate::serde_helpers::{oid_hex, option_oid_hex, vec_oid_hex};

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
/// `roomler` CLI (which initiates forwards) from the agent
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

/// Reject reason the agent sends when a forward names a `session_id` it
/// doesn't know. Canonical signal that the agent lost its tunnel-session
/// state: its per-connection peer maps die with the WS, so after a network
/// flap + reconnect EVERY forward on a pre-flap session gets this reject,
/// forever. Tunnel clients match on it (`RejectKind::AgentError` + this
/// substring) and treat it as session death — end the session so the flow
/// supervisor re-opens with fresh state instead of failing every local
/// connection. Wire-stable: the string predates the client-side match, so
/// rejects from older agents match too. Do not reword.
pub const REJECT_REASON_SESSION_GONE: &str = "tunnel session not open on agent";

/// Reject reason the SERVER sends when it holds NO tunnel session for the
/// connection a forward arrived on (the client sent `TcpForwardRequest`
/// before/without a live `TunnelOpen`, or the server tore the session down —
/// e.g. an agent-disconnect teardown — while the client kept forwarding).
/// Same session-death class as [`REJECT_REASON_SESSION_GONE`] but raised one
/// hop earlier, at the server: clients match it too and re-open. Wire-stable,
/// do not reword. Emitted at `crates/api/src/ws/tunnel.rs` (handle_forward_request).
pub const REJECT_REASON_NO_SESSION: &str = "no open session (send rc:tunnel.open first)";

/// Reject reason the SERVER sends when a forward's `session_id` doesn't match
/// the session currently open on that connection — the client is still
/// forwarding on a stale session id after the server rebuilt/replaced it.
/// Also session-death: re-opening yields a fresh id both sides agree on.
/// Wire-stable, do not reword. Emitted at `crates/api/src/ws/tunnel.rs`.
pub const REJECT_REASON_SESSION_MISMATCH: &str = "session_id mismatch";

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

/// FR-47 — why an [`ServerMsg::OverlayJoinRefused`] was sent.
///
/// Enumerated rather than free text because the daemon acts on it: address
/// exhaustion is an operator problem that retrying cannot fix, while a
/// transient store failure is worth another attempt. The `detail` string
/// carries the human-readable specifics.
///
/// `Deserialize` is hand-written for the same reason [`AgentCloseReason`]'s
/// is: a fielded node must survive a server that learned a new reason. Unknown
/// decodes to [`Unknown`](OverlayJoinRefusal::Unknown), which the daemon
/// reports verbatim and treats as non-retryable — the conservative arm, since
/// a reason we cannot interpret is not one we can safely retry against.
#[derive(Serialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OverlayJoinRefusal {
    /// Every host ordinal in the tenant's block is leased. Retrying cannot
    /// help; the block needs to grow or devices need releasing.
    AddressSpaceExhausted,
    /// The tenant's network row or CIDR could not be resolved.
    NetworkUnavailable,
    /// A transient persistence failure. Retryable.
    StoreUnavailable,
    /// A reason this build does not know.
    Unknown,
}

impl<'de> Deserialize<'de> for OverlayJoinRefusal {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Ok(match s.as_str() {
            "address_space_exhausted" => OverlayJoinRefusal::AddressSpaceExhausted,
            "network_unavailable" => OverlayJoinRefusal::NetworkUnavailable,
            "store_unavailable" => OverlayJoinRefusal::StoreUnavailable,
            _ => OverlayJoinRefusal::Unknown,
        })
    }
}

impl OverlayJoinRefusal {
    /// Is another join attempt worth making?
    ///
    /// Only the transient arm. Exhaustion in particular must NOT retry: the
    /// daemon would hammer a server that is already telling an operator it is
    /// out of addresses, and the reconnect loop would bury the one log line
    /// that explains the outage.
    pub fn is_retryable(self) -> bool {
        matches!(self, OverlayJoinRefusal::StoreUnavailable)
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
        /// Boxed: `AgentCaps` is ~15 `Vec` fields and dominates the size of
        /// the WHOLE `ClientMsg` enum — every message in every per-peer
        /// channel (`PEER_OUTBOUND_CAP` slots each) was paying for it, though
        /// only the once-per-connection hello carries one. Serde is
        /// transparent through `Box`, so the wire format is unchanged.
        caps: Box<AgentCaps>,
        /// Subnet CIDRs the agent advertises it can route for the tunnel mesh
        /// subnet-router (from its `advertise_routes` config). Admin-gated —
        /// stored as untrusted suggestions until approved into `Agent.routes`.
        /// `#[serde(default)]` keeps pre-feature agents (no field) compatible.
        #[serde(default)]
        advertised_routes: Vec<String>,
        /// Multi-region relay PoPs: the agent can receive
        /// [`ServerMsg::RelayRegions`] and answer with
        /// [`ClientMsg::RelayProbeReport`]. Capability-gates the push — a
        /// pre-feature agent's `ServerMsg` deserializer would ERROR on the
        /// unknown variant. Absent ⇒ `false`.
        #[serde(default)]
        supports_relay_regions: bool,
        /// P6 — the OpenSSH **public** half of this device's SSH host key
        /// (`ssh-ed25519 AAAA… roomler-host`), so a caller can verify what it
        /// dialled instead of trusting it on first use.
        ///
        /// Reported here rather than in [`AgentCaps`] because it is an
        /// identity, not a capability: caps answer "what can this device do",
        /// and two devices that can do identical things still must not be
        /// interchangeable to a client checking who it reached.
        ///
        /// Empty when the device stores no host key — never SSH-enabled, or a
        /// build without the `ssh-server` feature. Empty must therefore never
        /// be read as "any key is fine"; it means "this device cannot prove
        /// itself", and a client that cares should refuse rather than fall
        /// back to TOFU. Absent from an older agent's hello deserialises the
        /// same way.
        ///
        /// ⚠️ Non-empty does NOT imply SSH is on: the key survives in the
        /// config after `ssh_enabled` goes false, so a device that once served
        /// keeps publishing.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        ssh_host_pubkey: String,
    },

    /// Agent periodic stats.
    ///
    /// The legacy top-level `rss_mb`/`cpu_pct` were hardcoded 0 by every
    /// shipped agent, so the server deliberately ignores them; real
    /// telemetry rides the optional [`AgentSysStats`] block (stats PR-5),
    /// whose ABSENCE means "not measured" — old agents simply omit it
    /// (`#[serde(default)]` server-side, additive client→server = no
    /// capability flag needed).
    #[serde(rename = "rc:agent.heartbeat")]
    AgentHeartbeat {
        rss_mb: u32,
        cpu_pct: f32,
        active_sessions: u8,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sys: Option<AgentSysStats>,
        /// NAT-traversal health: how many server-reflexive candidates this
        /// node currently advertises.
        ///
        /// `Some(0)` is the interesting value — it means the node cannot
        /// hole-punch and every peer reads it as UDP-blocked, so all its pairs
        /// degrade to the relay/DERP tier. That state existed **fleet-wide and
        /// unnoticed** until 2026-08-06 because the only signals were `debug!`
        /// log lines on each device; one server-side counter would have caught
        /// it immediately. `None` = an agent that predates the field, which is
        /// distinct from a measured zero.
        ///
        /// Additive agent→server, so no capability flag is needed (unlike a
        /// `ServerMsg` a caller awaits).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        srflx_count: Option<u8>,
        /// C4 stage 2 — the live warm TURN allocation's relayed transport
        /// address (`worker-ip:port`), when one is standing. The server
        /// stores it pair-less so a PEER whose pair to this node just died
        /// can dial the relayed address immediately — no coordination
        /// round-trip through this node's (possibly captured) control WS.
        /// `None` = no live leg, or a pre-stage-2 agent. Additive
        /// agent→server like `srflx_count`, so no capability flag.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        warm_relay: Option<String>,
        /// FR-43 P2c — updated capabilities, when they have CHANGED since the
        /// last announcement.
        ///
        /// Caps otherwise travel exactly once, in `rc:agent.hello`, and that is
        /// too early for a macOS daemon: it must say hello immediately (the
        /// whole point of the root half is being reachable before anyone logs
        /// in), while the GUI worker whose `permissions` it reports attaches on
        /// its own schedule — ~260 ms later at boot, but also after a
        /// console-user change, after a hand-back, and never at all when nobody
        /// is logged in.
        ///
        /// ⚠️ Sent ONLY on change, never on every beat: caps are ~200 bytes
        /// against a frequent heartbeat, and the steady state must stay free.
        /// `None` therefore means "nothing to report", NOT "no capabilities" —
        /// a reader must leave the stored caps alone, exactly like
        /// `AgentCaps::permissions`' own `None` vs `Some([])` rule.
        ///
        /// ⚠️ It must also carry the caps BACK DOWN when a worker detaches. A
        /// row that keeps claiming a capture target which has gone hands the
        /// next session a black screen, which is the bug P2b existed to fix.
        ///
        /// ⚠️ Boxed: `AgentCaps` is large, and inline it made this variant
        /// ~536 bytes — a cost every frequent `AgentHeartbeat` and every other
        /// variant would pay for a field that is almost always absent. Serde is
        /// transparent through `Box`, so the wire is unchanged. Same reasoning
        /// as `localapi::Response::Status`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        caps: Option<Box<AgentCaps>>,
        /// FR-27 — the version of the `roomler-desktop` companion installed on
        /// this host, or `None` when none is installed / the probe failed /
        /// the agent predates the field. Additive agent→server like
        /// `srflx_count`, so no capability flag.
        ///
        /// Reported because the daemon and the companion update by DIFFERENT
        /// mechanisms on all three platforms, so a fleet-wide "Update all"
        /// moving `agent_version` says nothing about the companion — which is
        /// exactly the skew the operator hit on macOS.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        companion_version: Option<String>,
    },

    /// Multi-region relay PoPs: the agent's timed STUN probe results for the
    /// regions last pushed in [`ServerMsg::RelayRegions`]. The server derives
    /// the agent's `relay_home` (with hysteresis) and persists it on the
    /// `agents` + `overlay_nodes` rows.
    #[serde(rename = "rc:relay.probe_report")]
    RelayProbeReport { results: Vec<RelayRegionRtt> },

    /// Live remote-control session telemetry (wave 2). Sent by the agent
    /// every ~15 s while a session is active; the server folds it into
    /// `remote_sessions.stats`, which was declared-but-never-written
    /// since Phase 4 (every session recorded zeros).
    #[serde(rename = "rc:session.stats")]
    SessionStats {
        session_id: String,
        bytes_sent: u64,
        bytes_recv: u64,
        fps: f32,
        rtt_ms: f32,
        keyframe_requests: u32,
        input_events: u64,
        /// P8 Phase 4 (mixed-dial gate) — cumulative seconds this
        /// session's pipeline served ≥1 follower. `serde(default)`
        /// so pre-Phase-4 agents' samples still parse.
        #[serde(default)]
        shared_seconds: u64,
        /// … of which the viewers' dials (Priority / resolution pick)
        /// were NOT all equal — the share-shape a per-viewer pipeline
        /// (SVC / tiered transcode) would serve better than the
        /// floor-merge. The 2-4-week aggregate of this field is the
        /// SVC go/no-go input.
        #[serde(default)]
        mixed_dial_seconds: u64,
    },

    /// Multi-region DERP: request an EdDSA admission ticket for the regional
    /// relays (`derp_url`s in [`ServerMsg::RelayRegions`]). The server answers
    /// with [`ServerMsg::DerpTicket`]. Sent only by agents that saw a region
    /// list carrying a DERP endpoint, so an old server never receives it.
    #[serde(rename = "rc:relay.derp_ticket_request")]
    DerpTicketRequest {},

    // ─── Fleet RPC (rc:rpc.*) ────────────────────────────────────────
    /// Result of a [`ServerMsg::RpcExec`]. Always sent — a gate-4 refusal, a
    /// timeout, or a cancel come back carrying `error` rather than as
    /// silence, because a caller is blocked on this.
    #[serde(rename = "rc:rpc.result")]
    RpcResult {
        request_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
        /// Redacted (agent token / `Bearer …` / JWT-shaped strings masked)
        /// and capped at the request's `max_output_bytes`.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        stdout: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        stderr: String,
        /// Output hit the cap and was cut.
        #[serde(default)]
        truncated: bool,
        #[serde(default)]
        duration_ms: u64,
        /// Set when the command never ran or was killed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },

    /// Device-originated exec against ANOTHER device — the `roomler exec`
    /// CLI leg, where the local daemon's own agent WS carries the request so
    /// the CLI needs no user credentials of its own.
    ///
    /// The server resolves this device's `owner_user_id` as the acting
    /// principal, requires that user to hold `EXEC_DEVICE`, and additionally
    /// requires THIS device to be blessed with
    /// [`crate::models::ExecPolicy::can_originate`] — otherwise a single
    /// compromised laptop would inherit its owner's exec rights fleet-wide.
    /// Answered with [`ServerMsg::RpcExecResponse`].
    #[serde(rename = "rc:rpc.request")]
    RpcExecRequest {
        /// Client-minted correlation id, echoed on the response.
        request_id: String,
        /// Target device: an agent id (hex) or its device name.
        target: String,
        shell: String,
        command: String,
        timeout_ms: u64,
    },

    /// Device-originated roomler-SSH against ANOTHER device — the
    /// `roomler ssh` CLI leg, the twin of [`Self::RpcExecRequest`].
    ///
    /// The caller mints an **ephemeral** keypair per session and sends only
    /// the public half. If the server authorizes, it pushes the key to the
    /// target as a single-use [`ServerMsg::SshGrant`] and answers here with
    /// where to connect. Nothing long-lived is distributed and no secret
    /// crosses the wire: the private half never leaves the calling device, and
    /// the target learns a key it will accept exactly once, briefly.
    #[serde(rename = "rc:ssh.request")]
    SshRequest {
        /// Client-minted correlation id, echoed on the response.
        request_id: String,
        /// Target device: an agent id (hex) or its device name.
        target: String,
        /// OpenSSH public key of the ephemeral session keypair
        /// (`ssh-ed25519 AAAA…`).
        public_key: String,
        /// Requested session lifetime in seconds. Server-clamped to
        /// [`crate::models::ssh_limits::MAX_SESSION_SECS`]; 0 ⇒ the ceiling.
        #[serde(default)]
        session_secs: u64,
    },

    /// Device reports one thing that happened inside an SSH session (P8).
    ///
    /// Fire-and-forget: no reply, and the server drops it silently if the
    /// device is not entitled to report. A caller never awaits this, so unlike
    /// `rc:rpc.exec` it needs no capability gate — an older server that has
    /// never heard of the frame just ignores it, which is the correct
    /// behaviour for a log line.
    ///
    /// ⚠️ `tenant_id` / `agent_id` are taken from the authenticated WS, NOT
    /// from this frame, so a device can only ever write rows about itself.
    #[serde(rename = "rc:ssh.activity")]
    SshActivity {
        /// The grant this session redeemed; absent for a key-list session.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        grant_id: Option<String>,
        /// Principal as the device saw it.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        caller: String,
        kind: crate::models::SshActivityKind,
        /// Command for `Exec`, `host:port` for `Forward`. Already redacted and
        /// length-capped by the device.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
        /// `false` when the device itself refused the action.
        allowed: bool,
    },

    /// The agent's answer to a [`ServerMsg::ConfigPush`] — what it actually
    /// did (`docs/remote-config.md`).
    ///
    /// This is the frame `ConfigPush::revision`'s doc has always promised:
    /// without it, a device that REFUSED a push and a device that never heard
    /// about one are indistinguishable on the dashboard, and only one of those
    /// is something an operator can fix.
    ///
    /// Sent on every outcome, including the refusals — especially the
    /// refusals. `not_opted_in` and `not_primary` are the two states an
    /// operator will actually hit, and both have a concrete next action that
    /// they can only take if they are told.
    ///
    /// ⚠️ `tenant_id` / `agent_id` come from the authenticated WS, NOT from
    /// this frame, so a device can only ever report about itself — the same
    /// rule as [`Self::SshActivity`].
    ///
    /// ⚠️ Unlike `SshActivity` this one IS capability-gated on the server's
    /// read side ([`RpcCap::ConfigReport`]): rc.457/rc.458 agents apply pushed
    /// config and stay silent, so "no report" has to mean "cannot report"
    /// there, and "has not answered yet" on a newer device.
    ///
    /// [`RpcCap::ConfigReport`]: crate::models::RpcCap::ConfigReport
    #[serde(rename = "rc:agent.config_status")]
    ConfigStatus {
        /// The `desired_config.revision` being reported on, echoed from the
        /// push.
        revision: u64,
        outcome: crate::models::ConfigOutcome,
        /// Keys now IN FORCE.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        live: Vec<String>,
        /// Keys written but waiting for a restart. Never merged into `live` —
        /// see [`crate::models::ConfigReport::needs_restart`].
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        needs_restart: Vec<String>,
        /// Why, for `failed`. Redacted and capped by the device; re-clamped on
        /// receipt.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },

    /// FR-40 — the device's answer to [`ServerMsg::KeyRotate`], sent on the
    /// session that received the order (that session is about to end: a
    /// `rotated` device reconnects immediately under its new key).
    ///
    /// Sent on EVERY outcome, refusals included — the refusals are what an
    /// operator can act on. `tenant_id` / `agent_id` come from the
    /// authenticated WS, never this frame. Keys here are PUBLIC halves only.
    #[serde(rename = "rc:agent.key_rotated")]
    KeyRotated {
        request_id: String,
        outcome: crate::models::KeyRotationOutcome,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        old_public_key: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        new_public_key: Option<String>,
        /// The epoch the device will present on its next join.
        #[serde(default)]
        key_epoch: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
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
        /// FR-27 — why, when `granted` is false. Absent (every pre-FR-27
        /// agent) means an ordinary deny, which is what a bare `false` has
        /// always meant. Present values come from
        /// [`crate::consent::ConsentDenyReason::wire`]; an unrecognised one
        /// degrades to the same ordinary deny, so a newer agent can name a
        /// reason this server has never heard of.
        ///
        /// This exists because the agent's OWN prompt timeout produced a bare
        /// `false`, and the hub turned that into `EndReason::UserDenied` — so
        /// "nobody was at the machine" reached the controller as "a human
        /// refused you", which is both wrong and un-actionable.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },

    /// FR-34 — the on-host consent prompt is up, but the host is LOCKED, so
    /// nobody can see it until the machine is unlocked. Sent once when the
    /// prompt begins on a locked host so the controller's "awaiting consent"
    /// wait can explain WHY (walk over, unlock, approve) instead of looking
    /// like a hang. Advisory + additive: an old server ignores it, and it
    /// never changes the consent outcome — a locked host is still answered by
    /// unlocking and clicking the panel (the sound flow), just with 5 minutes
    /// (FR-34 P4) to get there.
    #[serde(rename = "rc:consent.pending")]
    ConsentPending {
        #[serde(with = "oid_hex")]
        session_id: ObjectId,
        host_locked: bool,
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
        /// agent's `ROOMLERD_VP9_CHROMA` env-var default". Only
        /// applies when `preferred_transport` is `data-channel-vp9-444`;
        /// ignored otherwise. Forwarded verbatim to the agent in the
        /// matching server-side [`ServerMsg::SessionRequest`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        chroma_pref: Option<String>,
        /// FR-17 — the controller can parse the framed DataChannel wire
        /// format (every message prefixed with
        /// `[frame_seq | chunk_idx | chunk_count]`). Sent only when the
        /// agent advertised `chunk-framing` in `AgentCaps.video`, so the
        /// two ends can never disagree about bytes already in flight.
        /// `None` / unset (older controllers) means the legacy unframed
        /// format, which is reassemblable only because the channel is
        /// reliable + ordered.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        chunk_framing: Option<bool>,
        /// Opt-in system/desktop audio. When `true` the controller wants
        /// the agent to add a WebRTC Opus audio track (the agent must
        /// also advertise `"opus"` in `AgentCaps.audio` and be built with
        /// the `audio` feature). `#[serde(default)]` → older controllers
        /// that omit the field get no audio track (silent-by-default).
        #[serde(default)]
        audio_enabled: bool,
        /// Phase 5 — admin break-glass. When an `ADMINISTRATOR` force-starts a
        /// session against a device they don't own, this carries the mandatory
        /// reason. The API gate VALIDATES it (admin + non-owner); a non-admin
        /// setting it has no effect. A validated override skips consent (`Auto`)
        /// and is recorded as `AuditKind::AdminOverride`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        override_reason: Option<String>,
        /// Loopback-TURN corp-relay (Phase 2). When the controller's browser
        /// discovered a local-agent TURN via its loopback probe, it forwards the
        /// descriptor here so the Hub appends `turn:{overlay_ip}:{turn_port}` to
        /// the REMOTE agent's ICE servers — letting a corp-Chrome viewer that
        /// can't punch direct relay through the local agent's overlay instead of
        /// the capped far coturn. `#[serde(default)]` → older controllers that
        /// omit it get today's behaviour. See [`LocalRelayDescriptor`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        local_relay: Option<LocalRelayDescriptor>,
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
    //   * Each `roomler forward` invocation owns ONE peer; many
    //     TCP flows multiplex onto a fixed DC pool via `flow_id`
    //     framing (see `tunnel-core::mux`). No per-flow DC creation.
    //   * Server is the auth boundary — `TcpForwardRequest` rides the
    //     WS so the server can apply the cross-tenant gate + policy
    //     eval before forwarding to the agent.
    //
    /// Sent right after WS upgrade by either a `roomler` client
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
        /// Client-chosen correlation id, echoed verbatim on the matching
        /// `TunnelOpened` (success) or open-failure `Error`. Lets ONE WS
        /// carry N concurrent opens: the `roomlerd` daemon (P3b-2)
        /// multiplexes many client sessions over its single agent WS and
        /// demuxes the reply by this nonce (post-open it switches to the
        /// server-minted `session_id`). The standalone `roomler`
        /// CLI has a single in-flight open and sends `None`, matching the
        /// reply positionally. `None` is omitted on the wire, so a
        /// pre-P3b-2 server/client stays byte-identical.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        open_nonce: Option<String>,
        /// R4 (`quic-derp-v1`): the CLIENT's overlay/DERP pubkey (64
        /// lowercase hex chars), so the agent can address its tunnel QUIC
        /// endpoint at this client over the established `/derp` WS. Only
        /// sent when the client can serve the derp flavor; the server
        /// copies it into `TunnelQuicSetup.client_derp_pubkey` when the
        /// flavor is negotiated. Absent ⇒ omitted on the wire (pre-R4
        /// byte-identical).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        derp_pubkey: Option<String>,
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
    ///
    /// The byte counts are the flow's final totals, reported by the
    /// endpoint because the server never sees them: tunnel payload
    /// rides the peer-to-peer data channel, which is why the
    /// `tunnel_audit.bytes_in`/`bytes_out` columns held a literal 0 on
    /// every row written before this. TCP-side application payload (not
    /// framed DC bytes), so the number is comparable across the WebRTC
    /// and QUIC transports. `#[serde(default)]` ⇒ a pre-wave-3 client
    /// still parses and simply books zero.
    #[serde(rename = "rc:tunnel.tcp.closed")]
    TcpClosed {
        #[serde(with = "oid_hex")]
        session_id: ObjectId,
        flow_id: u32,
        reason: CloseReason,
        /// Peer → local app (what the operator's tool received).
        #[serde(default)]
        bytes_in: u64,
        /// Local app → peer (what the operator's tool sent).
        #[serde(default)]
        bytes_out: u64,
    },

    /// Client → server (→ agent): open one UDP forward (SOCKS5 UDP
    /// ASSOCIATE). Gated exactly like [`TcpForwardRequest`] but the
    /// server evaluates the policy with `proto = udp`. `flow_id` is
    /// client-chosen + monotonic per `session_id`; one UDP flow is
    /// opened per distinct `(dst_host, dst_port)` the app addresses
    /// within a single SOCKS association. Datagrams for the flow are
    /// carried length-prefixed over the negotiated transport (DC: one
    /// `mux`-framed message per datagram; QUIC: a per-flow bidi stream
    /// of `[u16 len | datagram]`). Association lifetime = the SOCKS
    /// control TCP connection on the client; individual flows
    /// idle-close.
    #[serde(rename = "rc:tunnel.udp.request")]
    UdpForwardRequest {
        #[serde(with = "oid_hex")]
        session_id: ObjectId,
        flow_id: u32,
        dst_host: String,
        dst_port: u16,
    },

    /// Agent → server → client: agent bound a UDP socket for the flow
    /// and is ready to relay datagrams. Mirrors [`TcpForwardAccept`];
    /// `dc_index` selects the pool DC for the WebRTC transport (0 for
    /// QUIC, which has no DC pool).
    #[serde(rename = "rc:tunnel.udp.accept")]
    UdpForwardAccept {
        #[serde(with = "oid_hex")]
        session_id: ObjectId,
        flow_id: u32,
        dc_index: u8,
    },

    /// Agent → server (or server-synthesised on ACL deny): the UDP flow
    /// is rejected. Mirrors [`TcpForwardReject`].
    #[serde(rename = "rc:tunnel.udp.reject")]
    UdpForwardReject {
        #[serde(with = "oid_hex")]
        session_id: ObjectId,
        flow_id: u32,
        kind: RejectKind,
        reason: String,
    },

    /// Either side closes a UDP flow (idle-timeout, or the client's
    /// SOCKS association tore down). No half-close — UDP is
    /// datagram-oriented, there is no read-half to shut. Server relays
    /// to the peer + appends to `tunnel_audit` like [`TcpClosed`].
    #[serde(rename = "rc:tunnel.udp.closed")]
    UdpClosed {
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

    /// Agent → server → tunnel-client: the agent's QUIC server endpoint
    /// is up. Carries the SHA-256 fingerprint of the agent's ephemeral
    /// self-signed cert (the client PINS it — there is no CA) plus the
    /// candidate address(es) to dial. The QUIC analogue of
    /// `TunnelSdpAnswer`. Phase 1 ships direct/host `addrs`; Phase 2
    /// adds STUN server-reflexive candidates.
    #[serde(rename = "rc:tunnel.quic.ready")]
    TunnelQuicReady {
        #[serde(with = "oid_hex")]
        session_id: ObjectId,
        /// Lowercase-hex SHA-256 of the agent's ephemeral cert (DER).
        cert_fingerprint: String,
        /// `ip:port` candidates the client may dial (priority order).
        addrs: Vec<String>,
        /// R4 (`quic-derp-v1`): the AGENT's overlay/DERP pubkey (64 hex),
        /// which the client's DERP-backed QUIC endpoint addresses. Only
        /// present on derp-flavor sessions; absent ⇒ omitted (pre-R4
        /// byte-identical).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        derp_pubkey: Option<String>,
    },

    /// Tunnel-client → server → agent: the client's own QUIC candidate
    /// address(es) — specifically its TURN-relayed address when the
    /// session uses QUIC-over-TURN (Tier 2/3). The agent needs this to
    /// install a TURN permission for the client's relay address BEFORE
    /// the QUIC handshake: the agent is the QUIC *server* and never
    /// sends first, so without a permission pre-installed for the
    /// client's relay address coturn would drop the client's opening
    /// Initial packets. The client→agent mirror of `TunnelQuicReady`'s
    /// agent→client `addrs`. Phase 3c.
    #[serde(rename = "rc:tunnel.quic.candidate")]
    TunnelQuicCandidate {
        #[serde(with = "oid_hex")]
        session_id: ObjectId,
        /// `ip:port` candidates — the client's relay address for Tier
        /// 2/3, optionally plus host/srflx. Priority order.
        addrs: Vec<String>,
    },

    // ─── overlay node → server (rc:overlay.*) ────────────────────────
    /// Node (agent or tunnel-client) announces itself to the overlay and
    /// registers its WireGuard static public key. The server does IPAM
    /// (allocating or rehydrating the node's overlay IP), persists the
    /// `OverlayNode`, and replies with a full `rc:overlay.netmap`.
    #[serde(rename = "rc:overlay.join")]
    OverlayJoin {
        /// Optional hint for which network to join (multi-network is a
        /// later phase; today the tenant has exactly one).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        network_hint: Option<String>,
        /// base64 Curve25519 public key.
        wg_public_key: String,
        #[serde(default)]
        key_epoch: u32,
        /// Transports the node speaks (`["wireguard-v1", ...]`).
        #[serde(default)]
        supported: Vec<String>,
        /// Node's overlay MTU preference (server clamps to the network).
        mtu: u16,
        /// Initial connectivity candidates (host/srflx/relay).
        #[serde(default)]
        endpoints: Vec<String>,
        /// rc.142 — the node can carry WG over a QUIC-over-TURN relay carrier
        /// (`ROOMLERD_OVERLAY_QUIC=1`). The server persists it and echoes it
        /// per-peer in the netmap so QUIC is only attempted when BOTH ends
        /// advertise it (a QUIC/raw split would silently break the pair).
        #[serde(default)]
        supports_quic: bool,
        /// Phase D — the node can carry WG over the v1 single-relay carrier (ONE
        /// anchor allocation + a raw dialer, `ROOMLERD_OVERLAY_RELAY_SINGLE=1`).
        /// Persisted + echoed per-peer like `supports_quic`, so single-relay is
        /// only chosen when BOTH ends advertise it (a mixed pair stays on the
        /// both-allocate relay). Absent from a pre-Phase-D node ⇒ `false`.
        #[serde(default)]
        supports_relay_single: bool,
        /// Phase D (DERP) — the node can carry WG over the pubkey-addressed
        /// `/derp` WS relay (`ROOMLERD_OVERLAY_DERP=1`), the last-resort
        /// carrier for two BOTH-UDP-blocked peers. Persisted + echoed per-peer
        /// like `supports_relay_single`, so DERP is only chosen when BOTH ends
        /// advertise it. Absent from a pre-DERP node ⇒ `false`.
        #[serde(default)]
        supports_derp: bool,
        /// P7 (corp-DERP fallback) — the node honors the server's
        /// [`ServerMsg::OverlayForceDerp`] per-pair escalation push (it can pin
        /// a churning TURN pair onto the DERP carrier mid-run). The capability
        /// flag — not a version — gates the server: it only escalates a pair
        /// when BOTH ends advertise this. Absent from a pre-P7 node ⇒ `false`.
        #[serde(default)]
        supports_forced_derp: bool,
        /// U2 — the node accepts a SERVER-COMPUTED relay-tier verdict
        /// ([`NetmapPeer::relay_strategy`]) and uses it in place of its own
        /// local `relay_strategy()` derivation. The server populates the
        /// per-edge verdict ONLY when BOTH ends advertise this (an unflagged
        /// end computes locally from its own — possibly frozen — UDP-capability
        /// view the server can't reproduce, so a one-sided verdict would
        /// manufacture anchor/dialer role disagreements). Absent from a
        /// pre-U2 node ⇒ `false` ⇒ every pair keeps the client-authoritative
        /// path. Advertised only when the node's own
        /// `OVERLAY_SERVER_RELAY_STRATEGY` env/config is on.
        #[serde(default)]
        supports_server_relay_strategy: bool,
        /// Phase A (overlay v3) — the node runs the DERP always-on floor: its
        /// central `/derp` mux is open and registered for the whole session
        /// (not just when its srflx gather came up empty), so a pair may be
        /// floored on DERP at birth without predicting mux state. The server
        /// echoes it per-peer; the floor (and any measured DERP keying) is
        /// gated on BOTH ends advertising it — a pre-floor peer whose srflx
        /// gather succeeded holds no mux and never registers, so a floor
        /// toward it would blackhole. Absent ⇒ `false`.
        #[serde(default)]
        supports_derp_floor: bool,
        /// Data-probe — the node's overlay engine answers the overlay-native
        /// echo probe inline (the non-ICMP half of the hybrid carrier
        /// data-probe). Persisted + echoed per-peer so a prober prefers the
        /// engine-guaranteed echo toward capable peers (netstack-only nodes
        /// have no OS ICMP responder) and falls back to the ICMP probe for
        /// the rest. Absent from an older node ⇒ `false` ⇒ ICMP.
        #[serde(default)]
        supports_overlay_echo: bool,
        /// FR-19 — this node UNDERSTANDS `rc:overlay.relay_session` /
        /// `rc:overlay.relay_revoke` and the `org-relay` verdict. The server
        /// never pushes those to a node that has not said so: an unknown
        /// `ServerMsg` tag is dropped at `debug!` on the agent, which would make
        /// a minted session evaporate silently. Absent from an older node ⇒
        /// `false`. ⚠️ Distinct from the `relay-server` RPC verb, which says
        /// "I SERVE relays" — this says "I can USE one".
        #[serde(default)]
        supports_org_relay: bool,
        /// FR-47 — this node understands [`ServerMsg::OverlayJoinRefused`], so
        /// a join the server cannot complete comes back as a stated reason
        /// instead of silence. Absent from an older node ⇒ `false`, and such a
        /// node keeps exactly the pre-FR-47 behaviour: it waits for a netmap
        /// that never arrives, and the server-side ERROR log is the only
        /// signal. Deliberately NOT persisted — the refusal decision is made
        /// inside the same handler that parsed this join, so there is nothing
        /// to remember.
        #[serde(default)]
        supports_join_refusal: bool,
        /// FR-19 (P3c) — whether this join comes from the device's PRIMARY
        /// org. Relay serving and relay use are primary-only: a UDP listener
        /// is host-global, and a secondary org's admin must not be able to
        /// mint sessions onto the device owner's listener — the same trust
        /// line `rc:agent.update` draws. `None` (a build that does not say)
        /// is treated as NOT primary for every relay decision: fail closed.
        /// P4 sets it from `OrgCtx`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        org_primary: Option<bool>,
        /// FR-19 (P3c) — the UDP port this node's org-relay server listens
        /// on when it serves one; the mint pairs it with the node's public
        /// addresses. `None` ⇒ `peer_relay_limits::DEFAULT_RELAY_PORT`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        relay_port: Option<u16>,
        /// Phase 1 — subnet CIDRs this node offers to route for peers
        /// (`--advertise-routes` / config). The server stores them as *claimed*
        /// routes; an admin must **approve** each before it's distributed in the
        /// netmap `routes`. Empty for a normal node (`#[serde(default)]`).
        #[serde(default)]
        advertised_routes: Vec<String>,
    },

    /// Node trickles updated connectivity candidates; the server fans a
    /// delta to permitted peers.
    #[serde(rename = "rc:overlay.endpoints")]
    OverlayEndpoints { candidates: Vec<String> },

    /// Node trickles its server-reflexive (srflx) candidates — its public
    /// `ip:port` discovered via STUN on its own traffic sockets (NAT-traversal
    /// Phase B). The server stores them in a SEPARATE `srflx_endpoints` bucket
    /// (so the relay-endpoint trickle can't clobber them) and fans a delta to
    /// permitted peers, who may then dial this node directly through its NAT.
    /// Deliberately distinct from `rc:overlay.endpoints` (relay addresses):
    /// different lifecycle, different provenance, different bucket.
    ///
    /// `nat` (Phase C) is this node's probed NAT mapping type — `"cone"` (hole-
    /// punchable) or `"symmetric"` (not) — or omitted when it couldn't be probed
    /// ("unknown"). The server stores it alongside the srflx so peers can skip a
    /// futile punch when BOTH ends are symmetric. Optional + omitted-when-None ⇒
    /// a pre-Phase-C agent (rc.199, sends only `candidates`) still parses.
    ///
    /// `udp_dialer_ok` (dialer honesty, 2026-08-16) — whether this node can
    /// raw-UDP-dial arbitrary high ports (relay band), as opposed to merely
    /// having reached a well-known STUN port. New agents ALWAYS send
    /// `Some(_)` — field presence is the capability signal (an old server
    /// ignores it; an old agent omits it and every pair against it keeps
    /// the legacy srflx-only role inputs).
    #[serde(rename = "rc:overlay.srflx")]
    OverlaySrflx {
        candidates: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        nat: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        udp_dialer_ok: Option<bool>,
    },

    /// Phase B (overlay v3) — the node's MEASURED capability vector
    /// (netcheck): relay-band reachability over the exact dialer path,
    /// STUN/NAT snapshot, `/derp` WS health. Sent after each measurement
    /// pass (startup / netstate Major / ~20 min cadence). The server stores
    /// it with a receipt stamp and surfaces it per-peer behind a FRESHNESS
    /// gate — a stalled prober's vector must never stay fleet-
    /// authoritative. Unknown to a pre-B2 server ⇒ logged-and-ignored on
    /// the agent socket (no capability gate needed).
    #[serde(rename = "rc:overlay.netcheck")]
    OverlayNetcheck { caps: CapVectorWire },

    /// Node leaves the overlay (graceful). Server marks it offline and
    /// pushes a `netmap_delta` removing it from peers.
    #[serde(rename = "rc:overlay.leave")]
    OverlayLeave {},

    /// FR-19 P1 — reachability report: can THIS node reach an offered org
    /// relay's endpoint? Pairwise (node × relay × endpoint), which is why it is
    /// its own message and not a `CapVector` field — the vector is one
    /// process-global slot per node. Unknown to a pre-FR-19 server ⇒
    /// logged-and-ignored on the agent socket, no capability gate needed.
    #[serde(rename = "rc:overlay.relay_probe")]
    OverlayRelayProbe {
        #[serde(with = "oid_hex")]
        relay_node_id: ObjectId,
        /// The `ip:port` probed.
        endpoint: String,
        reachable: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rtt_ms: Option<u32>,
    },

    /// Node asks for short-lived coturn credentials to stand up a relay
    /// leg to a specific peer (used when direct hole-punch to that peer
    /// fails). The server replies with `rc:overlay.relay_grant` carrying
    /// creds keyed by the symmetric `pair_key`.
    ///
    /// U1 — the request now carries the requester's EVIDENCE, so the P7
    /// churn escalation can act on what actually happened instead of
    /// inferring everything from arrival timing. All three fields are
    /// additive with serde defaults: an old client omits them, an old
    /// server ignores them.
    #[serde(rename = "rc:overlay.relay_request")]
    OverlayRelayRequest {
        #[serde(with = "oid_hex")]
        peer_node_id: ObjectId,
        /// Which relay flavour this request is REPLACING (`"turn"` /
        /// `"derp"`); absent = a fresh establishment, which the churn
        /// counter should never mistake for a died-carrier cycle.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        current_kind: Option<String>,
        /// Why the prior carrier died (the `DeathReason` short string) —
        /// diagnostic evidence for the server's escalation logs.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        /// STICKY failure evidence: this client recently ATTEMPTED to open a
        /// `/derp` mux and failed (the `force_derp` veto condition). While
        /// set, the server must not choose or hold forced-DERP for this
        /// pair — a client that vetoes the pin while the server refuses TURN
        /// grants for the pin's TTL is a hard dark window (the silent-veto
        /// bug). Deliberately NOT "is a mux open": the mux opens lazily on
        /// first use, so a healthy client that never needed DERP has none.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        derp_mux_failed: bool,
    },

    /// C4 stage 1 — pair-less coturn credentials for the node's standing
    /// WARM allocation (one TURN/UDP allocation established while UDP
    /// works and kept alive so corp-VPN flow-grandfathering preserves a
    /// UDP relay leg — see `docs/overlay-warm-relay.md`). Request-driven,
    /// so no hello capability flag is needed: an agent that predates the
    /// feature never sends this, and therefore never sees the grant.
    /// The grant itself confers no reach — per-peer permissions still go
    /// through the ACL-checked `rc:overlay.relay_request` at pairing time.
    #[serde(rename = "rc:overlay.warm_relay_request")]
    OverlayWarmRelayRequest {},
}

// ────────────────────────────────────────────────────────────────────────────
// FR-69 D7 — who owns each client message
// ────────────────────────────────────────────────────────────────────────────

/// The server module that owns a client message (FR-69 D7). The wire stays
/// ONE socket and ONE `rc:*` enum; this is the map that tells the socket
/// which module's handler a message belongs to. The prefix is NOT the owner:
/// `rc:consent*` is fleet's (one consent payload for RC, exec and SSH since
/// FR-27), `rc:relay.*` is network's, and `rc:agent.key_rotated` is
/// network's (overlay-key rotation) although it arrives on the agent's
/// `rc:agent.*` lane. So the map is explicit per variant, exhaustive, and
/// locked by the tests below.
///
/// The wire crate is MPL and agent-linked, so this names modules by id and
/// never by type; `roomler-core` checks the ids against its module graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Owner {
    /// Device management: the agent's own lane (hello, heartbeat, config
    /// status), fleet RPC, owner consent, the socket keepalive.
    Fleet,
    /// Remote-desktop sessions: request, SDP, ICE, terminate, stats.
    Remote,
    /// The mesh and what rides it: overlay, tunnels, relays and DERP
    /// tickets, SSH, key rotation.
    Network,
}

impl Owner {
    /// The module id, as `roomler_core::graph::MODULES` spells it.
    pub const fn id(self) -> &'static str {
        match self {
            Owner::Fleet => "fleet",
            Owner::Remote => "remote",
            Owner::Network => "network",
        }
    }
}

impl ClientMsg {
    /// The wire tag this variant serialises as (`t`) — the same string as its
    /// `#[serde(rename)]`, spelled out so a variant can be named without an
    /// instance. Exhaustive: a new variant does not compile until it is here.
    pub fn wire_tag(&self) -> &'static str {
        match self {
            ClientMsg::AgentHello { .. } => "rc:agent.hello",
            ClientMsg::AgentHeartbeat { .. } => "rc:agent.heartbeat",
            ClientMsg::RelayProbeReport { .. } => "rc:relay.probe_report",
            ClientMsg::SessionStats { .. } => "rc:session.stats",
            ClientMsg::DerpTicketRequest { .. } => "rc:relay.derp_ticket_request",
            ClientMsg::RpcResult { .. } => "rc:rpc.result",
            ClientMsg::RpcExecRequest { .. } => "rc:rpc.request",
            ClientMsg::SshRequest { .. } => "rc:ssh.request",
            ClientMsg::SshActivity { .. } => "rc:ssh.activity",
            ClientMsg::ConfigStatus { .. } => "rc:agent.config_status",
            ClientMsg::KeyRotated { .. } => "rc:agent.key_rotated",
            ClientMsg::SdpAnswer { .. } => "rc:sdp.answer",
            ClientMsg::Consent { .. } => "rc:consent",
            ClientMsg::ConsentPending { .. } => "rc:consent.pending",
            ClientMsg::SessionRequest { .. } => "rc:session.request",
            ClientMsg::SdpOffer { .. } => "rc:sdp.offer",
            ClientMsg::Ice { .. } => "rc:ice",
            ClientMsg::Terminate { .. } => "rc:terminate",
            ClientMsg::Ping { .. } => "rc:ping",
            ClientMsg::TunnelHello { .. } => "rc:tunnel.hello",
            ClientMsg::TunnelOpen { .. } => "rc:tunnel.open",
            ClientMsg::TcpForwardRequest { .. } => "rc:tunnel.tcp.request",
            ClientMsg::TcpForwardAccept { .. } => "rc:tunnel.tcp.accept",
            ClientMsg::TcpForwardReject { .. } => "rc:tunnel.tcp.reject",
            ClientMsg::TcpHalfClose { .. } => "rc:tunnel.tcp.half_close",
            ClientMsg::TcpClosed { .. } => "rc:tunnel.tcp.closed",
            ClientMsg::UdpForwardRequest { .. } => "rc:tunnel.udp.request",
            ClientMsg::UdpForwardAccept { .. } => "rc:tunnel.udp.accept",
            ClientMsg::UdpForwardReject { .. } => "rc:tunnel.udp.reject",
            ClientMsg::UdpClosed { .. } => "rc:tunnel.udp.closed",
            ClientMsg::TunnelTerminate { .. } => "rc:tunnel.terminate",
            ClientMsg::TunnelSdpOffer { .. } => "rc:tunnel.sdp.offer",
            ClientMsg::TunnelSdpAnswer { .. } => "rc:tunnel.sdp.answer",
            ClientMsg::TunnelIce { .. } => "rc:tunnel.ice",
            ClientMsg::TunnelQuicReady { .. } => "rc:tunnel.quic.ready",
            ClientMsg::TunnelQuicCandidate { .. } => "rc:tunnel.quic.candidate",
            ClientMsg::OverlayJoin { .. } => "rc:overlay.join",
            ClientMsg::OverlayEndpoints { .. } => "rc:overlay.endpoints",
            ClientMsg::OverlaySrflx { .. } => "rc:overlay.srflx",
            ClientMsg::OverlayNetcheck { .. } => "rc:overlay.netcheck",
            ClientMsg::OverlayLeave { .. } => "rc:overlay.leave",
            ClientMsg::OverlayRelayProbe { .. } => "rc:overlay.relay_probe",
            ClientMsg::OverlayRelayRequest { .. } => "rc:overlay.relay_request",
            ClientMsg::OverlayWarmRelayRequest { .. } => "rc:overlay.warm_relay_request",
        }
    }

    /// The module that owns this message. Exhaustive on purpose (FR-69 AC6):
    /// a new variant does not compile until it names an owner — the
    /// structural replacement for the `_ =>` catch-all hazard.
    pub fn namespace(&self) -> Owner {
        match self {
            ClientMsg::AgentHello { .. }
            | ClientMsg::AgentHeartbeat { .. }
            | ClientMsg::RpcResult { .. }
            | ClientMsg::RpcExecRequest { .. }
            | ClientMsg::ConfigStatus { .. }
            | ClientMsg::Consent { .. }
            | ClientMsg::ConsentPending { .. }
            | ClientMsg::Ping { .. } => Owner::Fleet,
            ClientMsg::SessionRequest { .. }
            | ClientMsg::SdpOffer { .. }
            | ClientMsg::SdpAnswer { .. }
            | ClientMsg::Ice { .. }
            | ClientMsg::Terminate { .. }
            | ClientMsg::SessionStats { .. } => Owner::Remote,
            ClientMsg::RelayProbeReport { .. }
            | ClientMsg::DerpTicketRequest { .. }
            | ClientMsg::SshRequest { .. }
            | ClientMsg::SshActivity { .. }
            | ClientMsg::KeyRotated { .. }
            | ClientMsg::TunnelHello { .. }
            | ClientMsg::TunnelOpen { .. }
            | ClientMsg::TcpForwardRequest { .. }
            | ClientMsg::TcpForwardAccept { .. }
            | ClientMsg::TcpForwardReject { .. }
            | ClientMsg::TcpHalfClose { .. }
            | ClientMsg::TcpClosed { .. }
            | ClientMsg::UdpForwardRequest { .. }
            | ClientMsg::UdpForwardAccept { .. }
            | ClientMsg::UdpForwardReject { .. }
            | ClientMsg::UdpClosed { .. }
            | ClientMsg::TunnelTerminate { .. }
            | ClientMsg::TunnelSdpOffer { .. }
            | ClientMsg::TunnelSdpAnswer { .. }
            | ClientMsg::TunnelIce { .. }
            | ClientMsg::TunnelQuicReady { .. }
            | ClientMsg::TunnelQuicCandidate { .. }
            | ClientMsg::OverlayJoin { .. }
            | ClientMsg::OverlayEndpoints { .. }
            | ClientMsg::OverlaySrflx { .. }
            | ClientMsg::OverlayNetcheck { .. }
            | ClientMsg::OverlayLeave { .. }
            | ClientMsg::OverlayRelayProbe { .. }
            | ClientMsg::OverlayRelayRequest { .. }
            | ClientMsg::OverlayWarmRelayRequest { .. } => Owner::Network,
        }
    }
}

/// Every client wire tag with its owner: the table the composition baseline
/// snapshots (`crates/tests/fixtures/composition.baseline.json`, section
/// `namespaces`), so a message that changes hands is a visible line in a
/// review. Kept next to [`ClientMsg::namespace`] and locked against both the
/// enum's renames and the match by the tests below.
pub const CLIENT_MSG_OWNERS: &[(&str, Owner)] = &[
    ("rc:agent.hello", Owner::Fleet),
    ("rc:agent.heartbeat", Owner::Fleet),
    ("rc:relay.probe_report", Owner::Network),
    ("rc:session.stats", Owner::Remote),
    ("rc:relay.derp_ticket_request", Owner::Network),
    ("rc:rpc.result", Owner::Fleet),
    ("rc:rpc.request", Owner::Fleet),
    ("rc:ssh.request", Owner::Network),
    ("rc:ssh.activity", Owner::Network),
    ("rc:agent.config_status", Owner::Fleet),
    ("rc:agent.key_rotated", Owner::Network),
    ("rc:sdp.answer", Owner::Remote),
    ("rc:consent", Owner::Fleet),
    ("rc:consent.pending", Owner::Fleet),
    ("rc:session.request", Owner::Remote),
    ("rc:sdp.offer", Owner::Remote),
    ("rc:ice", Owner::Remote),
    ("rc:terminate", Owner::Remote),
    ("rc:ping", Owner::Fleet),
    ("rc:tunnel.hello", Owner::Network),
    ("rc:tunnel.open", Owner::Network),
    ("rc:tunnel.tcp.request", Owner::Network),
    ("rc:tunnel.tcp.accept", Owner::Network),
    ("rc:tunnel.tcp.reject", Owner::Network),
    ("rc:tunnel.tcp.half_close", Owner::Network),
    ("rc:tunnel.tcp.closed", Owner::Network),
    ("rc:tunnel.udp.request", Owner::Network),
    ("rc:tunnel.udp.accept", Owner::Network),
    ("rc:tunnel.udp.reject", Owner::Network),
    ("rc:tunnel.udp.closed", Owner::Network),
    ("rc:tunnel.terminate", Owner::Network),
    ("rc:tunnel.sdp.offer", Owner::Network),
    ("rc:tunnel.sdp.answer", Owner::Network),
    ("rc:tunnel.ice", Owner::Network),
    ("rc:tunnel.quic.ready", Owner::Network),
    ("rc:tunnel.quic.candidate", Owner::Network),
    ("rc:overlay.join", Owner::Network),
    ("rc:overlay.endpoints", Owner::Network),
    ("rc:overlay.srflx", Owner::Network),
    ("rc:overlay.netcheck", Owner::Network),
    ("rc:overlay.leave", Owner::Network),
    ("rc:overlay.relay_probe", Owner::Network),
    ("rc:overlay.relay_request", Owner::Network),
    ("rc:overlay.warm_relay_request", Owner::Network),
];

#[cfg(test)]
mod namespace_tests {
    use super::*;

    /// The renames inside the `ClientMsg` enum, read from this file's own
    /// source — so the table below is checked against what serde actually
    /// emits, not against a second hand-written list.
    fn client_renames() -> Vec<String> {
        let src = include_str!("signaling.rs");
        let start = src
            .find("pub enum ClientMsg {")
            .expect("the enum is in this file");
        let body = &src[start..];
        let end = body.find("\n}\n").expect("the enum closes");
        let body = &body[..end];
        let needle = "rename = \"";
        let mut out = Vec::new();
        let mut rest = body;
        while let Some(i) = rest.find(needle) {
            rest = &rest[i + needle.len()..];
            let Some(j) = rest.find('"') else { break };
            out.push(rest[..j].to_string());
            rest = &rest[j + 1..];
        }
        out
    }

    fn owner_of(tag: &str) -> Option<Owner> {
        CLIENT_MSG_OWNERS
            .iter()
            .find(|(t, _)| *t == tag)
            .map(|(_, o)| *o)
    }

    /// The table names every client wire tag exactly once — no variant
    /// without an owner, no owner for a variant that is gone.
    #[test]
    fn the_owner_table_names_every_client_wire_tag_exactly_once() {
        let renames = client_renames();
        assert!(
            renames.len() >= 40,
            "the enum span was not read: {} renames",
            renames.len()
        );
        let table: Vec<&str> = CLIENT_MSG_OWNERS.iter().map(|(t, _)| *t).collect();
        let mut deduped = table.clone();
        deduped.sort_unstable();
        deduped.dedup();
        assert_eq!(deduped.len(), table.len(), "a wire tag is listed twice");
        let mut missing: Vec<&str> = renames
            .iter()
            .map(String::as_str)
            .filter(|r| !table.contains(r))
            .collect();
        let mut extra: Vec<&str> = table
            .iter()
            .copied()
            .filter(|t| !renames.iter().any(|r| r == t))
            .collect();
        missing.sort_unstable();
        extra.sort_unstable();
        assert!(
            missing.is_empty() && extra.is_empty(),
            "owner table drift — missing: {missing:?}, extra: {extra:?}"
        );
    }

    /// `wire_tag()` is the tag serde emits, and `namespace()` is the table's
    /// owner — checked on the variants cheap enough to build.
    #[test]
    fn wire_tag_and_namespace_agree_with_the_table() {
        let samples = [
            ClientMsg::Ping { id: 7 },
            ClientMsg::Terminate {
                session_id: ObjectId::new(),
                reason: EndReason::AdminTerminated,
            },
            ClientMsg::OverlayLeave {},
            ClientMsg::TunnelTerminate {
                session_id: ObjectId::new(),
                reason: CloseReason::ClientShutdown,
            },
        ];
        for m in &samples {
            let v = serde_json::to_value(m).expect("a client message serialises");
            assert_eq!(v["t"].as_str(), Some(m.wire_tag()), "{m:?}");
            let owner = owner_of(m.wire_tag()).expect("every variant is in the table");
            assert_eq!(m.namespace(), owner, "{m:?}");
        }
    }

    /// The three placements D7 calls out because the prefix would mislead.
    #[test]
    fn the_prefix_is_not_the_owner() {
        assert_eq!(owner_of("rc:consent"), Some(Owner::Fleet));
        assert_eq!(owner_of("rc:consent.pending"), Some(Owner::Fleet));
        assert_eq!(owner_of("rc:relay.probe_report"), Some(Owner::Network));
        assert_eq!(owner_of("rc:agent.key_rotated"), Some(Owner::Network));
    }

    /// The ids are what the module graph spells, and they serialise as such.
    #[test]
    fn owner_ids_are_stable_lowercase_module_ids() {
        for owner in [Owner::Fleet, Owner::Remote, Owner::Network] {
            let json = serde_json::to_string(&owner).unwrap();
            assert_eq!(json, format!("\"{}\"", owner.id()));
        }
    }
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
        /// Multi-user P3 — the EFFECTIVE permission grant, which may be
        /// narrower than requested (the single-INPUT-holder rule strips
        /// INPUT while another live session on the agent holds it). Additive:
        /// `None` from a pre-P3 server means "as requested" (the legacy
        /// contract); serde-default so old viewers ignore it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        permissions: Option<crate::permissions::Permissions>,
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
        /// `ROOMLERD_VP9_CHROMA` env-var default".
        #[serde(default, skip_serializing_if = "Option::is_none")]
        chroma_pref: Option<String>,
        /// FR-17 — forwarded verbatim from the controller's
        /// [`ClientMsg::SessionRequest::chunk_framing`]. `Some(true)` means
        /// the controller can parse the framed wire format; anything else
        /// keeps the legacy unframed one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        chunk_framing: Option<bool>,
        /// Opt-in system/desktop audio, forwarded verbatim from the
        /// controller's [`ClientMsg::SessionRequest::audio_enabled`]. When
        /// `true` the agent adds a WebRTC Opus audio track (if built with
        /// the `audio` feature). `#[serde(default)]` → older servers /
        /// controllers that omit it get no audio track.
        #[serde(default)]
        audio_enabled: bool,
        /// Phase 2 — server-authoritative consent directive. `None` (an older
        /// server that predates consent modes) → the agent falls back to its
        /// local `auto_grant_session` config; `Some(mode)` → the agent OBEYS:
        /// `Auto` grants immediately with no prompt, everything else runs the
        /// on-host prompt path. Resolved server-side from the device's
        /// `AccessPolicy.consent_mode` (self-control → `Auto`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        consent_mode: Option<ConsentMode>,
        /// FR-27 — how long the ON-HOST prompt should stand, which is not
        /// always `consent_timeout_secs`.
        ///
        /// The two coincide for `prompt`, and diverge for `prompt_then_email`:
        /// the session waits the full async window for the owner's emailed
        /// link, but the modal on the host's screen has no business standing
        /// there for five minutes. Sent by the server rather than derived by
        /// the agent so there is ONE authority for the split; `None` (older
        /// server) means "use `consent_timeout_secs`", i.e. exactly the
        /// pre-FR-27 behaviour.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        host_prompt_timeout_secs: Option<u32>,
        /// P6 — the device's input arbitration mode directive from
        /// `AccessPolicy.input_mode`. `None` (older server / unset policy) →
        /// the agent's arbiter default (free). Only the FIRST session's hint
        /// seeds the mode; in-session toggles win afterwards. Additive —
        /// serde-default so old agents/servers interop.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        input_mode: Option<crate::models::InputMode>,
        /// Multi-org — the DISPLAY NAME of the organization this request
        /// comes from, so the consent prompt can say who is asking.
        ///
        /// A device enrolled in N orgs runs N signalling loops into the same
        /// host. Without this the operator sees "Alice wants to control this
        /// machine" and cannot tell whether Alice is asking as a colleague or
        /// as a contractor from another company — which is precisely the
        /// decision consent exists to make. The agent knows only its
        /// `tenant_id` (an opaque hex), so the NAME has to come from here.
        ///
        /// `None` on an older server → the prompt falls back to the org's
        /// config label, and to nothing at all for the primary enrollment
        /// (the pre-multi-org behaviour).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tenant_name: Option<String>,
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
    /// FR-34 — relayed from the agent's [`ClientMsg::ConsentPending`]: the
    /// host is locked while a consent prompt is pending, so the operator has
    /// to unlock the machine before they can see and approve it. Advisory; the
    /// viewer uses it to turn a bare "awaiting consent" into an instruction.
    #[serde(rename = "rc:consent.pending")]
    ConsentPending {
        #[serde(with = "oid_hex")]
        session_id: ObjectId,
        host_locked: bool,
    },

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
        /// Set only when this error rejects a `TunnelOpen` — carries that
        /// open's `open_nonce` so a multiplexing daemon can fail the exact
        /// pending flow instead of guessing. `None` (omitted) for every
        /// non-open error and whenever the originator sent no nonce. A
        /// nonce-less `Error` arriving mid-open therefore reads as "open
        /// rejected / server too old" and fails that flow fast.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        open_nonce: Option<String>,
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

    /// S1a — operator-triggered forced self-update ("Update now" in the
    /// admin UI, single or bulk). The agent runs one update cycle
    /// immediately — the same download/verify/install path as its
    /// periodic updater, still honouring the 5-min install-storm
    /// cooldown. Back-compat mirrors `Goodbye`: pre-S1a agents fail to
    /// decode the unknown tag in their `Err(e) => debug!` decoder branch
    /// and ignore it — no WS disruption.
    #[serde(rename = "rc:agent.update")]
    UpdateNow {
        /// Optional release tag to install (e.g. `agent-v0.3.0-rc.260`);
        /// `None` = latest. Pinning bypasses the is-newer check (the
        /// admin explicitly chose a version).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pin: Option<String>,
    },

    /// The device config an operator has asked for, for the agent to
    /// reconcile against its own (`docs/remote-config.md`).
    ///
    /// A REQUEST, not an instruction. Three independent things can make the
    /// agent decline it, and all three are the point rather than edge cases:
    /// the device may not have opted in (`remote_config_enabled`, default
    /// off), the frame may have arrived on a SECONDARY org's WS (these keys
    /// are machine-wide, so only the primary enrollment may drive them —
    /// the same rule `UpdateNow` applies to the machine-wide updater), and a
    /// device is always free to be running an older build.
    ///
    /// ⚠️ Unlike `Goodbye`/`UpdateNow`, the server must NOT send this
    /// blind. Those are fire-and-forget, so a pre-feature agent dropping the
    /// unknown tag in its `Err(e) => debug!` branch costs nothing. Here the
    /// dashboard is showing an operator that a change is pending, so a
    /// silently-evaporated frame becomes a lie on a screen. Gate on
    /// [`RpcCap::Config`] and surface "device too old" instead.
    ///
    /// [`RpcCap::Config`]: crate::models::RpcCap::Config
    #[serde(rename = "rc:agent.config")]
    ConfigPush {
        /// The `desired_config.revision` this frame carries. The agent echoes
        /// it back once applied, which is what lets the UI tell "refused" from
        /// "never heard" — two states that otherwise look identical.
        revision: u64,
        /// Only the keys under management; absent keys mean "leave alone".
        desired: crate::models::DesiredConfig,
    },

    /// FR-40 — order the device to retire its overlay (WireGuard) key: mint
    /// a fresh one LOCALLY, persist it, report back with
    /// [`ClientMsg::KeyRotated`], then reconnect and re-join under the new
    /// key (`docs/fr/FR-40-overlay-key-rotation.md`).
    ///
    /// An ORDER, never a delivery: this frame carries no key material and a
    /// test locks that it never grows any. The server never sees a private
    /// key at any step — which is the property that makes the overlay's
    /// end-to-end encryption mean something.
    ///
    /// Per org, honoured on whichever org's WS it arrives on: a device's
    /// overlay key is per enrollment (`AgentConfig::for_org` scopes it), so
    /// org B rotating its own key on a shared host touches nothing of org
    /// A's. This is the OPPOSITE of [`Self::UpdateNow`] / [`Self::ConfigPush`],
    /// which drive machine-wide state and are therefore primary-only.
    ///
    /// ⚠️ Gate on [`RpcCap::KeyRotate`] — never send it blind (see that
    /// verb's doc). Disruptive by design: the device ends every session it
    /// carries on that org and rebuilds its overlay runtime.
    ///
    /// [`RpcCap::KeyRotate`]: crate::models::RpcCap::KeyRotate
    #[serde(rename = "rc:agent.key_rotate")]
    KeyRotate {
        /// Echoed in the report so an answer can be matched to THIS order.
        request_id: String,
    },

    /// Multi-org — join an ADDITIONAL org from the admin UI ("Add to another
    /// organization"), so a device that can't be reached for a hands-on
    /// `roomler enroll` still gets a second enrollment.
    ///
    /// The agent exchanges the token for its own agent JWT in the target
    /// tenant, APPENDS an `[[orgs]]` entry (with a freshly minted per-org WG
    /// key — never a copy) and brings that org's supervised WS loop up
    /// in-process, no restart.
    ///
    /// **There is deliberately no `server_url` field.** The agent always
    /// enrolls against the control plane it is ALREADY talking to, so a
    /// forged/relayed frame can never point a device at a foreign server —
    /// and same-control-plane is a hard requirement for shared-TUN meshing
    /// anyway (`docs/multi-org.md` §4.4).
    ///
    /// Honoured ONLY on the PRIMARY enrollment's socket (mirroring
    /// [`Self::UpdateNow`]): a secondary org's admin must not be able to
    /// enroll a device it merely borrows into further orgs.
    ///
    /// Back-compat mirrors `Goodbye`/`UpdateNow`: an agent that predates the
    /// variant fails to decode the unknown tag in its `Err(e) => debug!`
    /// branch and ignores it — no WS disruption. The server still gates on
    /// `AgentCaps.multi_org` so the UI can be honest about which devices
    /// support it.
    #[serde(rename = "rc:agent.join_org")]
    JoinOrg {
        /// Single-use enrollment token for the TARGET tenant (10 min).
        enrollment_token: String,
        /// Label for the new `[[orgs]]` entry. `None` ⇒ the agent derives
        /// one (server host), same as a CLI `enroll` without `--label`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        /// Overlay participation for the new org: `off` (default) |
        /// `netstack` | `tun`. `tun` additionally needs the daemon's
        /// `overlay_multi_org` opt-in to take effect.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        overlay_mode: Option<String>,
    },

    // ─── Fleet RPC (rc:rpc.*) ────────────────────────────────────────
    /// Run one bounded shell command on this device and answer with
    /// [`ClientMsg::RpcResult`].
    ///
    /// The server has already cleared gates 1–3 (org kill-switch, caller
    /// permission, device [`crate::models::ExecPolicy`]) before this is sent;
    /// the agent enforces gate 4 — its own `exec_enabled` config key — plus
    /// consent and the execution bounds below. A gate-4 refusal comes back as
    /// an `RpcResult` carrying `error`, never as silence.
    ///
    /// Unlike `Goodbye`/`UpdateNow`, this variant must NOT rely on the
    /// unknown-tag debug branch: a caller is blocked on the answer, so
    /// silence reads as a hang. The server gates on `AgentCaps.rpc`
    /// containing `exec` and fails the request with `412` when it is absent.
    #[serde(rename = "rc:rpc.exec")]
    RpcExec {
        /// Correlates with [`ClientMsg::RpcResult`]. Server-minted.
        request_id: String,
        /// `pwsh` | `powershell` | `cmd` on Windows; `bash` | `sh` elsewhere.
        shell: String,
        command: String,
        /// Server-clamped wall-clock budget. The agent kills the whole
        /// process tree when it expires.
        timeout_ms: u64,
        /// Server-clamped output ceiling across both streams combined.
        max_output_bytes: u64,
        /// Working directory; `None` ⇒ the daemon's own.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        /// Display name of the acting principal — shown in the consent prompt
        /// and written to the agent's local log, so the person at the device
        /// can see who asked.
        caller: String,
        /// Server-resolved consent directive from the device's
        /// [`crate::models::ExecPolicy`], mirroring how [`Self::Request`]
        /// carries `consent_mode` for a session. `Auto` runs immediately
        /// (unattended servers); anything else prompts the person at the
        /// device and denies on timeout. Absent ⇒ the agent prompts, because
        /// the fail-safe direction for a gate that grants root is "ask".
        #[serde(default, skip_serializing_if = "Option::is_none")]
        consent_mode: Option<crate::models::ConsentMode>,
    },

    /// Kill an in-flight [`Self::RpcExec`] and its whole process tree. The
    /// agent still answers with an [`ClientMsg::RpcResult`] (carrying
    /// `error`), so a cancelled caller is never left waiting.
    #[serde(rename = "rc:rpc.cancel")]
    RpcCancel { request_id: String },

    /// Authorize ONE inbound roomler-SSH session on this device.
    ///
    /// Pushed to the **target** after the server has cleared gates 1-3 (org
    /// kill-switch, the caller's `SSH_DEVICE` permission, the device's
    /// `SshPolicy`). The agent holds it briefly and consumes it when a
    /// connection authenticates with the named key.
    ///
    /// This is why roomler SSH needs no key distribution and no shared secret
    /// with the server: the agent does not *verify* the grant cryptographically,
    /// it receives it over the control WS it is already authenticated on —
    /// the same trust path `rc:request` uses to open a remote-control session.
    #[serde(rename = "rc:ssh.grant")]
    SshGrant {
        /// Server-minted, single-use.
        grant_id: String,
        /// OpenSSH public key the caller will authenticate with. Accepted for
        /// exactly one session, until `expires_at_ms`.
        public_key: String,
        /// Display name of the acting principal — shown in the consent prompt,
        /// written to the agent's local log and carried into the audit record,
        /// so the person at the device can see who connected.
        caller: String,
        /// Which local account the session runs as, server-resolved from the
        /// device's policy: `daemon` | `console_user` | `named`.
        account_mode: String,
        /// The account for `named`. Absent otherwise.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account: Option<String>,
        /// Unix ms after which the grant may no longer be redeemed. Short —
        /// see [`crate::models::ssh_limits::GRANT_TTL_SECS`].
        expires_at_ms: u64,
        /// Server-clamped lifetime of the session once redeemed.
        session_secs: u64,
        /// Server-resolved consent directive, mirroring [`Self::RpcExec`].
        /// Absent ⇒ the agent prompts, the fail-safe direction.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        consent_mode: Option<crate::models::ConsentMode>,
    },

    /// Answer to a device-originated [`ClientMsg::SshRequest`] — the
    /// `roomler ssh` CLI leg. Carries where to connect, or why not.
    #[serde(rename = "rc:ssh.response")]
    SshResponse {
        request_id: String,
        /// The target's overlay address. Absent on refusal.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        address: Option<String>,
        /// The target's intercepted SSH port. Absent on refusal.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        port: Option<u16>,
        /// Echo of the grant the target was given, for correlation in logs.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        grant_id: Option<String>,
        /// P6a — the target's SSH host public key, so the dialling device can
        /// verify what it reached instead of trusting it on first use.
        ///
        /// This leg needs it MORE than the HTTP one, not less: `roomler ssh`
        /// originates here, and it is the caller that has no other channel to
        /// learn the key. Absent ⇒ the device reported none, which means it
        /// cannot prove itself — never "any key is fine".
        #[serde(default, skip_serializing_if = "Option::is_none")]
        host_pubkey: Option<String>,
        /// Unix ms the grant stops being redeemable — the caller should dial
        /// before this and give up after.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expires_at_ms: Option<u64>,
        /// Set when the server refused. A caller is blocked on this frame, so
        /// a refusal is always answered rather than dropped.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },

    /// Answer to a device-originated [`ClientMsg::RpcExecRequest`] — the
    /// `roomler exec` CLI leg. Carries the same payload the HTTP caller would
    /// have received, plus `error` for a gate refusal the server itself made.
    #[serde(rename = "rc:rpc.response")]
    RpcExecResponse {
        request_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        stdout: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        stderr: String,
        #[serde(default)]
        truncated: bool,
        #[serde(default)]
        duration_ms: u64,
        /// Set when the command never ran (gate refusal, offline, timeout).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
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
        /// Short-lived token the client presents on the QUIC connection
        /// so the agent's quinn endpoint can authorize the dialer (the
        /// server is no longer in the byte path for QUIC). `None` for
        /// `webrtc-dc-v1` sessions. Minted by the server, bound to the
        /// `session_id` + agent. Wired in Phase 1c/1d.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        quic_auth_token: Option<String>,
        /// Correlation id echoed from the originating `TunnelOpen`, so a
        /// daemon multiplexing N opens over one WS can match THIS
        /// `TunnelOpened` to the pending open that caused it. `None`
        /// (omitted) for the single-open CLI and for pre-P3b-2 servers.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        open_nonce: Option<String>,
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

    /// Server → agent: a tunnel-client wants to open this UDP forward;
    /// the server has already passed the cross-tenant gate + the tenant
    /// policy (evaluated with `proto = udp`). Mirrors
    /// [`TcpForwardForward`]. Agent binds a UDP socket + replies with
    /// `UdpForwardAccept` / `UdpForwardReject`.
    #[serde(rename = "rc:tunnel.udp.forward")]
    UdpForwardForward {
        #[serde(with = "oid_hex")]
        session_id: ObjectId,
        flow_id: u32,
        dst_host: String,
        dst_port: u16,
        #[serde(with = "oid_hex")]
        owner_user_id: ObjectId,
    },

    /// Server → client: relays the agent's `UdpForwardAccept`.
    #[serde(rename = "rc:tunnel.udp.accept")]
    UdpForwardAccept {
        #[serde(with = "oid_hex")]
        session_id: ObjectId,
        flow_id: u32,
        dc_index: u8,
    },

    /// Server → client: relays the agent's reject OR synthesises one
    /// from the server-side ACL gate.
    #[serde(rename = "rc:tunnel.udp.reject")]
    UdpForwardReject {
        #[serde(with = "oid_hex")]
        session_id: ObjectId,
        flow_id: u32,
        kind: RejectKind,
        reason: String,
    },

    /// Server → peer: relays a UDP flow close.
    #[serde(rename = "rc:tunnel.udp.closed")]
    UdpClosed {
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

    /// Server → agent: prepare a QUIC server endpoint for this session
    /// and authorize the client that presents `quic_auth_token`. The
    /// agent's trigger to mint its ephemeral cert + bind a quinn
    /// endpoint, then reply with `ClientMsg::TunnelQuicReady`. The QUIC
    /// analogue of the server relaying an SDP offer to the agent.
    #[serde(rename = "rc:tunnel.quic.setup")]
    TunnelQuicSetup {
        #[serde(with = "oid_hex")]
        session_id: ObjectId,
        quic_auth_token: String,
        /// Short-lived coturn ICE/TURN credentials the AGENT uses to
        /// allocate its own relay for QUIC-over-TURN (Tier 2/3). Empty
        /// for direct-only sessions and from pre-3c servers (hence
        /// `#[serde(default)]` — an older agent simply ignores the field
        /// and a newer agent treats its absence as "no relay creds, go
        /// direct"). Phase 3c.
        #[serde(default)]
        ice_servers: Vec<IceServer>,
        /// R4: the negotiated transport flavor when it is NOT the default
        /// QUIC-over-TURN path — today only `quic-derp-v1`. A pre-R4 agent
        /// never receives it (the server version-gates the flavor), and a
        /// pre-R4 SERVER omits it, which a new agent reads as the classic
        /// TURN/direct path. Absent ⇒ omitted on the wire.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        transport: Option<String>,
        /// R4 (`quic-derp-v1`): the CLIENT's overlay/DERP pubkey (64 hex),
        /// copied verbatim from `TunnelOpen.derp_pubkey` — the peer the
        /// agent's DERP-backed QUIC endpoint serves.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_derp_pubkey: Option<String>,
    },

    /// Server → tunnel-client: relays the agent's `TunnelQuicReady`
    /// (cert fingerprint to pin + dialable addrs). Mirror of
    /// `ClientMsg::TunnelQuicReady`.
    #[serde(rename = "rc:tunnel.quic.ready")]
    TunnelQuicReady {
        #[serde(with = "oid_hex")]
        session_id: ObjectId,
        cert_fingerprint: String,
        addrs: Vec<String>,
        /// R4 — see `ClientMsg::TunnelQuicReady::derp_pubkey` (relayed
        /// verbatim).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        derp_pubkey: Option<String>,
    },

    /// Server → agent: relays the tunnel-client's `TunnelQuicCandidate`
    /// (the client's relay address the agent must permit before the
    /// handshake). Mirror of `ClientMsg::TunnelQuicCandidate`. Phase 3c.
    #[serde(rename = "rc:tunnel.quic.candidate")]
    TunnelQuicCandidate {
        #[serde(with = "oid_hex")]
        session_id: ObjectId,
        addrs: Vec<String>,
    },

    // ─── server → overlay node (rc:overlay.*) ────────────────────────
    /// Full network map sent to a node on join. Carries the node's own
    /// `self_ip`, the network parameters, and every peer it may reach.
    /// `epoch` monotonically increases per network so a node can detect
    /// a missed delta and (in a later phase) request a resync.
    #[serde(rename = "rc:overlay.netmap")]
    OverlayNetmap {
        self_ip: String,
        network: OverlayNetworkInfo,
        peers: Vec<NetmapPeer>,
        /// ⚠️ `#[serde(default)]` because this field is REQUIRED to decode the
        /// frame and is read by NOBODY. The agent destructures
        /// `{ self_ip, network, peers, .. }` and discards it; the resync it was
        /// added for was never built.
        ///
        /// Without the default, a netmap that omitted `epoch` — a rolled-back
        /// server, or any future sender that forgets it — fails the whole
        /// `ServerMsg` parse and the node gets NO address and NO peers. A field
        /// no consumer reads must not be able to destroy the frame carrying it.
        #[serde(default)]
        epoch: u64,
    },

    /// Incremental netmap update: peers to add/update and node_ids to
    /// remove. Pushed on join/leave/endpoint-change (and, later,
    /// ACL-change/rekey).
    #[serde(rename = "rc:overlay.netmap_delta")]
    OverlayNetmapDelta {
        /// Defaulted for the same reason as [`ServerMsg::OverlayNetmap`]'s: the
        /// agent reads `{ upserts, removes, .. }` and never this. A delta lost
        /// to a missing `epoch` silently strands a peer add or, worse, a peer
        /// REMOVAL — leaving a node routing to an address that has been
        /// recycled to someone else.
        #[serde(default)]
        epoch: u64,
        #[serde(default)]
        upserts: Vec<NetmapPeer>,
        #[serde(default, with = "vec_oid_hex")]
        removes: Vec<ObjectId>,
    },

    /// On-demand coturn credentials for a relay leg to a specific peer.
    /// `pair_key = sorted(node_a_hex, node_b_hex)` so both ends derive
    /// identical short-lived creds (same-worker hairpin), exactly like
    /// the QUIC tunnel's per-session creds.
    #[serde(rename = "rc:overlay.relay_grant")]
    OverlayRelayGrant {
        ice_servers: Vec<IceServer>,
        #[serde(with = "oid_hex")]
        peer_node_id: ObjectId,
        pair_key: String,
    },

    /// C4 stage 1 — reply to `rc:overlay.warm_relay_request`: ephemeral
    /// coturn creds keyed by the requesting node itself (no pair). The
    /// agent allocates over TURN/UDP and keeps the allocation alive; cred
    /// expiry is derivable client-side from the ephemeral username's
    /// timestamp prefix. Only ever sent in reply, never pushed — that is
    /// what exempts it from the hello-capability-flag rule.
    #[serde(rename = "rc:overlay.warm_relay_grant")]
    OverlayWarmRelayGrant { ice_servers: Vec<IceServer> },

    /// P7 (corp-DERP fallback) — the server observed sustained TURN-relay
    /// churn for this pair (repeated grant→re-request cycles: a corp
    /// middlebox RST-ing the TURNS/TCP control socket kills every
    /// allocation) and escalates it to the DERP carrier. Pushed to **BOTH**
    /// ends of the pair — this must NOT ride the relay grant, because the
    /// single-relay DIALER never sends a `relay_request` and so never sees a
    /// grant. Each end pins the pair to `RelayStrategy::Derp` for `ttl_ms`;
    /// the pin governs (re)establishment only — an already-healthy carrier
    /// is left alone, and LAN/direct upgrades keep working (DERP is a
    /// fallback tier, not a destination). Only sent when BOTH ends
    /// advertised `supports_forced_derp` + `supports_derp`, so a mixed-
    /// version pair can never split tiers.
    #[serde(rename = "rc:overlay.force_derp")]
    OverlayForceDerp {
        #[serde(with = "oid_hex")]
        peer_node_id: ObjectId,
        /// How long the pin lasts, in ms (server-side TTL mirrored so both
        /// ends expire together).
        ttl_ms: u64,
        /// Multi-region DERP: the REGIONAL relay both ends must dial for this
        /// pair (server-computed from the pair's relay homes, pushed
        /// identically to both — symmetric by construction). Absent/`None` =
        /// the central `/derp`. `#[serde(default)]` for pre-region agents.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        derp_url: Option<String>,
    },

    /// FR-19 — a minted org-relay session, pushed to each MEMBER of the pair.
    /// Only ever sent to a node that advertised `supports_org_relay` on join.
    /// The `bind_secret` is per-(session, member): possession of it is what
    /// "this is the node the session was minted for" means at the relay.
    #[serde(rename = "rc:overlay.relay_session")]
    OverlayRelaySession {
        /// 24-bit session id, unique per relay node.
        vni: u32,
        /// Re-mint counter for this (pair, relay); covered by the bind MAC.
        generation: u64,
        #[serde(with = "oid_hex")]
        peer_node_id: ObjectId,
        #[serde(with = "oid_hex")]
        relay_node_id: ObjectId,
        /// The relay's reachable `ip:port`s, measured + static. Try in order.
        relay_endpoints: Vec<String>,
        /// This member's bind secret, base64 (32 bytes).
        bind_secret: String,
        /// Seconds the relay allows to complete the bind; the agent re-clamps
        /// against its own clock (server timestamps only ever shorten).
        bind_secs: u32,
        /// Absolute session lifetime in seconds, independent of traffic.
        max_lifetime_secs: u32,
    },

    /// FR-19 — the same session, pushed to the RELAY node with BOTH members'
    /// secrets so it can verify either party's bind. Only ever sent to a node
    /// whose hello advertised the `relay-server` verb.
    #[serde(rename = "rc:overlay.relay_serve")]
    OverlayRelayServe {
        vni: u32,
        generation: u64,
        /// The two members. Exactly two — a relay session is a pair.
        members: Vec<RelayMemberWire>,
        bind_secs: u32,
        idle_secs: u32,
        max_lifetime_secs: u32,
    },

    /// FR-19 — revoke a session immediately, on relay AND members. Pushed on
    /// org mode-off, ACL revoke, policy revoke and device removal. Revocation
    /// is a push rather than an expiry because a session's idle deadline is
    /// refreshed by traffic and never fires under a WireGuard keepalive.
    #[serde(rename = "rc:overlay.relay_revoke")]
    OverlayRelayRevoke { vni: u32 },

    /// FR-47 — the join could not be completed, and this says why.
    ///
    /// Before this frame a join that failed IPAM was a server-side `warn!` and
    /// nothing else: the node sat waiting for a netmap that would never come,
    /// so a device that could not be given an address looked exactly like one
    /// that was merely offline — to the operator as much as to the daemon.
    ///
    /// Hello-gated on `supports_join_refusal`, following the
    /// [`ServerMsg::RelayRegions`] convention: a fielded agent's `ServerMsg`
    /// deserializer errors on an unknown variant, and while that error is
    /// absorbed harmlessly (`pre_rc53_server_msg_rejects_goodbye_so_agent_err_arm_fires`
    /// locks it), a frame nobody can read is not worth sending. Pre-FR-47
    /// nodes keep exactly today's behaviour, and the server-side ERROR log
    /// remains their signal.
    #[serde(rename = "rc:overlay.join_refused")]
    OverlayJoinRefused {
        reason: OverlayJoinRefusal,
        /// Operator-facing detail. Safe to log; never contains another
        /// tenant's identifiers.
        detail: String,
    },

    /// Multi-region relay PoPs: the probe-target list the server pushes to a
    /// node that advertised `supports_relay_regions` (hello-gated — the agent's
    /// `ServerMsg` deserializer errors on unknown variants, so this is NEVER
    /// sent to a node that didn't advertise the capability). The node times a
    /// STUN binding against each region's `stun` target and reports back via
    /// [`ClientMsg::RelayProbeReport`]; the resulting `relay_home` drives every
    /// per-session/per-pair TURN region pick. Re-pushed with a new `rev` when
    /// the region set changes; same `rev` ⇒ the node may skip re-probing early.
    #[serde(rename = "rc:relay.regions")]
    RelayRegions {
        regions: Vec<RelayRegionInfo>,
        /// Stable hash of the region list — change detection for re-probes.
        rev: u64,
    },

    /// Multi-region DERP: the admission ticket for the regional relays, in
    /// reply to [`ClientMsg::DerpTicketRequest`] (never unsolicited — reply-
    /// only makes it capability-safe for old agents). `exp` is unix seconds;
    /// the agent re-requests at ~90 % of the TTL.
    #[serde(rename = "rc:relay.derp_ticket")]
    DerpTicket { ticket: String, exp: u64 },
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct IceServer {
    pub urls: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
}

/// Phase B (overlay v3) — the MEASURED capability vector on the wire
/// (netcheck). Every field serde-defaulted so the shape can grow; `Option`
/// fields distinguish "measured false" from "not measured" — absence of
/// measurement is NEVER evidence of absence of capability, and consumers
/// fall back to presence rules on `None`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default, Copy)]
pub struct CapVectorWire {
    /// The srflx gather found a public mapping.
    #[serde(default)]
    pub stun_udp: bool,
    /// Raw UDP reaches coturn's relay band, measured over the exact
    /// single-relay dialer path. `Some(false)` = the CORPLAP-class egress drop.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay_band_udp: Option<bool>,
    /// The central `/derp` WS is up + registered (the floor's health).
    #[serde(default)]
    pub derp_ws_ok: bool,
}

/// One member of an org-relay session as pushed to the relay in
/// [`ServerMsg::OverlayRelayServe`].
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct RelayMemberWire {
    /// base64 WireGuard public key — the member's identity as the mint names it.
    pub wg_public_key: String,
    /// base64 32-byte bind secret for THIS member.
    pub bind_secret: String,
}

/// One relay region as pushed in [`ServerMsg::RelayRegions`] — the probe
/// target, not the credential set (creds keep flowing through the normal
/// grant paths; this only lets the node measure which PoP is nearest).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct RelayRegionInfo {
    /// Region id (`relay_home` value), e.g. `"us-east"`.
    pub id: String,
    /// STUN probe target `host:port` (the region's coturn answers STUN
    /// Binding on its TURN UDP port).
    pub stun: String,
    /// The region's DERP endpoint, when it runs one (regional-DERP dialing).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub derp_url: Option<String>,
    /// Phase E (overlay v3) — the region's coturn relay allocation band
    /// (`min..=max` UDP ports). Lets the netcheck relay-band probe target
    /// the region's REAL band; absent from a pre-E server ⇒ agents keep
    /// the allocation-derived behavior.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay_band: Option<(u16, u16)>,
}

/// Agent host/process telemetry riding [`ClientMsg::AgentHeartbeat`]
/// (stats PR-5). Persisted into the `stats_machine` minute buckets; the
/// carrier tallies come from the overlay runtime's live view. Absence of
/// the whole block means "not measured" (pre-v2 agent).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct AgentSysStats {
    /// Agent process resident set, MiB.
    pub rss_mb: u32,
    /// Agent process CPU share, percent of one core.
    pub cpu_pct: f32,
    /// Host-total cumulative network counters (bytes since boot, summed
    /// over interfaces) — the server derives rates from successive
    /// samples; a reboot reads as a counter reset.
    pub net_rx_bytes: u64,
    pub net_tx_bytes: u64,
    /// Live overlay carriers by kind at sample time.
    pub direct: u32,
    pub relay: u32,
    pub derp: u32,
    /// Median prober RTT across live overlay peers, ms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer_rtt_ms: Option<u32>,
    /// Cumulative overlay IP-data volume, summed across this node's peer
    /// carriers (wave 3). Host-total `net_*_bytes` above counts ALL traffic
    /// on every interface; these two isolate the mesh's own share, which is
    /// what "how much did the overlay move" actually means.
    ///
    /// Cumulative-since-carrier-install, NOT since boot: a carrier rebuild
    /// resets its counters, so the sum can step DOWN. The server stores it
    /// with the same min/max-per-bucket treatment as `net_*_bytes` and
    /// differences read-side, which under-reports across a rebuild rather
    /// than going negative. Absent on pre-wave-3 agents.
    #[serde(default)]
    pub overlay_rx_bytes: u64,
    #[serde(default)]
    pub overlay_tx_bytes: u64,
    /// Cumulative tunnel-forward volume across this node's live flows
    /// (wave 3), from the endpoint's own counters.
    ///
    /// The server cannot measure this itself: tunnel payload rides the
    /// peer-to-peer data channel, which is why `tunnel_audit.bytes_in`/
    /// `bytes_out` have held a literal 0 on every row ever written. These
    /// two are DEVICE-attributed — a daemon-supervised forward belongs to
    /// the host, not to a person. A dedicated `roomler` client's
    /// flows are user-owned and still report nothing; that needs the same
    /// counters wired into the CLI's own heartbeat.
    ///
    /// `in` = the local app received, `out` = the local app sent.
    #[serde(default)]
    pub tunnel_rx_bytes: u64,
    #[serde(default)]
    pub tunnel_tx_bytes: u64,
    /// Per-peer mesh edges as this node currently reaches them (wave 2).
    /// The aggregate counters above answer "how many carriers of each
    /// kind"; this answers "which peer, over what, how fast" — the graph
    /// the org dashboard draws. Absent on pre-wave-2 agents.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<PeerLink>,
}

/// One overlay edge from the reporting node's point of view.
///
/// BOTH ends report the same pair, so the read side dedupes on the
/// sorted node pair and merges — the two ends can legitimately disagree
/// (one has a direct carrier installed while the other still relays),
/// and the pessimistic merge keeps the graph honest.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct PeerLink {
    /// The peer's overlay node id (hex).
    pub node: String,
    /// How this node reaches it: `direct` | `relay` | `derp` | `tunnel` |
    /// `blocked` | `offline`.
    pub carrier: String,
    /// Prober round-trip in ms, when measured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rtt_ms: Option<u32>,
    /// The health sweep's silently-one-way verdict — an edge that looks
    /// installed but carries nothing.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stalled: bool,
    /// IP-data bytes carried over this edge since its carrier was installed
    /// (wave 3). Handshakes and keepalives touch neither, so `tx > 0` with
    /// `rx == 0` is exactly the one-way signal `stalled` reports — the two
    /// corroborate each other. Zero on pre-wave-3 agents.
    #[serde(default)]
    pub tx: u64,
    #[serde(default)]
    pub rx: u64,
    /// Relay flavour + transport as one qualified suffix — `turn/udp`,
    /// `turn/tcp`, `derp/tcp` — the same detail the CLI's CONN column
    /// renders after `relay:` (wave 4). A bare carrier can't distinguish a
    /// ~50 ms coturn/UDP hop from a ~175 ms DERP/TCP one, which is exactly
    /// what the mesh graph exists to show. `None` for non-relay edges and
    /// for agents older than the field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay: Option<String>,
}

/// One region's probe outcome in [`ClientMsg::RelayProbeReport`].
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct RelayRegionRtt {
    pub region: String,
    /// Median STUN round-trip in ms; `None` = every sample timed out
    /// (UDP-blocked path or dead PoP).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rtt_ms: Option<u32>,
}

/// Loopback-TURN corp-relay descriptor (loopback-TURN Phase 2). Minted + served
/// by the controller host's LOCAL enrolled agent on its loopback probe endpoint
/// (`http://127.0.0.1:47989/rc-local-turn`); the browser forwards it verbatim in
/// [`ClientMsg::SessionRequest::local_relay`] so the Hub can append
/// `turn:{overlay_ip}:{turn_port}` to the REMOTE agent's ICE servers. The media
/// then relays through the local agent's overlay (WFP-permitted, corp-traversal
/// proven) instead of the capped far coturn — the whole point of the feature.
///
/// One type, three hops: the agent SERIALIZES it (loopback response), the
/// browser parses + re-emits it (`LocalRelayDescriptor` in `useRemoteControl.ts`,
/// same snake_case field names so the JSON round-trips unchanged), and the Hub
/// DESERIALIZES it here. WebRTC media is DTLS-E2E, so a relay only ever moves
/// ciphertext — the server is a dumb pass-through of the descriptor.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct LocalRelayDescriptor {
    /// UDP port the local agent's TURN server listens on (loopback + overlay).
    pub turn_port: u16,
    /// The local agent's assigned overlay IP (100.64.x.y) — the relay-candidate
    /// address the remote agent routes to over the roomler adapter. The Hub
    /// validates this is inside the overlay range before trusting it.
    pub overlay_ip: String,
    /// coturn-REST username (`{expiry}:{user_id}`) minted by the local agent.
    pub username: String,
    /// coturn-REST credential (hex HMAC-SHA1 over `username`) matching it.
    pub credential: String,
}

// ────────────────────────────────────────────────────────────────────────────
// Overlay network supporting types (rc:overlay.*)
// ────────────────────────────────────────────────────────────────────────────

/// U2 — the server's per-edge relay-tier verdict (the recipient's view of
/// THIS peer), mirroring the client's own `RelayStrategy` so the client can
/// use it verbatim in place of its local derivation. Serialised as a
/// lowercase-kebab string tag; an unknown/absent value ⇒ the client falls
/// back to its local computation, so adding a variant later is
/// forward-compatible.
///
/// ⚠️ That last sentence is only true because [`NetmapPeer::relay_strategy`]
/// decodes through [`relay_strategy_lenient`]. The derive alone does **not**
/// deliver it — see that function for what happens without it. Any *new*
/// field or message carrying this type must use the same shim.
///
/// Anchor/dialer symmetry is the whole reason this is server-computed: the
/// server holds BOTH ends' pubkeys, so it stamps exactly one end
/// `SingleRelayAnchor` and the other `SingleRelayDialer` — the client's own
/// derivation depends on its (possibly frozen) `my_udp_relay_ok`, which the
/// server can't reproduce and which is what let the two ends disagree.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum RelayStrategyWire {
    /// v1 single-relay ANCHOR (allocate the one relay, advertise `R`).
    SingleRelayAnchor,
    /// v1 single-relay DIALER (no allocation; raw-dial the anchor's `R`).
    SingleRelayDialer,
    /// DERP `/derp` WS relay (both ends UDP-blocked).
    Derp,
    /// Two coturn allocations (the fall-through).
    BothAllocate,
    /// FR-19 — ride a tenant-owned org relay. A UNIT variant on purpose: this
    /// enum derives `Copy` and is serialised as a bare string tag, so the
    /// session itself travels out-of-band in `rc:overlay.relay_session`.
    /// Only ever emitted to a peer whose join advertised `supports_org_relay`;
    /// a pre-FR-19 agent that somehow received it decodes it to `None` via
    /// `relay_strategy_lenient` (#811) rather than losing the frame.
    OrgRelay,
}

/// Lenient decoder for [`NetmapPeer::relay_strategy`]: an **unrecognised**
/// tag becomes `None` instead of failing the parse.
///
/// ⚠️ Without this, [`RelayStrategyWire`]'s own doc comment is a lie with
/// fleet-wide consequences, and the failure is invisible. The enum is
/// externally tagged with only unit variants, so it decodes from a bare
/// string; `#[serde(default)]` on the field covers an **absent** value and
/// does nothing for an **unknown** one. An unknown tag is therefore a hard
/// serde error that fails the *whole enclosing `ServerMsg`* — not just this
/// field, not just this peer — and the agent's parse arm swallows that at
/// `debug!`. So the first server to add a variant would make every older
/// agent drop entire netmap frames and stop installing peers, silently.
///
/// `#[serde(other)]` cannot express this: serde permits it only on
/// internally or adjacently tagged enums, and this one is neither.
///
/// Unknown ⇒ `None` ⇒ the client computes the tier locally — precisely what
/// an absent field already means, so every existing consumer handles it with
/// no new arm. A non-string value degrades the same way rather than
/// exploding, so a future variant that grows a payload is survivable too.
fn relay_strategy_lenient<'de, D>(de: D) -> Result<Option<RelayStrategyWire>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let Some(raw) = Option::<serde_json::Value>::deserialize(de)? else {
        return Ok(None);
    };
    let serde_json::Value::String(tag) = raw else {
        return Ok(None);
    };
    // Re-parse through the derive so the kebab-case spellings live in exactly
    // one place; a rename cannot drift away from this shim.
    Ok(RelayStrategyWire::deserialize(
        serde::de::value::StrDeserializer::<serde::de::value::Error>::new(tag.as_str()),
    )
    .ok())
}

/// One peer in a netmap. `node_id` is the peer's `overlay_nodes._id`
/// (the stable handle the control plane uses for fan-out + ACL). The
/// node installs this peer as a WireGuard `Tunn` keyed by
/// `wg_public_key`, with `allowed_ips = overlay_ip/32`. `reachable` is
/// **server-precomputed** from the ACL — a forbidden peer is dropped
/// from the netmap entirely, so this is `true` for every peer the node
/// actually receives (the field is retained so a future soft-deny can
/// ship a peer with `reachable=false` without a wire change).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct NetmapPeer {
    #[serde(with = "oid_hex")]
    pub node_id: ObjectId,
    pub overlay_ip: String,
    /// Human-facing node name (Phase 0) — the MagicDNS label for this peer,
    /// unique per network. Empty from a pre-Phase-0 server (`#[serde(default)]`).
    #[serde(default)]
    pub name: String,
    pub wg_public_key: String,
    #[serde(default)]
    pub endpoints: Vec<String>,
    /// NAT-traversal Phase A — the peer's JOIN-TIME NIC-derived endpoints (the
    /// server's `lan_endpoints` bucket, verbatim — NOT unioned with the relay
    /// trickle like `endpoints`). A globally-routable address in here means the
    /// peer's NIC itself holds a public IP (bare-metal / no NAT in front), so
    /// any node can dial it directly without STUN — the *direct-to-public*
    /// carrier tier. Provenance matters: `endpoints` also contains coturn
    /// relayed addresses, and on this fleet the coturn worker IPs COLLIDE with
    /// host public IPs (the workers run on the same hosts), so a "public and
    /// not a coturn IP" heuristic over the union cannot distinguish them —
    /// only the join-time bucket can. Empty from a pre-Phase-A server
    /// (`#[serde(default)]`) → the public-direct tier stays inert.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lan_endpoints: Vec<String>,
    /// NAT-traversal Phase B — the peer's **server-reflexive** (srflx)
    /// candidates: its public `ip:port` as seen through its NAT, discovered by
    /// the peer querying a STUN server (`OverlayNetworkInfo.stun_urls`) on each
    /// of its own traffic sockets and trickled up via `rc:overlay.srflx`. Lets a
    /// node behind a 1:1 / cone NAT (e.g. a cloud exit whose NIC IP is private)
    /// become directly dialable without a relay: a dialer sends its WireGuard
    /// INIT here and the peer accepts it over the NAT mapping its own STUN query
    /// opened. Kept in a SEPARATE bucket from `endpoints` (relay) and
    /// `lan_endpoints` (public NIC) so each provenance stays distinct (CC2) and
    /// the relay trickle can't clobber it. Empty from a pre-Phase-B server or a
    /// node that gathered no public srflx (`#[serde(default)]`) → the srflx tier
    /// stays inert.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub srflx_endpoints: Vec<String>,
    /// Phase C — this peer's probed NAT mapping type (`"cone"` / `"symmetric"`),
    /// or `None` when unknown. A dialer skips the srflx punch only when BOTH ends
    /// are `"symmetric"` (neither can predict the other's per-destination port);
    /// any other combination attempts. Optional + omitted-when-None ⇒ a
    /// pre-Phase-C server/peer stays "unknown" (attempted, never skipped).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub srflx_nat: Option<String>,
    /// FR-19 — echoed from the peer's join so a node knows whether its peer can
    /// use an org relay (both ends must). Skipped when `false` so a pre-FR-19
    /// node's netmap shape is byte-identical.
    #[serde(default, skip_serializing_if = "<&bool as std::ops::Not>::not")]
    pub supports_org_relay: bool,
    /// Phase B (overlay v3) — this peer's MEASURED capability vector, or
    /// `None` when the peer hasn't measured (pre-B agent) or its vector went
    /// STALE (the server's freshness gate treats >3× cadence as absent — a
    /// stalled prober must not stay fleet-authoritative). Consumers fall
    /// back to the presence rules whenever this is `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caps: Option<CapVectorWire>,
    /// Dialer honesty (field 2026-08-16, CORPLAP-3) — can this peer
    /// raw-UDP-dial ARBITRARY high ports (the single-relay DIALER's job:
    /// its raw socket sends straight to a coturn relay-band port)? A srflx
    /// candidate only proves UDP to WELL-KNOWN ports (a corp egress that
    /// whitelists STUN:3478 still drops the ~10-13k relay band), so
    /// srflx-presence alone mis-assigns such hosts the dialer role and the
    /// pair dies undialed. `Some(false)` = the peer PROVED it can't (its
    /// dialer-role relay carriers convicted against ≥2 distinct peers);
    /// `Some(true)` = no such evidence; `None` = pre-honesty peer — both
    /// ends then keep the legacy srflx-only inputs, so a mixed-version
    /// pair can never split roles (presence of the field IS the
    /// capability signal).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub udp_dialer_ok: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay_home: Option<String>,
    /// C4 stage 2 — this peer's STANDING warm TURN allocation's relayed
    /// address (`worker-ip:port`), refreshed by its heartbeats while a leg
    /// is live. A dialer whose pair to this peer died can dial it
    /// IMMEDIATELY (validated against the coturn worker set like any
    /// anchor advert) — no waiting for the peer's per-pair relay
    /// advertisement to crawl through a possibly-captured control WS.
    /// `None` from a pre-stage-2 server or while the peer holds no leg.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warm_relay_endpoint: Option<String>,
    pub reachable: bool,
    /// rc.142 — this peer advertised it can carry WG over a QUIC-over-TURN
    /// relay carrier. A node only attempts the QUIC upgrade with a peer when
    /// both ends set this (else raw relay), so a capability mismatch can't
    /// leave one side on QUIC and the other on raw (which wouldn't decapsulate).
    #[serde(default)]
    pub supports_quic: bool,
    /// Phase D — this peer advertised it can run the v1 single-relay carrier
    /// (one anchor allocation + a raw dialer). The runtime picks single-relay
    /// only when both ends set this AND the local `OVERLAY_RELAY_SINGLE` flag is
    /// on, so a mismatch can't split a pair into anchor/dialer and deadlock.
    /// Absent from a pre-Phase-D server/peer ⇒ `false` (both-allocate).
    #[serde(default)]
    pub supports_relay_single: bool,
    /// Phase D (DERP) — this peer advertised it can carry WG over the
    /// pubkey-addressed `/derp` relay. The runtime falls to DERP only when BOTH
    /// ends set this AND both are UDP-blocked (the single-relay `(false,false)`
    /// arm) AND the local `OVERLAY_DERP` flag is on. Pre-DERP peer ⇒ `false`.
    #[serde(default)]
    pub supports_derp: bool,
    /// U2 — this peer advertised `supports_forced_derp`. Echoed per-peer (it
    /// was NOT before U2) so the client can tell whether a peer will honor a
    /// pin, and so the server-computed [`relay_strategy`](Self::relay_strategy)
    /// is only applied by a client whose peer also participates. Pre-U2
    /// server/peer ⇒ `false`.
    #[serde(default)]
    pub supports_forced_derp: bool,
    /// U2 — the server's relay-tier verdict for THIS edge (the recipient →
    /// this peer). Populated ONLY when both ends advertised
    /// `supports_server_relay_strategy`; `None` otherwise (and for every
    /// pre-U2 server), in which case the client computes the tier locally as
    /// before. Skipped-when-None so a pre-U2 node's wire shape is unchanged.
    ///
    /// ⚠️ Decodes through [`relay_strategy_lenient`], NOT the bare derive:
    /// an unknown tag from a newer server must degrade to `None`, not fail
    /// the whole `ServerMsg`. Do not "simplify" this back to `#[serde(default)]`.
    #[serde(
        default,
        deserialize_with = "relay_strategy_lenient",
        skip_serializing_if = "Option::is_none"
    )]
    pub relay_strategy: Option<RelayStrategyWire>,
    /// Data-probe — this peer's overlay engine answers the overlay-native
    /// echo probe inline. The prober uses the engine echo toward such peers
    /// (guaranteed responder, netstack included) and the ICMP probe toward
    /// the rest. Pre-capability server/peer ⇒ `false` ⇒ ICMP.
    #[serde(default)]
    pub supports_overlay_echo: bool,
    /// Phase A (overlay v3) — this peer advertised the DERP always-on floor
    /// (its `/derp` mux is permanently open + registered). A pair is floored
    /// at birth only when BOTH ends carry this. Pre-floor server/peer ⇒
    /// `false` ⇒ the lazy-mux rules apply unchanged.
    #[serde(default)]
    pub supports_derp_floor: bool,
    /// Phase 1 — subnet routes this peer is an **approved** router for (CIDR
    /// strings like `"192.168.1.0/24"`). A receiving node installs each CIDR
    /// into its router (allowed_ips) + OS route table pointing at this peer, so
    /// LAN behind the peer is reachable over the overlay. Empty for a normal
    /// (non-router) peer or a pre-Phase-1 server (`#[serde(default)]`).
    #[serde(default)]
    pub routes: Vec<String>,
    /// P3b-3 — the `agents._id` backing this overlay node, when the node is an
    /// agent (`OverlayNode.node_ref == Agent`). `None` for a tunnel-client node
    /// or a pre-P3b-3 server (`#[serde(default)]`). Lets a controlling node join
    /// this overlay peer to a daemon-originated tunnel flow (which is keyed by
    /// agent id, a DIFFERENT ObjectId namespace than `node_id` = overlay-node
    /// id) and surface `ConnectionType::Tunnel`. `option_oid_hex` keeps it a
    /// bare-hex string on the wire (NOT bson `{"$oid":…}`), matching `node_id`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "option_oid_hex"
    )]
    pub agent_id: Option<ObjectId>,
    /// P4 — **server-compiled ingress rules**: what THIS peer may address when
    /// its packets arrive at the recipient of this netmap. Per-(recipient,peer),
    /// which is why it lives on the peer entry rather than on the network.
    ///
    /// Compiled from `overlay_policies` by evaluating the REVERSE direction
    /// (this peer as the source, the recipient as the `via` router), so the node
    /// never sees policy documents and a policy edit rides the existing
    /// netmap-delta fan-out — no restart, no new message type.
    ///
    /// `None` vs `Some(vec![])` is load-bearing and must not be collapsed:
    /// **`None`** = no rules compiled (a pre-P4 server, or the tenant's
    /// `acl_mode` is `off`) ⇒ the node falls back to its coarse
    /// [`LocalScope`](../../tunnel_core/overlay/router/struct.LocalScope.html)
    /// check only. **`Some(vec![])`** = the ACL ran and granted this peer
    /// nothing ⇒ deny. Skipped when absent so a pre-P4 node's wire shape is
    /// unchanged (13 serde wire-lock tests depend on that).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ingress_rules: Option<Vec<crate::models::OverlayRule>>,
}

/// Network-wide parameters carried in a full netmap so the node can size
/// its TUN/MTU and validate its own address against the range.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct OverlayNetworkInfo {
    /// The block containing **this recipient's own** overlay address.
    ///
    /// FR-47 P5d — this used to be the network's single CIDR, and for every
    /// network that has not grown it still is exactly that value: a one-block
    /// address space has one block, and it is the one the node lives in. The
    /// field only becomes recipient-specific once a network holds more than
    /// one block, which requires `overlay.multi_block_enabled`.
    ///
    /// ⚠️ Why per-recipient rather than "the first block": a fielded agent
    /// derives its TUN netmask and its subnet-router NAT source scope from
    /// this string. Both are correct only for the block its OWN address lives
    /// in — sending block 0 to a node addressed in block 1 would mis-size its
    /// netmask, which is a fleet-wide failure mode, not a cosmetic one. Peers
    /// in *other* blocks stay reachable through the per-peer `/32`s the agent
    /// already installs, so a pre-P5d agent needs nothing else.
    pub cidr: String,
    /// FR-47 P5d — every block in the network's address space, in allocation
    /// order. `#[serde(default)]` so a pre-P5d server (which sends none) and a
    /// pre-P5d agent (which reads none) both behave exactly as before.
    ///
    /// A node that understands this uses the union — for RPF scope and for
    /// knowing the full extent of its own org. A node that does not still has
    /// `cidr`, which is correct for it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cidrs: Vec<String>,
    pub mtu: u16,
    /// Phase 2 MagicDNS — the tenant's overlay DNS suffix (e.g.
    /// `"myorg.roomler.net"`), or `None` when MagicDNS is off. A node with a
    /// domain set brings up its local split-DNS resolver. `#[serde(default)]`
    /// for pre-Phase-2 servers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub magic_domain: Option<String>,
    /// Phase 2 MagicDNS — upstream nameservers for non-overlay queries. Empty →
    /// the node's existing system resolvers.
    #[serde(default)]
    pub nameservers: Vec<String>,
    /// W2 MagicDNS — THIS node's own display name, so its local resolver can
    /// answer `<own-name>.<domain>` (the netmap's peer list excludes self,
    /// and the agent has no other authoritative source for its server-side
    /// name). `#[serde(default)]` + skip-when-None keeps both directions
    /// compatible with pre-W2 peers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub self_name: Option<String>,
    /// NAT-traversal Phase B — STUN server URLs the node queries (on each of its
    /// own traffic sockets) to discover its server-reflexive candidates — its
    /// public `ip:port` as seen through the NAT. Lets a peer/exit behind a 1:1
    /// (or cone) NAT become directly dialable without a relay. Typically the
    /// same coturn workers used for TURN (a `turn:` host doubles as a STUN
    /// server). The runtime has no per-peer ICE creds at join/`setup_direct`
    /// time (those arrive only via `RelayGrant`), so the STUN endpoints must
    /// ride the netmap itself. Empty → no srflx gathering (pre-Phase-B server,
    /// or STUN disabled). `#[serde(default)]` for back-compat.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stun_urls: Vec<String>,
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
    fn agent_hello_advertised_routes_default_and_roundtrip() {
        // New agent: round-trip preserves advertised_routes on the wire.
        let m = ClientMsg::AgentHello {
            machine_name: "host".into(),
            os: OsKind::Linux,
            agent_version: "0.3.0".into(),
            displays: vec![],
            caps: Box::new(AgentCaps::default()),
            advertised_routes: vec!["192.168.1.0/24".into()],
            supports_relay_regions: true,
            ssh_host_pubkey: String::new(),
        };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains(r#""t":"rc:agent.hello""#));
        assert!(s.contains(r#""advertised_routes":["192.168.1.0/24"]"#));
        match serde_json::from_str::<ClientMsg>(&s).unwrap() {
            ClientMsg::AgentHello {
                advertised_routes, ..
            } => assert_eq!(advertised_routes, vec!["192.168.1.0/24".to_string()]),
            other => panic!("wrong variant: {other:?}"),
        }

        // Old (pre-feature) agent: the same hello minus the field must
        // deserialize with advertised_routes defaulted to [] (wire back-compat).
        let mut obj = serde_json::to_value(&m).unwrap();
        obj.as_object_mut().unwrap().remove("advertised_routes");
        match serde_json::from_value::<ClientMsg>(obj).unwrap() {
            ClientMsg::AgentHello {
                advertised_routes, ..
            } => assert!(
                advertised_routes.is_empty(),
                "a hello without advertised_routes must default to none"
            ),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    /// P8 wire lock. The tag and the `kind` spellings are a compatibility
    /// surface with every deployed agent: renaming one doesn't fail loudly,
    /// it makes a fleet's activity reports silently unparseable — the same
    /// class the `RpcCap` wire test exists to prevent.
    #[test]
    fn ssh_activity_wire_shape_is_locked() {
        use crate::models::SshActivityKind;

        let m = ClientMsg::SshActivity {
            grant_id: Some("g1".into()),
            caller: "ssh:Someone@100.65.4.2:1".into(),
            kind: SshActivityKind::Exec,
            detail: Some("uptime".into()),
            exit_code: Some(0),
            allowed: true,
        };
        let v = serde_json::to_value(&m).unwrap();
        assert_eq!(v["t"], "rc:ssh.activity");
        assert_eq!(v["kind"], "exec");
        assert_eq!(v["allowed"], true);

        for (k, want) in [
            (SshActivityKind::SessionOpen, "session_open"),
            (SshActivityKind::SessionClose, "session_close"),
            (SshActivityKind::Exec, "exec"),
            (SshActivityKind::Shell, "shell"),
            (SshActivityKind::Sftp, "sftp"),
            (SshActivityKind::Forward, "forward"),
        ] {
            assert_eq!(
                serde_json::to_value(k).unwrap(),
                serde_json::Value::String(want.into()),
                "activity kind wire spelling changed"
            );
        }
    }

    /// The optional fields must all be omissible: a `session_open` carries no
    /// command, no exit code and no grant (key-list sessions). If any of them
    /// stopped defaulting, the commonest row on the wire would fail to parse.
    #[test]
    fn a_minimal_activity_report_round_trips() {
        use crate::models::SshActivityKind;

        let m = ClientMsg::SshActivity {
            grant_id: None,
            caller: String::new(),
            kind: SshActivityKind::SessionOpen,
            detail: None,
            exit_code: None,
            allowed: true,
        };
        let v = serde_json::to_value(&m).unwrap();
        let obj = v.as_object().unwrap();
        for absent in ["grant_id", "caller", "detail", "exit_code"] {
            assert!(!obj.contains_key(absent), "{absent} should be skipped");
        }
        match serde_json::from_value::<ClientMsg>(v).unwrap() {
            ClientMsg::SshActivity {
                grant_id,
                caller,
                kind,
                detail,
                exit_code,
                allowed,
            } => {
                assert!(grant_id.is_none() && detail.is_none() && exit_code.is_none());
                assert!(caller.is_empty() && allowed);
                assert_eq!(kind, SshActivityKind::SessionOpen);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn ssh_response_carries_the_host_key_on_the_device_leg() {
        // `roomler ssh` originates on THIS leg, and the dialling device has no
        // other channel to learn the key — so if it goes missing here, the
        // client is back to TOFU with no way to notice. It went missing once
        // already: the HTTP body gained the field and the hand-written wire
        // mapping kept sending the old shape, which compiles cleanly.
        const KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIExample target";
        let m = ServerMsg::SshResponse {
            request_id: "r1".into(),
            address: Some("100.65.4.30".into()),
            port: Some(2222),
            grant_id: Some("g1".into()),
            host_pubkey: Some(KEY.into()),
            expires_at_ms: Some(123),
            error: None,
        };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains(r#""host_pubkey""#), "must reach the wire");
        match serde_json::from_str::<ServerMsg>(&s).unwrap() {
            ServerMsg::SshResponse { host_pubkey, .. } => {
                assert_eq!(host_pubkey.as_deref(), Some(KEY))
            }
            other => panic!("wrong variant: {other:?}"),
        }

        // A refusal carries neither an address nor a key — they are only ever
        // meaningful together — and an older server that omits the field must
        // land on "cannot verify", not on something a client treats as a key.
        let refused = ServerMsg::SshResponse {
            request_id: "r2".into(),
            address: None,
            port: None,
            grant_id: None,
            host_pubkey: None,
            expires_at_ms: None,
            error: Some("nope".into()),
        };
        let s = serde_json::to_string(&refused).unwrap();
        assert!(!s.contains("host_pubkey"), "absent, not null");
        match serde_json::from_str::<ServerMsg>(&s).unwrap() {
            ServerMsg::SshResponse { host_pubkey, .. } => assert!(host_pubkey.is_none()),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn agent_hello_ssh_host_pubkey_roundtrips_and_absent_means_no_key() {
        const KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIExampleExampleExample host";
        let m = ClientMsg::AgentHello {
            machine_name: "host".into(),
            os: OsKind::Linux,
            agent_version: "0.3.0".into(),
            displays: vec![],
            caps: Box::new(AgentCaps::default()),
            advertised_routes: vec![],
            supports_relay_regions: true,
            ssh_host_pubkey: KEY.into(),
        };
        let s = serde_json::to_string(&m).unwrap();
        assert!(
            s.contains(r#""ssh_host_pubkey""#),
            "field must reach the wire"
        );
        match serde_json::from_str::<ClientMsg>(&s).unwrap() {
            ClientMsg::AgentHello {
                ssh_host_pubkey, ..
            } => assert_eq!(ssh_host_pubkey, KEY),
            other => panic!("wrong variant: {other:?}"),
        }

        // An agent that predates the field — the overwhelming majority of a
        // fleet on the day this ships — must still parse, and must land on
        // "no key" rather than anything a client could mistake for one.
        let mut obj = serde_json::to_value(&m).unwrap();
        obj.as_object_mut().unwrap().remove("ssh_host_pubkey");
        match serde_json::from_value::<ClientMsg>(obj).unwrap() {
            ClientMsg::AgentHello {
                ssh_host_pubkey, ..
            } => assert!(
                ssh_host_pubkey.is_empty(),
                "a hello without the field must mean NO host key, never a wildcard"
            ),
            other => panic!("wrong variant: {other:?}"),
        }

        // And an empty key must not be emitted at all, so "absent" and
        // "empty" cannot drift into two different meanings on the wire.
        let none = ClientMsg::AgentHello {
            machine_name: "host".into(),
            os: OsKind::Linux,
            agent_version: "0.3.0".into(),
            displays: vec![],
            caps: Box::new(AgentCaps::default()),
            advertised_routes: vec![],
            supports_relay_regions: true,
            ssh_host_pubkey: String::new(),
        };
        assert!(
            !serde_json::to_string(&none)
                .unwrap()
                .contains("ssh_host_pubkey"),
            "an empty host key must be omitted, not sent as \"\""
        );
    }

    /// Multi-region relay wire locks: exact tags, field defaults, and the
    /// hello capability flag's absent⇒false back-compat.
    #[test]
    fn relay_regions_wire_roundtrip_and_defaults() {
        let m = ServerMsg::RelayRegions {
            regions: vec![
                RelayRegionInfo {
                    id: "us-east".into(),
                    stun: "coturn-us-east.roomler.ai:3478".into(),
                    derp_url: Some("wss://derp-us-east.roomler.ai/derp".into()),
                    relay_band: Some((49152, 65535)),
                },
                RelayRegionInfo {
                    id: "eu-central".into(),
                    stun: "coturn.roomler.ai:3478".into(),
                    derp_url: None,
                    relay_band: None,
                },
            ],
            rev: 7,
        };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains(r#""t":"rc:relay.regions""#));
        assert!(s.contains(r#""stun":"coturn-us-east.roomler.ai:3478""#));
        // Absent derp_url is OMITTED, not null.
        assert!(!s.contains(r#""derp_url":null"#));
        match serde_json::from_str::<ServerMsg>(&s).unwrap() {
            ServerMsg::RelayRegions { regions, rev } => {
                assert_eq!(rev, 7);
                assert_eq!(regions.len(), 2);
                assert_eq!(regions[0].id, "us-east");
                assert!(regions[1].derp_url.is_none());
            }
            other => panic!("wrong variant: {other:?}"),
        }

        let r = ClientMsg::RelayProbeReport {
            results: vec![
                RelayRegionRtt {
                    region: "us-east".into(),
                    rtt_ms: Some(23),
                },
                RelayRegionRtt {
                    region: "eu-central".into(),
                    rtt_ms: None,
                },
            ],
        };
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains(r#""t":"rc:relay.probe_report""#));
        assert!(s.contains(r#""rtt_ms":23"#));
        match serde_json::from_str::<ClientMsg>(&s).unwrap() {
            ClientMsg::RelayProbeReport { results } => {
                assert_eq!(results[0].rtt_ms, Some(23));
                assert_eq!(results[1].rtt_ms, None, "all-timed-out region is None");
            }
            other => panic!("wrong variant: {other:?}"),
        }

        // Pre-feature hello (no flag) ⇒ supports_relay_regions == false, so
        // the server never pushes rc:relay.regions at an old agent.
        let hello = serde_json::json!({
            "t": "rc:agent.hello",
            "machine_name": "old-host",
            "os": "linux",
            "agent_version": "0.3.0-rc.200",
            "displays": [],
            "caps": AgentCaps::default(),
        });
        match serde_json::from_value::<ClientMsg>(hello).unwrap() {
            ClientMsg::AgentHello {
                supports_relay_regions,
                ..
            } => assert!(
                !supports_relay_regions,
                "absent capability flag must default to false"
            ),
            other => panic!("wrong variant: {other:?}"),
        }
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
            permissions: None,
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
            chroma_pref: None,
            chunk_framing: None,
            audio_enabled: false,
            override_reason: None,
            local_relay: None,
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
            chroma_pref: None,
            chunk_framing: None,
            audio_enabled: true,
            override_reason: None,
            local_relay: None,
        };
        let s = serde_json::to_string(&req_with_t).unwrap();
        assert!(s.contains("\"preferred_transport\":\"data-channel-vp9-444\""));

        // Loopback-TURN corp-relay (Phase 2): a set `local_relay` round-trips
        // with the browser's snake_case field names, and the default None is
        // skipped so the wire stays compatible with older controllers.
        assert!(
            !s.contains("local_relay"),
            "None local_relay must be skipped"
        );
        let req_lr = ClientMsg::SessionRequest {
            agent_id,
            permissions: Permissions::VIEW,
            browser_caps: vec![],
            preferred_transport: None,
            chroma_pref: None,
            chunk_framing: None,
            audio_enabled: false,
            override_reason: None,
            local_relay: Some(LocalRelayDescriptor {
                turn_port: 47989,
                overlay_ip: "100.64.0.7".into(),
                username: "1700000600:507f1f77bcf86cd799439012".into(),
                credential: "deadbeef".into(),
            }),
        };
        let s = serde_json::to_string(&req_lr).unwrap();
        assert!(
            s.contains("\"local_relay\":{"),
            "set local_relay serialises: {s}"
        );
        assert!(s.contains("\"turn_port\":47989"));
        assert!(s.contains("\"overlay_ip\":\"100.64.0.7\""));
        // Round-trips back to the same descriptor.
        let back: ClientMsg = serde_json::from_str(&s).unwrap();
        match back {
            ClientMsg::SessionRequest { local_relay, .. } => {
                let d = local_relay.expect("local_relay present");
                assert_eq!(d.turn_port, 47989);
                assert_eq!(d.overlay_ip, "100.64.0.7");
            }
            _ => panic!("expected SessionRequest"),
        }
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
            sys: None,
            srflx_count: Some(2),
            warm_relay: Some("5.9.157.221:12586".into()),
            companion_version: Some("0.4.16".into()),
            // FR-43 P2c — absent means "no news", which is the steady state.
            caps: None,
        };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains(r#""t":"rc:agent.heartbeat""#));
        assert!(s.contains(r#""rss_mb":142"#));
        assert!(s.contains(r#""cpu_pct":3.25"#));
        assert!(s.contains(r#""active_sessions":2"#));
        assert!(s.contains(r#""warm_relay":"5.9.157.221:12586""#));
        // v1 shape stays byte-identical: `sys: None` must not serialize.
        assert!(!s.contains(r#""sys""#));

        let back: ClientMsg = serde_json::from_str(&s).unwrap();
        match back {
            ClientMsg::AgentHeartbeat {
                rss_mb,
                cpu_pct,
                active_sessions,
                sys,
                srflx_count,
                warm_relay,
                companion_version,
                caps,
            } => {
                // FR-43 P2c — the steady state is ABSENT, and that is the
                // property worth asserting: caps ride the heartbeat only when
                // they change, so an ordinary beat must carry none.
                assert!(caps.is_none(), "an ordinary heartbeat must not carry caps");
                assert_eq!(rss_mb, 142);
                assert!((cpu_pct - 3.25).abs() < f32::EPSILON);
                assert_eq!(active_sessions, 2);
                assert!(sys.is_none());
                assert_eq!(srflx_count, Some(2));
                assert_eq!(warm_relay.as_deref(), Some("5.9.157.221:12586"));
                assert_eq!(companion_version.as_deref(), Some("0.4.16"));
            }
            other => panic!("wrong variant: {other:?}"),
        }
        assert!(s.contains(r#""companion_version":"0.4.16""#));
        // A stage-1 agent omits the field — decodes as None, and a
        // warm-less stage-2 agent's None must not serialize at all.
        let stage1 = r#"{"t":"rc:agent.heartbeat","rss_mb":0,"cpu_pct":0.0,"active_sessions":1}"#;
        let back: ClientMsg = serde_json::from_str(stage1).unwrap();
        assert!(matches!(
            back,
            ClientMsg::AgentHeartbeat {
                warm_relay: None,
                ..
            }
        ));
        // FR-27 — same rule for the companion version: a pre-FR-27 agent omits
        // it, and that must decode as "not reported", never as an empty string
        // the grid would render as a version.
        assert!(matches!(
            serde_json::from_str::<ClientMsg>(stage1).unwrap(),
            ClientMsg::AgentHeartbeat {
                companion_version: None,
                ..
            }
        ));
    }

    #[test]
    fn agent_heartbeat_sys_block_round_trips_and_v1_payload_parses() {
        // Stats PR-5 wire lock. Old agents omit `sys` entirely — the
        // server-side deserializer must default it.
        let v1 = r#"{"t":"rc:agent.heartbeat","rss_mb":0,"cpu_pct":0.0,"active_sessions":1}"#;
        let back: ClientMsg = serde_json::from_str(v1).unwrap();
        assert!(matches!(back, ClientMsg::AgentHeartbeat { sys: None, .. }));
        // A pre-feature agent omits the field entirely — that must decode as
        // None ("not reported"), never as a measured Some(0).
        assert!(matches!(
            serde_json::from_str::<ClientMsg>(v1).unwrap(),
            ClientMsg::AgentHeartbeat {
                srflx_count: None,
                ..
            }
        ));

        let m = ClientMsg::AgentHeartbeat {
            rss_mb: 0,
            cpu_pct: 0.0,
            active_sessions: 1,
            sys: Some(AgentSysStats {
                rss_mb: 87,
                cpu_pct: 1.5,
                net_rx_bytes: 123_456_789,
                net_tx_bytes: 987_654,
                direct: 3,
                relay: 1,
                derp: 1,
                peer_rtt_ms: Some(42),
                overlay_rx_bytes: 4_096,
                overlay_tx_bytes: 8_192,
                tunnel_rx_bytes: 65_536,
                tunnel_tx_bytes: 1_024,
                links: vec![
                    PeerLink {
                        node: "6a1f00000000000000000001".into(),
                        carrier: "direct".into(),
                        rtt_ms: Some(12),
                        stalled: false,
                        tx: 512,
                        rx: 256,
                        relay: None,
                    },
                    PeerLink {
                        node: "6a1f00000000000000000002".into(),
                        carrier: "relay".into(),
                        rtt_ms: Some(87),
                        stalled: false,
                        tx: 0,
                        rx: 0,
                        relay: Some("turn/udp".into()),
                    },
                ],
            }),
            // The value that matters operationally: a measured ZERO must be
            // distinguishable on the wire from an agent that doesn't report.
            srflx_count: Some(0),
            warm_relay: None,
            // No companion on this host — must not serialize at all, so a
            // server reading it cannot tell it apart from an old agent (both
            // mean "nothing to show").
            companion_version: None,
            caps: None,
        };
        let s = serde_json::to_string(&m).unwrap();
        assert!(
            s.contains(r#""srflx_count":0"#),
            "a measured zero must serialise, not be skipped: {s}"
        );
        assert!(s.contains(r#""net_rx_bytes":123456789"#));
        assert!(s.contains(r#""direct":3"#));
        assert!(s.contains(r#""peer_rtt_ms":42"#));
        // Wave 2: mesh edges ride the same block; an empty list is
        // skipped so a pre-mesh agent's payload stays byte-identical.
        assert!(s.contains(r#""carrier":"direct""#));
        assert!(
            !s.contains(r#""stalled""#),
            "false stalled must be skipped: {s}"
        );
        // Wave 3: per-edge and per-agent overlay volume. Unlike `stalled`
        // these are NOT skipped when zero — a mesh that genuinely moved no
        // bytes has to stay distinguishable from an agent too old to count.
        assert!(s.contains(r#""tx":512"#));
        assert!(s.contains(r#""rx":256"#));
        // Wave 4: the relay qualifier serialises when present and is SKIPPED
        // when absent — a direct edge must not carry a null `relay` key
        // (pre-wave-4 payloads stay byte-identical for such links).
        assert!(s.contains(r#""relay":"turn/udp""#));
        assert!(
            !s.contains(r#""relay":null"#),
            "a direct edge must omit `relay`, not null it: {s}"
        );
        assert!(s.contains(r#""overlay_rx_bytes":4096"#));
        assert!(s.contains(r#""overlay_tx_bytes":8192"#));
        assert!(s.contains(r#""tunnel_rx_bytes":65536"#));
        assert!(s.contains(r#""tunnel_tx_bytes":1024"#));
        let back: ClientMsg = serde_json::from_str(&s).unwrap();
        match back {
            ClientMsg::AgentHeartbeat { sys: Some(sys), .. } => {
                assert_eq!(sys.rss_mb, 87);
                assert_eq!(sys.net_tx_bytes, 987_654);
                assert_eq!(sys.derp, 1);
                assert_eq!(sys.peer_rtt_ms, Some(42));
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
            open_nonce: None,
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
        // `agents/roomlerd/src/signaling.rs:333` swallows it
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
            open_nonce: None,
            derp_pubkey: None,
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
    fn udp_forward_wire_discriminators_and_roundtrip() {
        let session_id = ObjectId::parse_str("507f1f77bcf86cd799439011").unwrap();
        // Client → server request.
        let req = ClientMsg::UdpForwardRequest {
            session_id,
            flow_id: 7,
            dst_host: "dns.intranet".into(),
            dst_port: 53,
        };
        let s = serde_json::to_string(&req).unwrap();
        assert!(s.contains(r#""t":"rc:tunnel.udp.request""#));
        assert!(s.contains(r#""dst_port":53"#));
        assert!(matches!(
            serde_json::from_str::<ClientMsg>(&s).unwrap(),
            ClientMsg::UdpForwardRequest {
                flow_id: 7,
                dst_port: 53,
                ..
            }
        ));

        // Client → server close (agent-emitted too).
        let closed = ClientMsg::UdpClosed {
            session_id,
            flow_id: 7,
            reason: CloseReason::IdleTimeout,
        };
        let s = serde_json::to_string(&closed).unwrap();
        assert!(s.contains(r#""t":"rc:tunnel.udp.closed""#));

        // Server → agent forward.
        let owner = ObjectId::parse_str("507f1f77bcf86cd799439012").unwrap();
        let fwd = ServerMsg::UdpForwardForward {
            session_id,
            flow_id: 7,
            dst_host: "dns.intranet".into(),
            dst_port: 53,
            owner_user_id: owner,
        };
        let s = serde_json::to_string(&fwd).unwrap();
        assert!(s.contains(r#""t":"rc:tunnel.udp.forward""#));
        assert!(matches!(
            serde_json::from_str::<ServerMsg>(&s).unwrap(),
            ServerMsg::UdpForwardForward {
                flow_id: 7,
                dst_port: 53,
                ..
            }
        ));

        // Server → client accept.
        let acc = ServerMsg::UdpForwardAccept {
            session_id,
            flow_id: 7,
            dc_index: 3,
        };
        let s = serde_json::to_string(&acc).unwrap();
        assert!(s.contains(r#""t":"rc:tunnel.udp.accept""#));
        assert!(s.contains(r#""dc_index":3"#));
    }

    #[test]
    fn destination_rule_proto_defaults_to_any_and_roundtrips() {
        use crate::models::{DestinationRule, HostPattern, PortRange, ProtocolKind};
        // A pre-UDP stored rule (no `proto` field) deserialises to Any.
        let legacy = r#"{"host_pattern":{"kind":"exact","value":"db"},"port_range":{"low":5432,"high":5432}}"#;
        let r: DestinationRule = serde_json::from_str(legacy).unwrap();
        assert_eq!(r.proto, ProtocolKind::Any);
        // Explicit proto round-trips snake_case.
        let udp = DestinationRule {
            host_pattern: HostPattern::Exact("dns".into()),
            port_range: PortRange { low: 53, high: 53 },
            proto: ProtocolKind::Udp,
        };
        let s = serde_json::to_string(&udp).unwrap();
        assert!(s.contains(r#""proto":"udp""#));
        assert_eq!(
            serde_json::from_str::<DestinationRule>(&s).unwrap().proto,
            ProtocolKind::Udp
        );
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
                bytes_in: 4_096,
                bytes_out: 512,
            };
            let s = serde_json::to_string(&m).unwrap();
            let back: ClientMsg = serde_json::from_str(&s).unwrap();
            match back {
                ClientMsg::TcpClosed {
                    reason,
                    bytes_in,
                    bytes_out,
                    ..
                } => {
                    assert_eq!(reason, r);
                    assert_eq!((bytes_in, bytes_out), (4_096, 512));
                }
                _ => panic!("wrong variant"),
            }
        }
    }

    /// Wave 3 — a pre-wave-3 client omits the byte counts entirely; the
    /// server must still parse the close and simply book zero, because a
    /// flow that fails to close would leak its audit row.
    #[test]
    fn tcp_closed_without_byte_counts_still_parses() {
        let legacy = r#"{"t":"rc:tunnel.tcp.closed","session_id":"6a1f00000000000000000001","flow_id":7,"reason":"eof"}"#;
        match serde_json::from_str::<ClientMsg>(legacy).unwrap() {
            ClientMsg::TcpClosed {
                flow_id,
                bytes_in,
                bytes_out,
                ..
            } => {
                assert_eq!(flow_id, 7);
                assert_eq!((bytes_in, bytes_out), (0, 0));
            }
            _ => panic!("wrong variant"),
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
            quic_auth_token: None,
            open_nonce: None,
        };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains(r#""t":"rc:tunnel.opened""#));
        assert!(s.contains(r#""dc_pool_size":8"#));
        assert!(s.contains(r#""sctp_rwnd_bytes":8388608"#));
        // Back-compat: a None quic_auth_token must NOT appear on the
        // wire, so a webrtc-dc-v1 controller predating the field parses
        // TunnelOpened unchanged. Same contract for the P3b-2 open_nonce.
        assert!(!s.contains("quic_auth_token"));
        assert!(!s.contains("open_nonce"));
    }

    #[test]
    fn tunnel_opened_carries_quic_auth_token_when_set() {
        let m = ServerMsg::TunnelOpened {
            session_id: ObjectId::new(),
            transport: "quic-v1".into(),
            dc_pool_size: 0,
            sctp_rwnd_bytes: 0,
            ice_servers: vec![],
            quic_auth_token: Some("tok-abc123".into()),
            open_nonce: None,
        };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains(r#""transport":"quic-v1""#));
        assert!(s.contains(r#""quic_auth_token":"tok-abc123""#));
        match serde_json::from_str::<ServerMsg>(&s).unwrap() {
            ServerMsg::TunnelOpened {
                quic_auth_token,
                transport,
                ..
            } => {
                assert_eq!(transport, "quic-v1");
                assert_eq!(quic_auth_token.as_deref(), Some("tok-abc123"));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn tunnel_opened_missing_quic_auth_token_defaults_to_none() {
        // An OLD server (no field) → the wire omits it → a NEW client
        // must deserialize None (serde default) and treat it as a
        // webrtc-dc-v1 session. Locks the back-compat path.
        let json = r#"{"t":"rc:tunnel.opened","session_id":"6a11682e804368d30edf57c6","transport":"webrtc-dc-v1","dc_pool_size":8,"sctp_rwnd_bytes":8388608,"ice_servers":[]}"#;
        match serde_json::from_str::<ServerMsg>(json).unwrap() {
            ServerMsg::TunnelOpened {
                quic_auth_token, ..
            } => assert_eq!(quic_auth_token, None),
            _ => panic!("wrong variant"),
        }
    }

    // ─── open_nonce correlation-id wire locks (P3b-2) ──────────────────
    //
    // The daemon multiplexes N tunnel-client opens over its single agent
    // WS and demuxes each `TunnelOpened` / open-failure `Error` by the
    // `open_nonce` it stamped on the `TunnelOpen`. These lock: (a) the
    // nonce round-trips on all three carriers when set, (b) it is OMITTED
    // (not null) when None so a pre-P3b-2 peer is byte-identical, and
    // (c) a wire frame with no `open_nonce` deserialises to None (the
    // single-open CLI + old-server safe-degrade path).

    #[test]
    fn tunnel_open_carries_open_nonce_when_set() {
        let m = ClientMsg::TunnelOpen {
            agent_id: ObjectId::new(),
            transport: "webrtc-dc-v1".into(),
            open_nonce: Some("nonce-7f3a".into()),
            derp_pubkey: None,
        };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains(r#""open_nonce":"nonce-7f3a""#));
        match serde_json::from_str::<ClientMsg>(&s).unwrap() {
            ClientMsg::TunnelOpen { open_nonce, .. } => {
                assert_eq!(open_nonce.as_deref(), Some("nonce-7f3a"));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn tunnel_open_omits_open_nonce_when_none() {
        // A single-open CLI sends None → the field must not appear, so a
        // pre-P3b-2 server (which has no such field) parses it unchanged.
        let m = ClientMsg::TunnelOpen {
            agent_id: ObjectId::new(),
            transport: "webrtc-dc-v1".into(),
            open_nonce: None,
            derp_pubkey: None,
        };
        let s = serde_json::to_string(&m).unwrap();
        assert!(
            !s.contains("open_nonce"),
            "None nonce leaked onto wire: {s}"
        );
    }

    #[test]
    fn tunnel_open_missing_open_nonce_defaults_to_none() {
        // A pre-P3b-2 client omits the field → a P3b-2 server must
        // deserialize None (serde default) and match the reply
        // positionally.
        let json = r#"{"t":"rc:tunnel.open","agent_id":"507f1f77bcf86cd799439012","transport":"webrtc-dc-v1"}"#;
        match serde_json::from_str::<ClientMsg>(json).unwrap() {
            ClientMsg::TunnelOpen { open_nonce, .. } => assert_eq!(open_nonce, None),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn tunnel_opened_carries_open_nonce_when_set() {
        let m = ServerMsg::TunnelOpened {
            session_id: ObjectId::new(),
            transport: "webrtc-dc-v1".into(),
            dc_pool_size: 8,
            sctp_rwnd_bytes: 8 * 1024 * 1024,
            ice_servers: vec![],
            quic_auth_token: None,
            open_nonce: Some("nonce-7f3a".into()),
        };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains(r#""open_nonce":"nonce-7f3a""#));
        match serde_json::from_str::<ServerMsg>(&s).unwrap() {
            ServerMsg::TunnelOpened { open_nonce, .. } => {
                assert_eq!(open_nonce.as_deref(), Some("nonce-7f3a"));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn error_carries_open_nonce_when_set() {
        // The open-failure carrier: the server rejects a TunnelOpen and
        // echoes the nonce so the daemon fails the exact pending flow.
        let e = ServerMsg::Error {
            session_id: None,
            code: "cross_tenant".into(),
            message: "agent belongs to a different tenant".into(),
            open_nonce: Some("nonce-dead".into()),
        };
        let s = serde_json::to_string(&e).unwrap();
        assert!(s.contains(r#""open_nonce":"nonce-dead""#));
        match serde_json::from_str::<ServerMsg>(&s).unwrap() {
            ServerMsg::Error {
                open_nonce, code, ..
            } => {
                assert_eq!(open_nonce.as_deref(), Some("nonce-dead"));
                assert_eq!(code, "cross_tenant");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn error_missing_open_nonce_defaults_to_none() {
        // A nonce-less Error mid-open (e.g. from a pre-P3b-2 server that
        // routed the open to the Hub) MUST decode to None so the daemon
        // reads it as "open rejected / server too old" and fails fast —
        // never hangs the pending waiter.
        let json = r#"{"t":"rc:error","session_id":null,"code":"boom","message":"x"}"#;
        match serde_json::from_str::<ServerMsg>(json).unwrap() {
            ServerMsg::Error { open_nonce, .. } => assert_eq!(open_nonce, None),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn tunnel_quic_ready_round_trips_with_distinct_discriminator() {
        let m = ClientMsg::TunnelQuicReady {
            session_id: ObjectId::new(),
            cert_fingerprint: "ab".repeat(32),
            addrs: vec!["203.0.113.7:51820".into(), "192.168.1.5:51820".into()],
            derp_pubkey: None,
        };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains(r#""t":"rc:tunnel.quic.ready""#));
        match serde_json::from_str::<ClientMsg>(&s).unwrap() {
            ClientMsg::TunnelQuicReady {
                cert_fingerprint,
                addrs,
                ..
            } => {
                assert_eq!(cert_fingerprint.len(), 64);
                assert_eq!(addrs.len(), 2);
            }
            _ => panic!("wrong variant"),
        }
    }

    /// R4 wire lock — the derp-flavor fields: absent ⇒ omitted (pre-R4
    /// byte-identical wire), present ⇒ they survive serialization + parse on
    /// BOTH enums. The server relays `ClientMsg::TunnelQuicReady` by
    /// REBUILDING `ServerMsg::TunnelQuicReady` — a lagging server struct is
    /// exactly where the field would silently vanish, so both sides lock.
    #[test]
    fn tunnel_derp_fields_lock_omission_and_presence() {
        let pk = "ab".repeat(32);
        let m = ClientMsg::TunnelOpen {
            agent_id: ObjectId::new(),
            transport: "quic-derp-v1".into(),
            open_nonce: None,
            derp_pubkey: Some(pk.clone()),
        };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains(&format!(r#""derp_pubkey":"{pk}""#)));

        let m = ServerMsg::TunnelQuicSetup {
            session_id: ObjectId::new(),
            quic_auth_token: "tok".into(),
            ice_servers: vec![],
            transport: Some("quic-derp-v1".into()),
            client_derp_pubkey: Some(pk.clone()),
        };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains(r#""transport":"quic-derp-v1""#));
        assert!(s.contains(&format!(r#""client_derp_pubkey":"{pk}""#)));
        match serde_json::from_str::<ServerMsg>(&s).unwrap() {
            ServerMsg::TunnelQuicSetup {
                transport,
                client_derp_pubkey,
                ..
            } => {
                assert_eq!(transport.as_deref(), Some("quic-derp-v1"));
                assert_eq!(client_derp_pubkey.as_deref(), Some(pk.as_str()));
            }
            other => panic!("wrong variant: {other:?}"),
        }

        // The agent→server ready and its server→client relay twin BOTH carry
        // the pubkey; None omits it on both.
        let m = ClientMsg::TunnelQuicReady {
            session_id: ObjectId::new(),
            cert_fingerprint: "cd".repeat(32),
            addrs: vec![],
            derp_pubkey: Some(pk.clone()),
        };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains(&format!(r#""derp_pubkey":"{pk}""#)));
        let m = ServerMsg::TunnelQuicReady {
            session_id: ObjectId::new(),
            cert_fingerprint: "cd".repeat(32),
            addrs: vec![],
            derp_pubkey: Some(pk.clone()),
        };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains(&format!(r#""derp_pubkey":"{pk}""#)));
        let m = ServerMsg::TunnelQuicReady {
            session_id: ObjectId::new(),
            cert_fingerprint: "cd".repeat(32),
            addrs: vec![],
            derp_pubkey: None,
        };
        let s = serde_json::to_string(&m).unwrap();
        assert!(!s.contains("derp_pubkey"), "None leaked onto the wire: {s}");
    }

    #[test]
    fn tunnel_quic_setup_is_server_to_agent() {
        let m = ServerMsg::TunnelQuicSetup {
            session_id: ObjectId::new(),
            quic_auth_token: "tok-xyz".into(),
            ice_servers: vec![IceServer {
                urls: vec!["turn:coturn.roomler.live:3478?transport=udp".into()],
                username: Some("1780000000:agent".into()),
                credential: Some("base64hmac".into()),
            }],
            transport: None,
            client_derp_pubkey: None,
        };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains(r#""t":"rc:tunnel.quic.setup""#));
        assert!(s.contains(r#""quic_auth_token":"tok-xyz""#));
        match serde_json::from_str::<ServerMsg>(&s).unwrap() {
            ServerMsg::TunnelQuicSetup { ice_servers, .. } => {
                assert_eq!(ice_servers.len(), 1, "agent gets its own TURN creds");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn tunnel_quic_setup_back_compat_no_ice_servers() {
        // A pre-3c server omits `ice_servers`; a 3c+ agent must still
        // deserialize it (serde default → empty), treating the absence
        // as "no relay creds, direct only".
        let json = r#"{"t":"rc:tunnel.quic.setup","session_id":"69e2a1ee7af054f8a14e84c6","quic_auth_token":"tok"}"#;
        match serde_json::from_str::<ServerMsg>(json).unwrap() {
            ServerMsg::TunnelQuicSetup { ice_servers, .. } => assert!(ice_servers.is_empty()),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn tunnel_quic_candidate_round_trips_both_directions() {
        // client → server
        let c = ClientMsg::TunnelQuicCandidate {
            session_id: ObjectId::new(),
            addrs: vec!["94.130.141.74:49160".into()],
        };
        let cs = serde_json::to_string(&c).unwrap();
        assert!(cs.contains(r#""t":"rc:tunnel.quic.candidate""#));
        match serde_json::from_str::<ClientMsg>(&cs).unwrap() {
            ClientMsg::TunnelQuicCandidate { addrs, .. } => assert_eq!(addrs.len(), 1),
            _ => panic!("wrong client variant"),
        }
        // server → agent (same wire tag, mirror variant)
        let s = ServerMsg::TunnelQuicCandidate {
            session_id: ObjectId::new(),
            addrs: vec!["94.130.141.74:49160".into(), "10.0.0.4:51820".into()],
        };
        let ss = serde_json::to_string(&s).unwrap();
        assert!(ss.contains(r#""t":"rc:tunnel.quic.candidate""#));
        match serde_json::from_str::<ServerMsg>(&ss).unwrap() {
            ServerMsg::TunnelQuicCandidate { addrs, .. } => assert_eq!(addrs.len(), 2),
            _ => panic!("wrong server variant"),
        }
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

    // ─── rc:overlay.* wire-format locks ───────────────────────────────

    /// A netmap missing `epoch` still decodes.
    ///
    /// `epoch` was REQUIRED and read by nobody: the agent destructures
    /// `{ self_ip, network, peers, .. }`, and the resync it was added for was
    /// never built. So a sender that omitted it failed the whole `ServerMsg`
    /// parse, and the node got no address and no peers — a total mesh failure
    /// caused by a field with no consumer.
    ///
    /// This is the shape of the bug, not just the instance: any required field
    /// on a frame the agent must parse can silently destroy it. The parse arm
    /// in `signaling.rs` now WARNs when an `rc:`-tagged frame fails to decode,
    /// so the next one is visible instead of invisible.
    #[test]
    fn a_netmap_without_epoch_still_decodes_and_does_not_strand_the_node() {
        let no_epoch = r#"{"t":"rc:overlay.netmap","self_ip":"100.65.0.7",
            "network":{"cidr":"100.65.0.0/22","mtu":1280},"peers":[]}"#;
        match serde_json::from_str::<ServerMsg>(no_epoch).unwrap() {
            ServerMsg::OverlayNetmap {
                self_ip,
                epoch,
                peers,
                ..
            } => {
                assert_eq!(self_ip, "100.65.0.7", "the address still arrives");
                assert_eq!(epoch, 0, "a missing epoch defaults rather than failing");
                assert!(peers.is_empty());
            }
            other => panic!("expected OverlayNetmap, got {other:?}"),
        }

        // Same for a delta — where the cost of losing one is worse: a dropped
        // REMOVAL leaves a node routing to an address that may since have been
        // recycled to a different device.
        let delta = r#"{"t":"rc:overlay.netmap_delta","upserts":[],
            "removes":["6a95ba653d54d39b773c56ba"]}"#;
        match serde_json::from_str::<ServerMsg>(delta).unwrap() {
            ServerMsg::OverlayNetmapDelta { epoch, removes, .. } => {
                assert_eq!(epoch, 0);
                assert_eq!(
                    removes.iter().map(|o| o.to_hex()).collect::<Vec<_>>(),
                    vec!["6a95ba653d54d39b773c56ba".to_string()],
                    "the removal survives a missing epoch"
                );
            }
            other => panic!("expected OverlayNetmapDelta, got {other:?}"),
        }
    }

    /// FR-47 P5d — a netmap from a pre-P5d server decodes, and carries no
    /// block list rather than an invented one.
    #[test]
    fn a_pre_p5d_netmap_still_decodes_and_carries_no_block_list() {
        // FR-47 P5d — the compatibility property the whole phase rests on: a
        // netmap from a server that has never heard of `cidrs` must decode, and
        // must leave the field EMPTY rather than inventing a list. An agent
        // then falls back to `cidr`, which is what it always used.
        let old = r#"{"t":"rc:overlay.netmap","self_ip":"100.65.0.7",
            "network":{"cidr":"100.65.0.0/22","mtu":1280},"peers":[],"epoch":1}"#;
        match serde_json::from_str::<ServerMsg>(old).unwrap() {
            ServerMsg::OverlayNetmap { network, .. } => {
                assert_eq!(network.cidr, "100.65.0.0/22");
                assert!(
                    network.cidrs.is_empty(),
                    "a pre-P5d server sends no block list; inventing one would make \
                     the agent trust a space the server never described"
                );
            }
            other => panic!("expected OverlayNetmap, got {other:?}"),
        }

        // And the reverse direction: a P5d server's netmap for a network that
        // has NOT grown carries a one-element list whose only entry equals
        // `cidr`. That equality is what makes multi-block inert on every
        // deployment that exists today.
        let net = OverlayNetworkInfo {
            cidr: "100.65.0.0/22".into(),
            cidrs: vec!["100.65.0.0/22".into()],
            mtu: 1280,
            magic_domain: None,
            nameservers: vec![],
            self_name: None,
            stun_urls: vec![],
        };
        let s = serde_json::to_string(&net).unwrap();
        let back: OverlayNetworkInfo = serde_json::from_str(&s).unwrap();
        assert_eq!(back.cidrs, vec![back.cidr.clone()]);
    }

    /// FR-68 / FR-47 P5d — the half of the compatibility claim that was missing.
    ///
    /// The test above proves OLD-server → NEW-decoder. This proves the direction
    /// that actually carries risk, and the one FR-47 flagged as unargued: a P5d
    /// server's netmap **for a GROWN org**, decoded by an agent that has never
    /// heard of `cidrs`.
    ///
    /// The decoder below is copied VERBATIM from `agent-v0.4.42` — the last
    /// release before P5d (`agent-v0.4.43` is the first with it). ⚠️ Do not
    /// "modernise" it or share it with the real types: its entire value is that
    /// it is frozen. It has no `cidrs` and no `deny_unknown_fields`, and that
    /// pairing is the property under test — serde ignores an unknown FIELD,
    /// while an unknown internally-tagged VARIANT would fail the whole frame
    /// (locked separately by
    /// `pre_rc53_server_msg_rejects_goodbye_so_agent_err_arm_fires`).
    ///
    /// ⚠️ What this does NOT prove: that the SERVER picks the right block. The
    /// netmap here is hand-built, so `cidr` is whatever this test wrote. The
    /// server's per-recipient choice is asserted end-to-end in
    /// `overlay_growth_tests`, against a netmap the server actually sent.
    #[test]
    fn a_grown_org_netmap_decodes_on_a_pinned_pre_p5d_agent() {
        #[derive(Deserialize)]
        struct PinnedNetworkInfo {
            cidr: String,
            #[allow(dead_code)]
            mtu: u16,
        }

        #[derive(Deserialize)]
        #[serde(tag = "t")]
        enum PinnedServerMsg {
            #[serde(rename = "rc:overlay.netmap")]
            OverlayNetmap {
                self_ip: String,
                network: PinnedNetworkInfo,
            },
        }

        // A GROWN org: block 0 is full, block 1 was appended. This node holds an
        // ordinal in block 1, so P5d sends it block 1 as its own `cidr`.
        let grown = ServerMsg::OverlayNetmap {
            self_ip: "100.65.8.1".into(),
            network: OverlayNetworkInfo {
                cidr: "100.65.8.0/22".into(),
                cidrs: vec!["100.65.4.0/22".into(), "100.65.8.0/22".into()],
                mtu: 1280,
                magic_domain: None,
                nameservers: vec![],
                self_name: None,
                stun_urls: vec![],
            },
            peers: vec![],
            epoch: 7,
        };
        let wire = serde_json::to_string(&grown).unwrap();

        let decoded: PinnedServerMsg = serde_json::from_str(&wire).expect(
            "a pre-P5d agent must still decode a grown-org netmap; if this fails, \
             every agent below 0.4.43 is stranded the moment an org grows",
        );
        let PinnedServerMsg::OverlayNetmap { self_ip, network } = decoded;

        // The on-link prefix the old agent derives contains its own address and
        // excludes the other block — checked structurally, never by string
        // prefix. FR-47 recorded that trap: a `starts_with` test passed against
        // wrong behaviour.
        let (base, plen) = network.cidr.split_once('/').unwrap();
        let base: std::net::Ipv4Addr = base.parse().unwrap();
        let plen: u32 = plen.parse().unwrap();
        let mask = u32::MAX << (32 - plen);
        let contains = |ip: &str| {
            let ip: std::net::Ipv4Addr = ip.parse().unwrap();
            u32::from(ip) & mask == u32::from(base) & mask
        };
        assert!(contains(&self_ip), "the node's own address must be on-link");
        assert!(
            !contains("100.65.4.7"),
            "a block-0 address must NOT fall inside block 1's on-link prefix — \
             cross-block peers are reached by their per-peer /32s"
        );
    }

    /// FR-47 — the refusal frame round-trips, and an OLD node that omits
    /// `supports_join_refusal` decodes as `false` so the server withholds it.
    #[test]
    fn overlay_join_refused_roundtrips_and_the_capability_defaults_off() {
        let m = ServerMsg::OverlayJoinRefused {
            reason: OverlayJoinRefusal::AddressSpaceExhausted,
            detail: "every host ordinal up to 1022 is leased".into(),
        };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains(r#""t":"rc:overlay.join_refused""#));
        assert!(s.contains(r#""reason":"address_space_exhausted""#));
        match serde_json::from_str::<ServerMsg>(&s).unwrap() {
            ServerMsg::OverlayJoinRefused { reason, detail } => {
                assert_eq!(reason, OverlayJoinRefusal::AddressSpaceExhausted);
                assert!(
                    !reason.is_retryable(),
                    "a full block does not empty on retry"
                );
                assert!(detail.contains("1022"));
            }
            other => panic!("expected OverlayJoinRefused, got {other:?}"),
        }

        // A pre-FR-47 join carries no capability field at all.
        let old = r#"{"t":"rc:overlay.join","wg_public_key":"k","key_epoch":0,
            "supported":[],"mtu":1280,"endpoints":[]}"#;
        match serde_json::from_str::<ClientMsg>(old).unwrap() {
            ClientMsg::OverlayJoin {
                supports_join_refusal,
                ..
            } => assert!(
                !supports_join_refusal,
                "an older node must default OFF, so the server never sends it a \
                 frame it cannot parse"
            ),
            other => panic!("expected OverlayJoin, got {other:?}"),
        }
    }

    /// A reason this build has never heard of must not fail the frame — the
    /// same forward-compat hatch `AgentCloseReason` has. It lands on
    /// `Unknown`, which is deliberately NOT retryable: a refusal we cannot
    /// interpret is not one we can safely retry against.
    #[test]
    fn an_unknown_refusal_reason_decodes_to_unknown_and_is_not_retryable() {
        let json = r#"{"t":"rc:overlay.join_refused","reason":"quota_exceeded_v9",
            "detail":"from a newer server"}"#;
        match serde_json::from_str::<ServerMsg>(json).unwrap() {
            ServerMsg::OverlayJoinRefused { reason, detail } => {
                assert_eq!(reason, OverlayJoinRefusal::Unknown);
                assert!(!reason.is_retryable());
                assert_eq!(detail, "from a newer server");
            }
            other => panic!("expected OverlayJoinRefused, got {other:?}"),
        }
    }

    #[test]
    fn overlay_join_roundtrip() {
        let m = ClientMsg::OverlayJoin {
            network_hint: None,
            wg_public_key: "cHVia2V5".into(),
            key_epoch: 0,
            supported: vec!["wireguard-v1".into(), "quic-v1".into()],
            mtu: 1280,
            endpoints: vec!["203.0.113.5:51820".into()],
            supports_quic: true,
            supports_relay_single: true,
            supports_derp: true,
            supports_forced_derp: true,
            supports_overlay_echo: false,
            supports_org_relay: false,
            supports_join_refusal: true,
            org_primary: None,
            relay_port: None,
            supports_server_relay_strategy: true,
            supports_derp_floor: true,
            advertised_routes: vec!["192.168.1.0/24".into()],
        };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains(r#""t":"rc:overlay.join""#));
        match serde_json::from_str::<ClientMsg>(&s).unwrap() {
            ClientMsg::OverlayJoin {
                wg_public_key,
                mtu,
                supported,
                supports_quic,
                supports_relay_single,
                supports_derp,
                supports_forced_derp,
                supports_server_relay_strategy,
                supports_derp_floor,
                advertised_routes,
                ..
            } => {
                assert_eq!(wg_public_key, "cHVia2V5");
                assert_eq!(mtu, 1280);
                assert!(supported.iter().any(|t| t == "wireguard-v1"));
                assert!(supports_quic, "supports_quic must round-trip");
                assert!(
                    supports_relay_single,
                    "supports_relay_single must round-trip"
                );
                assert!(supports_derp, "supports_derp must round-trip");
                assert!(
                    supports_forced_derp,
                    "supports_forced_derp must round-trip (P7)"
                );
                assert!(
                    supports_server_relay_strategy,
                    "supports_server_relay_strategy must round-trip (U2)"
                );
                assert!(
                    supports_derp_floor,
                    "supports_derp_floor must round-trip (Phase A)"
                );
                assert_eq!(advertised_routes, vec!["192.168.1.0/24".to_string()]);
            }
            other => panic!("expected OverlayJoin, got {other:?}"),
        }
        // P7 back-compat lock: a pre-P7 join (no supports_forced_derp key)
        // parses with the flag defaulted false.
        let legacy = r#"{"t":"rc:overlay.join","wg_public_key":"cHVia2V5","mtu":1280}"#;
        match serde_json::from_str::<ClientMsg>(legacy).unwrap() {
            ClientMsg::OverlayJoin {
                supports_forced_derp,
                supports_overlay_echo,
                ..
            } => {
                assert!(!supports_forced_derp, "absent flag must default false");
                // Data-probe back-compat lock: a pre-capability join (no
                // supports_overlay_echo key) defaults false ⇒ peers probe
                // this node with ICMP, never the overlay-native echo.
                assert!(
                    !supports_overlay_echo,
                    "absent supports_overlay_echo must default false"
                );
            }
            other => panic!("expected OverlayJoin, got {other:?}"),
        }
    }

    /// P7 — `rc:overlay.force_derp` wire-format lock: hex node id + ttl_ms
    /// round-trip, and the tag string is stable.
    #[test]
    fn overlay_force_derp_roundtrip() {
        let id = ObjectId::new();
        let m = ServerMsg::OverlayForceDerp {
            peer_node_id: id,
            ttl_ms: 1_800_000,
            derp_url: None,
        };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains(r#""t":"rc:overlay.force_derp""#));
        assert!(
            s.contains(&id.to_hex()),
            "node id must serialize as raw hex"
        );
        // Wire-compat lock: a central-DERP pin (None) serializes WITHOUT the
        // field — byte-identical to the pre-region message, so old agents see
        // exactly what they always saw.
        assert!(!s.contains("derp_url"));
        match serde_json::from_str::<ServerMsg>(&s).unwrap() {
            ServerMsg::OverlayForceDerp {
                peer_node_id,
                ttl_ms,
                derp_url,
            } => {
                assert_eq!(peer_node_id, id);
                assert_eq!(ttl_ms, 1_800_000);
                assert_eq!(derp_url, None);
            }
            other => panic!("expected OverlayForceDerp, got {other:?}"),
        }
        // Regional pin round-trips the url.
        let m = ServerMsg::OverlayForceDerp {
            peer_node_id: id,
            ttl_ms: 1,
            derp_url: Some("wss://derp-us-east.roomler.ai/derp".into()),
        };
        let s = serde_json::to_string(&m).unwrap();
        match serde_json::from_str::<ServerMsg>(&s).unwrap() {
            ServerMsg::OverlayForceDerp { derp_url, .. } => {
                assert_eq!(
                    derp_url.as_deref(),
                    Some("wss://derp-us-east.roomler.ai/derp")
                );
            }
            other => panic!("expected OverlayForceDerp, got {other:?}"),
        }
    }

    /// Multi-region DERP ticket wire locks.
    #[test]
    fn derp_ticket_request_and_reply_roundtrip() {
        let m = ClientMsg::DerpTicketRequest {};
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains(r#""t":"rc:relay.derp_ticket_request""#));
        assert!(matches!(
            serde_json::from_str::<ClientMsg>(&s).unwrap(),
            ClientMsg::DerpTicketRequest {}
        ));

        let m = ServerMsg::DerpTicket {
            ticket: "eyJ.header.sig".into(),
            exp: 1_900_000_000,
        };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains(r#""t":"rc:relay.derp_ticket""#));
        match serde_json::from_str::<ServerMsg>(&s).unwrap() {
            ServerMsg::DerpTicket { ticket, exp } => {
                assert_eq!(ticket, "eyJ.header.sig");
                assert_eq!(exp, 1_900_000_000);
            }
            other => panic!("expected DerpTicket, got {other:?}"),
        }
    }

    #[test]
    fn agent_update_now_roundtrip() {
        // pin omitted entirely when None (wire-compat lock: old servers /
        // agents never see a null field).
        let m = ServerMsg::UpdateNow { pin: None };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains(r#""t":"rc:agent.update""#));
        assert!(!s.contains("pin"), "pin must be omitted when None: {s}");
        match serde_json::from_str::<ServerMsg>(&s).unwrap() {
            ServerMsg::UpdateNow { pin } => assert_eq!(pin, None),
            other => panic!("expected UpdateNow, got {other:?}"),
        }

        let m = ServerMsg::UpdateNow {
            pin: Some("agent-v0.3.0-rc.260".to_string()),
        };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains(r#""pin":"agent-v0.3.0-rc.260""#));
        match serde_json::from_str::<ServerMsg>(&s).unwrap() {
            ServerMsg::UpdateNow { pin } => {
                assert_eq!(pin.as_deref(), Some("agent-v0.3.0-rc.260"))
            }
            other => panic!("expected UpdateNow, got {other:?}"),
        }
    }

    // ─── FR-40 key-rotation wire locks ───────────────────────────────

    /// The order carries NO key material — and never may. A future field
    /// named like a key would turn "the server orders a re-mint it never
    /// sees" into a delivery path, so the frame's whole shape is pinned.
    #[test]
    fn key_rotate_order_carries_no_key_shaped_field() {
        let m = ServerMsg::KeyRotate {
            request_id: "req-1".to_string(),
        };
        let v: serde_json::Value = serde_json::to_value(&m).unwrap();
        assert_eq!(v["t"], "rc:agent.key_rotate");
        let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            ["request_id", "t"],
            "the order is exactly {{t, request_id}}: {v}"
        );
        for k in &keys {
            let k = k.to_ascii_lowercase();
            assert!(
                !k.contains("key") && !k.contains("secret") && !k.contains("priv"),
                "a key-shaped field on the ORDER would make the server a key path: {k}"
            );
        }
        match serde_json::from_str::<ServerMsg>(&serde_json::to_string(&m).unwrap()).unwrap() {
            ServerMsg::KeyRotate { request_id } => assert_eq!(request_id, "req-1"),
            other => panic!("expected KeyRotate, got {other:?}"),
        }
    }

    #[test]
    fn key_rotated_report_roundtrip_and_public_only() {
        use crate::models::KeyRotationOutcome;
        let m = ClientMsg::KeyRotated {
            request_id: "req-1".to_string(),
            outcome: KeyRotationOutcome::Rotated,
            old_public_key: Some("old==".to_string()),
            new_public_key: Some("new==".to_string()),
            key_epoch: 3,
            detail: None,
        };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains(r#""t":"rc:agent.key_rotated""#));
        assert!(s.contains(r#""outcome":"rotated""#));
        assert!(!s.contains("detail"), "detail omitted when None: {s}");
        assert!(!s.contains("secret"), "{s}");
        match serde_json::from_str::<ClientMsg>(&s).unwrap() {
            ClientMsg::KeyRotated {
                request_id,
                outcome,
                new_public_key,
                key_epoch,
                ..
            } => {
                assert_eq!(request_id, "req-1");
                assert_eq!(outcome, KeyRotationOutcome::Rotated);
                assert_eq!(new_public_key.as_deref(), Some("new=="));
                assert_eq!(key_epoch, 3);
            }
            other => panic!("expected KeyRotated, got {other:?}"),
        }
        // A refusal parses with no keys and a detail.
        let refused = r#"{"t":"rc:agent.key_rotated","request_id":"r","outcome":"disabled","detail":"overlay_key_rotation=false"}"#;
        match serde_json::from_str::<ClientMsg>(refused).unwrap() {
            ClientMsg::KeyRotated {
                outcome, key_epoch, ..
            } => {
                assert_eq!(outcome, KeyRotationOutcome::Disabled);
                assert_eq!(key_epoch, 0);
            }
            other => panic!("expected KeyRotated, got {other:?}"),
        }
    }

    // ─── Fleet RPC wire locks ────────────────────────────────────────

    #[test]
    fn rpc_exec_roundtrip() {
        let m = ServerMsg::RpcExec {
            request_id: "r1".into(),
            shell: "pwsh".into(),
            command: "Get-NetRoute -AddressFamily IPv4".into(),
            timeout_ms: 30_000,
            max_output_bytes: 262_144,
            cwd: None,
            caller: "goran".into(),
            consent_mode: Some(crate::models::ConsentMode::Auto),
        };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains(r#""t":"rc:rpc.exec""#));
        assert!(s.contains(r#""consent_mode":"auto""#));
        // `cwd` omitted entirely when None — an older decoder must never see
        // a null it has to special-case.
        assert!(!s.contains("cwd"), "cwd must be omitted when None: {s}");
        match serde_json::from_str::<ServerMsg>(&s).unwrap() {
            ServerMsg::RpcExec {
                request_id,
                shell,
                timeout_ms,
                max_output_bytes,
                caller,
                ..
            } => {
                assert_eq!(request_id, "r1");
                assert_eq!(shell, "pwsh");
                assert_eq!(timeout_ms, 30_000);
                assert_eq!(max_output_bytes, 262_144);
                assert_eq!(caller, "goran");
            }
            other => panic!("expected RpcExec, got {other:?}"),
        }

        // An absent consent_mode must decode to None — which the agent reads
        // as "prompt". The fail-safe direction for a gate that grants root is
        // to ask, so a frame from an older/partial server can never be taken
        // as blanket unattended approval.
        let bare = r#"{"t":"rc:rpc.exec","request_id":"r2","shell":"bash",
            "command":"id","timeout_ms":1000,"max_output_bytes":1024,"caller":"x"}"#;
        match serde_json::from_str::<ServerMsg>(bare).unwrap() {
            ServerMsg::RpcExec { consent_mode, .. } => assert_eq!(consent_mode, None),
            other => panic!("expected RpcExec, got {other:?}"),
        }

        let c = ServerMsg::RpcCancel {
            request_id: "r1".into(),
        };
        let s = serde_json::to_string(&c).unwrap();
        assert!(s.contains(r#""t":"rc:rpc.cancel""#));
        assert!(matches!(
            serde_json::from_str::<ServerMsg>(&s).unwrap(),
            ServerMsg::RpcCancel { .. }
        ));
    }

    #[test]
    fn rpc_result_roundtrip() {
        let m = ClientMsg::RpcResult {
            request_id: "r1".into(),
            exit_code: Some(0),
            stdout: "ok".into(),
            stderr: String::new(),
            truncated: false,
            duration_ms: 12,
            error: None,
        };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains(r#""t":"rc:rpc.result""#));
        // Empty streams + a None error stay off the wire: a fleet-wide
        // heartbeat-rate message shouldn't carry three empty strings.
        assert!(!s.contains("stderr"), "empty stderr must be omitted: {s}");
        assert!(!s.contains("error"), "None error must be omitted: {s}");
        match serde_json::from_str::<ClientMsg>(&s).unwrap() {
            ClientMsg::RpcResult {
                exit_code,
                stdout,
                stderr,
                error,
                ..
            } => {
                assert_eq!(exit_code, Some(0));
                assert_eq!(stdout, "ok");
                assert_eq!(stderr, "");
                assert_eq!(error, None);
            }
            other => panic!("expected RpcResult, got {other:?}"),
        }

        // A refusal / timeout carries `error` and no exit code — the shape a
        // caller must be able to distinguish from "ran and exited 0".
        let m = ClientMsg::RpcResult {
            request_id: "r2".into(),
            exit_code: None,
            stdout: String::new(),
            stderr: String::new(),
            truncated: false,
            duration_ms: 30_000,
            error: Some("timed out after 30000ms".into()),
        };
        let s = serde_json::to_string(&m).unwrap();
        match serde_json::from_str::<ClientMsg>(&s).unwrap() {
            ClientMsg::RpcResult {
                exit_code, error, ..
            } => {
                assert_eq!(exit_code, None);
                assert_eq!(error.as_deref(), Some("timed out after 30000ms"));
            }
            other => panic!("expected RpcResult, got {other:?}"),
        }
    }

    #[test]
    fn rpc_request_and_response_roundtrip() {
        let m = ClientMsg::RpcExecRequest {
            request_id: "c1".into(),
            target: "winhost-a".into(),
            shell: "pwsh".into(),
            command: "Get-NetAdapter".into(),
            timeout_ms: 0,
        };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains(r#""t":"rc:rpc.request""#));
        match serde_json::from_str::<ClientMsg>(&s).unwrap() {
            ClientMsg::RpcExecRequest { target, shell, .. } => {
                assert_eq!(target, "winhost-a");
                assert_eq!(shell, "pwsh");
            }
            other => panic!("expected RpcExecRequest, got {other:?}"),
        }

        let r = ServerMsg::RpcExecResponse {
            request_id: "c1".into(),
            exit_code: Some(1),
            stdout: String::new(),
            stderr: "boom".into(),
            truncated: true,
            duration_ms: 5,
            error: None,
        };
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains(r#""t":"rc:rpc.response""#));
        assert!(s.contains(r#""truncated":true"#));
        match serde_json::from_str::<ServerMsg>(&s).unwrap() {
            ServerMsg::RpcExecResponse {
                exit_code,
                stderr,
                truncated,
                ..
            } => {
                assert_eq!(exit_code, Some(1));
                assert_eq!(stderr, "boom");
                assert!(truncated);
            }
            other => panic!("expected RpcExecResponse, got {other:?}"),
        }
    }

    #[test]
    fn rpc_caps_absent_on_older_agents() {
        // An agent that predates Fleet RPC sends no `rpc` key at all; the
        // server must read that as "no exec capability" and refuse with 412
        // rather than pushing a message into its unknown-tag debug branch.
        let caps: crate::models::AgentCaps =
            serde_json::from_str(r#"{"hw_encoders":[],"codecs":[],"has_input_permission":false,"supports_clipboard":false,"supports_file_transfer":false,"max_simultaneous_sessions":1}"#)
                .unwrap();
        assert!(caps.rpc.is_empty());

        // And an empty list must not be serialised back onto the wire.
        let s = serde_json::to_string(&caps).unwrap();
        assert!(!s.contains("rpc"), "empty rpc caps must be omitted: {s}");
    }

    #[test]
    fn overlay_endpoints_and_leave_roundtrip() {
        let e = ClientMsg::OverlayEndpoints {
            candidates: vec!["198.51.100.7:51820".into()],
        };
        let s = serde_json::to_string(&e).unwrap();
        assert!(s.contains(r#""t":"rc:overlay.endpoints""#));
        assert!(matches!(
            serde_json::from_str::<ClientMsg>(&s).unwrap(),
            ClientMsg::OverlayEndpoints { .. }
        ));

        let l = ClientMsg::OverlayLeave {};
        let s = serde_json::to_string(&l).unwrap();
        assert!(s.contains(r#""t":"rc:overlay.leave""#));
        assert!(matches!(
            serde_json::from_str::<ClientMsg>(&s).unwrap(),
            ClientMsg::OverlayLeave {}
        ));
    }

    #[test]
    fn overlay_srflx_trickle_roundtrip() {
        // Phase B — the srflx trickle is its own message tag, distinct from
        // `rc:overlay.endpoints`, so the server routes it into the separate
        // `srflx_endpoints` bucket.
        let e = ClientMsg::OverlaySrflx {
            candidates: vec!["198.51.100.7:41820".into()],
            nat: Some("cone".into()),
            udp_dialer_ok: None,
        };
        let s = serde_json::to_string(&e).unwrap();
        assert!(s.contains(r#""t":"rc:overlay.srflx""#));
        assert!(s.contains(r#""candidates":["198.51.100.7:41820"]"#));
        assert!(s.contains(r#""nat":"cone""#));
        match serde_json::from_str::<ClientMsg>(&s).unwrap() {
            ClientMsg::OverlaySrflx {
                candidates, nat, ..
            } => {
                assert_eq!(candidates, vec!["198.51.100.7:41820".to_string()]);
                assert_eq!(nat.as_deref(), Some("cone"));
            }
            other => panic!("expected OverlaySrflx, got {other:?}"),
        }

        // Phase C back-compat: a pre-Phase-C agent (rc.199) sends only
        // `candidates` (no `nat`) → parses with nat = None; and a None nat is
        // OMITTED on the wire (skip_serializing_if).
        let legacy = r#"{"t":"rc:overlay.srflx","candidates":["1.2.3.4:5"]}"#;
        match serde_json::from_str::<ClientMsg>(legacy).unwrap() {
            ClientMsg::OverlaySrflx {
                candidates, nat, ..
            } => {
                assert_eq!(candidates, vec!["1.2.3.4:5".to_string()]);
                assert_eq!(nat, None);
            }
            other => panic!("expected OverlaySrflx, got {other:?}"),
        }
        let none_nat = ClientMsg::OverlaySrflx {
            candidates: vec!["1.2.3.4:5".into()],
            nat: None,
            udp_dialer_ok: None,
        };
        assert!(
            !serde_json::to_string(&none_nat).unwrap().contains("nat"),
            "a None nat must be omitted on the wire"
        );
    }

    /// C4 stage 1 — the warm-relay request/grant pair's wire tags are
    /// LOCKED (request-driven, so no hello capability flag exists to catch
    /// a tag drift — the tag itself is the contract).
    #[test]
    fn warm_relay_wire_roundtrip() {
        let m = ClientMsg::OverlayWarmRelayRequest {};
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains("\"rc:overlay.warm_relay_request\""), "{s}");
        match serde_json::from_str::<ClientMsg>(&s).unwrap() {
            ClientMsg::OverlayWarmRelayRequest {} => {}
            other => panic!("roundtrip mismatch: {other:?}"),
        }
        let g = ServerMsg::OverlayWarmRelayGrant {
            ice_servers: vec![],
        };
        let s = serde_json::to_string(&g).unwrap();
        assert!(s.contains("\"rc:overlay.warm_relay_grant\""), "{s}");
        match serde_json::from_str::<ServerMsg>(&s).unwrap() {
            ServerMsg::OverlayWarmRelayGrant { ice_servers } => assert!(ice_servers.is_empty()),
            other => panic!("roundtrip mismatch: {other:?}"),
        }
    }

    #[test]
    fn overlay_relay_request_uses_raw_hex_peer_id() {
        let peer = ObjectId::parse_str("507f1f77bcf86cd799439014").unwrap();
        let m = ClientMsg::OverlayRelayRequest {
            peer_node_id: peer,
            current_kind: None,
            reason: None,
            derp_mux_failed: false,
        };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains(r#""t":"rc:overlay.relay_request""#));
        assert!(!s.contains("$oid"));
        // U1 — absent evidence stays OFF the wire entirely, so an old server
        // sees byte-identical requests from a new fresh-establishment client.
        assert!(!s.contains("current_kind"));
        assert!(!s.contains("reason"));
        assert!(!s.contains("derp_mux_failed"));
        match serde_json::from_str::<ClientMsg>(&s).unwrap() {
            ClientMsg::OverlayRelayRequest { peer_node_id, .. } => assert_eq!(peer_node_id, peer),
            other => panic!("expected OverlayRelayRequest, got {other:?}"),
        }
    }

    /// U1 — wire-compat both directions: a LEGACY request (no evidence
    /// fields) decodes with inert defaults, and a populated one round-trips.
    #[test]
    fn overlay_relay_request_evidence_fields_roundtrip_and_default() {
        let legacy =
            r#"{"t":"rc:overlay.relay_request","peer_node_id":"507f1f77bcf86cd799439014"}"#;
        match serde_json::from_str::<ClientMsg>(legacy).unwrap() {
            ClientMsg::OverlayRelayRequest {
                current_kind,
                reason,
                derp_mux_failed,
                ..
            } => {
                assert_eq!(current_kind, None);
                assert_eq!(reason, None);
                assert!(!derp_mux_failed);
            }
            other => panic!("expected OverlayRelayRequest, got {other:?}"),
        }
        let m = ClientMsg::OverlayRelayRequest {
            peer_node_id: ObjectId::parse_str("507f1f77bcf86cd799439014").unwrap(),
            current_kind: Some("turn".into()),
            reason: Some("rekey-unanswered".into()),
            derp_mux_failed: true,
        };
        let s = serde_json::to_string(&m).unwrap();
        match serde_json::from_str::<ClientMsg>(&s).unwrap() {
            ClientMsg::OverlayRelayRequest {
                current_kind,
                reason,
                derp_mux_failed,
                ..
            } => {
                assert_eq!(current_kind.as_deref(), Some("turn"));
                assert_eq!(reason.as_deref(), Some("rekey-unanswered"));
                assert!(derp_mux_failed);
            }
            other => panic!("expected OverlayRelayRequest, got {other:?}"),
        }
    }

    #[test]
    fn overlay_netmap_roundtrip_with_raw_hex_node_id() {
        let node_id = ObjectId::parse_str("507f1f77bcf86cd799439011").unwrap();
        // P3b-3: the agent_id backing an agent node — a DISTINCT ObjectId from
        // node_id (different namespace), so the test can assert both round-trip.
        let agent_id = ObjectId::parse_str("aaaaaaaaaaaaaaaaaaaaaaaa").unwrap();
        let m = ServerMsg::OverlayNetmap {
            self_ip: "100.64.0.3".into(),
            network: OverlayNetworkInfo {
                cidr: "100.64.0.0/10".into(),
                cidrs: vec![],
                mtu: 1280,
                magic_domain: Some("myorg.roomler.net".into()),
                nameservers: vec!["1.1.1.1".into()],
                self_name: None,
                stun_urls: vec!["stun:5.9.157.221:3478".into()],
            },
            peers: vec![NetmapPeer {
                node_id,
                overlay_ip: "100.64.0.4".into(),
                name: "devbox".into(),
                wg_public_key: "cGVlcg==".into(),
                endpoints: vec!["203.0.113.9:51820".into()],
                lan_endpoints: vec!["203.0.113.9:51820".into()],
                srflx_endpoints: vec!["198.51.100.7:41820".into()],
                srflx_nat: Some("cone".into()),
                caps: None,
                udp_dialer_ok: None,
                relay_home: None,
                warm_relay_endpoint: None,
                reachable: true,
                supports_quic: true,
                supports_relay_single: true,
                supports_derp: true,
                supports_forced_derp: true,
                supports_derp_floor: false,
                supports_org_relay: false,
                supports_overlay_echo: false,
                relay_strategy: Some(RelayStrategyWire::SingleRelayDialer),
                routes: vec!["10.0.0.0/24".into()],
                agent_id: Some(agent_id),
                ingress_rules: None,
            }],
            epoch: 7,
        };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains(r#""t":"rc:overlay.netmap""#));
        // P4 — an un-compiled `ingress_rules` must be ABSENT from the wire, not
        // `null`: a pre-P4 node's netmap shape has to stay byte-identical, and
        // `None` (no ACL) must never be confused with `Some([])` (denied).
        assert!(!s.contains("ingress_rules"));
        // Both node_id AND the populated agent_id are bare hex on the wire (no
        // $oid) — the latter locks the `option_oid_hex` serde (a bare
        // `Option<ObjectId>` would leak bson `{"$oid":…}`, the B1 trap).
        assert!(!s.contains("$oid"));

        // …and when rules ARE compiled they round-trip, including the
        // Some([]) = "denied" shape that must survive as distinct from None.
        let mut m2 = m.clone();
        if let ServerMsg::OverlayNetmap { peers, .. } = &mut m2 {
            peers[0].ingress_rules = Some(vec![crate::models::OverlayRule {
                cidr: "10.66.0.0/16".into(),
                port_range: crate::models::PortRange { low: 22, high: 22 },
                proto: crate::models::ProtocolKind::Tcp,
            }]);
        }
        let s2 = serde_json::to_string(&m2).unwrap();
        assert!(s2.contains(r#""cidr":"10.66.0.0/16""#));
        assert!(s2.contains(r#""proto":"tcp""#));
        let back: ServerMsg = serde_json::from_str(&s2).unwrap();
        let ServerMsg::OverlayNetmap { peers, .. } = &back else {
            panic!("wrong variant");
        };
        let rules = peers[0].ingress_rules.as_ref().unwrap();
        assert_eq!(rules[0].port_range.high, 22);

        let mut m3 = m.clone();
        if let ServerMsg::OverlayNetmap { peers, .. } = &mut m3 {
            peers[0].ingress_rules = Some(Vec::new());
        }
        let back3: ServerMsg = serde_json::from_str(&serde_json::to_string(&m3).unwrap()).unwrap();
        let ServerMsg::OverlayNetmap { peers, .. } = &back3 else {
            panic!("wrong variant");
        };
        assert_eq!(
            peers[0].ingress_rules.as_deref(),
            Some(&[][..]),
            "Some([]) must survive the wire as DENY, never decay to None"
        );
        assert!(s.contains("\"507f1f77bcf86cd799439011\""));
        assert!(s.contains("\"aaaaaaaaaaaaaaaaaaaaaaaa\""));
        match serde_json::from_str::<ServerMsg>(&s).unwrap() {
            ServerMsg::OverlayNetmap {
                self_ip,
                peers,
                epoch,
                ..
            } => {
                assert_eq!(self_ip, "100.64.0.3");
                assert_eq!(epoch, 7);
                assert_eq!(peers.len(), 1);
                assert_eq!(peers[0].node_id, node_id);
                assert_eq!(peers[0].agent_id, Some(agent_id));
                // U2 — the server verdict + echoed forced-DERP support
                // round-trip.
                assert_eq!(
                    peers[0].relay_strategy,
                    Some(RelayStrategyWire::SingleRelayDialer)
                );
                assert!(peers[0].supports_forced_derp);
            }
            other => panic!("expected OverlayNetmap, got {other:?}"),
        }
        // U2 — the verdict serialises as a kebab string tag.
        assert!(s.contains(r#""relay_strategy":"single-relay-dialer""#));
    }

    /// U2 — wire compat both directions: a pre-U2 peer omits both
    /// `relay_strategy` and `supports_forced_derp` (⇒ `None`/`false`, the
    /// client-authoritative path), and every verdict variant round-trips
    /// through its kebab tag.
    #[test]
    fn relay_strategy_wire_defaults_and_roundtrips() {
        let legacy = r#"{
            "node_id":"507f1f77bcf86cd799439011",
            "overlay_ip":"100.64.0.4",
            "wg_public_key":"cGVlcg==",
            "reachable":true
        }"#;
        let p: NetmapPeer = serde_json::from_str(legacy).unwrap();
        assert_eq!(p.relay_strategy, None);
        assert!(!p.supports_forced_derp);
        // An absent verdict must stay OFF the wire (pre-U2 shape unchanged).
        assert!(
            !serde_json::to_string(&p)
                .unwrap()
                .contains("relay_strategy")
        );

        for (v, tag) in [
            (RelayStrategyWire::SingleRelayAnchor, "single-relay-anchor"),
            (RelayStrategyWire::SingleRelayDialer, "single-relay-dialer"),
            (RelayStrategyWire::Derp, "derp"),
            (RelayStrategyWire::BothAllocate, "both-allocate"),
        ] {
            let j = serde_json::to_string(&v).unwrap();
            assert_eq!(j, format!("\"{tag}\""), "{v:?} tag");
            assert_eq!(serde_json::from_str::<RelayStrategyWire>(&j).unwrap(), v);
        }
    }

    /// Back-compat: a netmap peer from a pre-P3b-3 server carries no `agent_id`
    /// field → it deserialises to `None` (no Tunnel labelling), unchanged.
    #[test]
    fn netmap_peer_without_agent_id_defaults_to_none() {
        let json = r#"{
            "node_id":"507f1f77bcf86cd799439011",
            "overlay_ip":"100.64.0.4",
            "name":"devbox",
            "wg_public_key":"cGVlcg==",
            "reachable":true
        }"#;
        let p: NetmapPeer = serde_json::from_str(json).unwrap();
        assert!(p.agent_id.is_none());
    }

    /// FR-19 wire locks: the three relay-session tags, raw-hex ObjectIds, the
    /// skipped-when-absent `rtt_ms`, and the `org-relay` verdict string. A
    /// rename here does not fail loudly — it makes every deployed device look
    /// like it lacks the feature.
    #[test]
    fn org_relay_messages_wire_roundtrip_and_tags_are_locked() {
        let oid = ObjectId::parse_str("507f1f77bcf86cd799439011").unwrap();
        let m = ServerMsg::OverlayRelaySession {
            vni: 0x0042_4242,
            generation: 3,
            peer_node_id: oid,
            relay_node_id: oid,
            relay_endpoints: vec!["62.210.194.66:3478".into()],
            bind_secret: "AAAA".into(),
            bind_secs: 30,
            max_lifetime_secs: 3600,
        };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains(r#""t":"rc:overlay.relay_session""#), "{s}");
        assert!(
            s.contains(r#""peer_node_id":"507f1f77bcf86cd799439011""#),
            "ObjectIds must be raw hex, not $oid: {s}"
        );
        let ServerMsg::OverlayRelaySession {
            vni, generation, ..
        } = serde_json::from_str(&s).unwrap()
        else {
            panic!("relay_session must round-trip");
        };
        assert_eq!((vni, generation), (0x0042_4242, 3));

        let m = ServerMsg::OverlayRelayServe {
            vni: 7,
            generation: 1,
            members: vec![
                RelayMemberWire {
                    wg_public_key: "a".into(),
                    bind_secret: "b".into(),
                },
                RelayMemberWire {
                    wg_public_key: "c".into(),
                    bind_secret: "d".into(),
                },
            ],
            bind_secs: 30,
            idle_secs: 300,
            max_lifetime_secs: 3600,
        };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains(r#""t":"rc:overlay.relay_serve""#), "{s}");
        let ServerMsg::OverlayRelayServe { members, .. } = serde_json::from_str(&s).unwrap() else {
            panic!("relay_serve must round-trip");
        };
        assert_eq!(members.len(), 2);
        assert_eq!(members[1].bind_secret, "d");

        let s = serde_json::to_string(&ServerMsg::OverlayRelayRevoke { vni: 7 }).unwrap();
        assert!(s.contains(r#""t":"rc:overlay.relay_revoke""#), "{s}");

        let m = ClientMsg::OverlayRelayProbe {
            relay_node_id: oid,
            endpoint: "62.210.194.66:3478".into(),
            reachable: true,
            rtt_ms: None,
        };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains(r#""t":"rc:overlay.relay_probe""#), "{s}");
        assert!(!s.contains("rtt_ms"), "an absent rtt must be skipped: {s}");

        assert_eq!(
            serde_json::to_string(&RelayStrategyWire::OrgRelay).unwrap(),
            r#""org-relay""#
        );
    }

    /// The join flag and its netmap echo both default to `false`, and the echo
    /// is SKIPPED when false, so a pre-FR-19 node's netmap shape is unchanged.
    #[test]
    fn supports_org_relay_defaults_false_and_is_skipped_in_the_netmap() {
        let json = r#"{"node_id":"507f1f77bcf86cd799439011","overlay_ip":"100.64.0.4",
                       "name":"d","wg_public_key":"cGVlcg==","reachable":true}"#;
        let p: NetmapPeer = serde_json::from_str(json).unwrap();
        assert!(!p.supports_org_relay);
        assert!(
            !serde_json::to_string(&p)
                .unwrap()
                .contains("supports_org_relay"),
            "false must be skipped, not serialised"
        );
    }

    /// The regression this shim exists for, at the level where it actually
    /// bites: a NEWER server stamps a `relay_strategy` tag this build has
    /// never heard of, and an OLDER agent must still parse the **whole
    /// netmap** and install its peers.
    ///
    /// Without `relay_strategy_lenient` the unknown tag is a hard serde error
    /// on the enclosing `ServerMsg` — the frame is lost, not the field — and
    /// the agent's parse arm swallows it at `debug!`. That is a fleet-wide,
    /// silent peer-install outage triggered by a server-side deploy, which is
    /// why this asserts the surviving peer's other fields too: "it parsed" is
    /// the claim, not merely "the enum was None".
    #[test]
    fn unknown_relay_strategy_tag_keeps_the_whole_netmap_parseable() {
        let json = r#"{
            "t":"rc:overlay.netmap",
            "self_ip":"100.64.0.2",
            "network":{"cidr":"100.64.0.0/10","mtu":1280},
            "epoch":7,
            "peers":[{
                "node_id":"507f1f77bcf86cd799439011",
                "overlay_ip":"100.64.0.4",
                "name":"devbox",
                "wg_public_key":"cGVlcg==",
                "reachable":true,
                "relay_strategy":"some-future-relay-kind"
            }]
        }"#;
        let msg: ServerMsg =
            serde_json::from_str(json).expect("unknown tag must not fail the frame");
        let ServerMsg::OverlayNetmap { peers, epoch, .. } = msg else {
            panic!("expected an OverlayNetmap");
        };
        assert_eq!(epoch, 7);
        assert_eq!(peers.len(), 1, "the peer must survive, not be dropped");
        assert_eq!(peers[0].name, "devbox");
        assert_eq!(peers[0].overlay_ip, "100.64.0.4");
        assert!(
            peers[0].relay_strategy.is_none(),
            "unknown ⇒ None ⇒ the client derives the tier locally, exactly as for an absent field"
        );
    }

    /// Leniency must not swallow the values that DO exist — a shim that
    /// mapped everything to `None` would pass the test above while silently
    /// disabling the server verdict fleet-wide.
    #[test]
    fn every_known_relay_strategy_tag_still_decodes() {
        for (tag, want) in [
            ("single-relay-anchor", RelayStrategyWire::SingleRelayAnchor),
            ("single-relay-dialer", RelayStrategyWire::SingleRelayDialer),
            ("derp", RelayStrategyWire::Derp),
            ("both-allocate", RelayStrategyWire::BothAllocate),
            ("org-relay", RelayStrategyWire::OrgRelay),
        ] {
            let json = format!(
                r#"{{"node_id":"507f1f77bcf86cd799439011","overlay_ip":"100.64.0.4",
                     "name":"d","wg_public_key":"cGVlcg==","reachable":true,
                     "relay_strategy":"{tag}"}}"#
            );
            let p: NetmapPeer = serde_json::from_str(&json).unwrap();
            assert_eq!(p.relay_strategy, Some(want), "tag {tag} must still decode");
            // …and still serialise back to the same tag.
            let s = serde_json::to_string(&p).unwrap();
            assert!(s.contains(&format!(r#""relay_strategy":"{tag}""#)), "{s}");
        }
    }

    /// Absent, `null`, and a non-string shape all mean "no verdict". The last
    /// case matters because a future variant that grows a payload would arrive
    /// as a map, and an older agent must degrade rather than lose the frame.
    #[test]
    fn relay_strategy_absent_null_and_non_string_all_decode_to_none() {
        let base = r#""node_id":"507f1f77bcf86cd799439011","overlay_ip":"100.64.0.4",
                      "name":"d","wg_public_key":"cGVlcg==","reachable":true"#;
        for tail in [
            String::new(),
            r#","relay_strategy":null"#.to_string(),
            r#","relay_strategy":{"org-relay":{"vni":7}}"#.to_string(),
            r#","relay_strategy":42"#.to_string(),
        ] {
            let p: NetmapPeer = serde_json::from_str(&format!("{{{base}{tail}}}")).unwrap();
            assert!(
                p.relay_strategy.is_none(),
                "tail {tail} must decode to None"
            );
        }
    }

    /// Back-compat both directions for the Phase-A `lan_endpoints` field: a
    /// pre-Phase-A server omits it → defaults empty (public-direct tier inert);
    /// and an empty vec is OMITTED on the wire (`skip_serializing_if`), so a
    /// Phase-A node talking to any consumer serialises byte-identically to
    /// before unless the bucket is populated.
    #[test]
    fn netmap_peer_lan_endpoints_default_and_skip() {
        let json = r#"{
            "node_id":"507f1f77bcf86cd799439011",
            "overlay_ip":"100.64.0.4",
            "wg_public_key":"cGVlcg==",
            "reachable":true
        }"#;
        let p: NetmapPeer = serde_json::from_str(json).unwrap();
        assert!(p.lan_endpoints.is_empty());
        let s = serde_json::to_string(&p).unwrap();
        assert!(
            !s.contains("lan_endpoints"),
            "empty lan_endpoints must be omitted on the wire: {s}"
        );
        // Populated → serialised and round-trips.
        let mut p2 = p.clone();
        p2.lan_endpoints = vec!["5.9.157.226:41234".into()];
        let s2 = serde_json::to_string(&p2).unwrap();
        assert!(s2.contains(r#""lan_endpoints":["5.9.157.226:41234"]"#));
        assert_eq!(serde_json::from_str::<NetmapPeer>(&s2).unwrap(), p2);
    }

    /// Back-compat both directions for the Phase-B `srflx_endpoints` field: a
    /// pre-Phase-B server omits it → defaults empty (srflx tier inert); an empty
    /// vec is OMITTED on the wire (`skip_serializing_if`); populated round-trips.
    #[test]
    fn netmap_peer_srflx_endpoints_default_and_skip() {
        let json = r#"{
            "node_id":"507f1f77bcf86cd799439011",
            "overlay_ip":"100.64.0.4",
            "wg_public_key":"cGVlcg==",
            "reachable":true
        }"#;
        let p: NetmapPeer = serde_json::from_str(json).unwrap();
        assert!(p.srflx_endpoints.is_empty());
        let s = serde_json::to_string(&p).unwrap();
        assert!(
            !s.contains("srflx_endpoints"),
            "empty srflx_endpoints must be omitted on the wire: {s}"
        );
        let mut p2 = p.clone();
        p2.srflx_endpoints = vec!["198.51.100.7:41820".into()];
        let s2 = serde_json::to_string(&p2).unwrap();
        assert!(s2.contains(r#""srflx_endpoints":["198.51.100.7:41820"]"#));
        assert_eq!(serde_json::from_str::<NetmapPeer>(&s2).unwrap(), p2);
    }

    /// Dialer honesty — `udp_dialer_ok` back-compat, BOTH directions: a
    /// pre-honesty server/peer omits it → `None` (legacy role inputs on both
    /// ends); `None` is OMITTED on the wire (an old shape stays byte-stable);
    /// `Some(_)` round-trips — its PRESENCE is the capability signal.
    #[test]
    fn netmap_peer_and_srflx_msg_udp_dialer_ok_default_skip_and_roundtrip() {
        let json = r#"{
            "node_id":"507f1f77bcf86cd799439011",
            "overlay_ip":"100.64.0.4",
            "wg_public_key":"cGVlcg==",
            "reachable":true
        }"#;
        let p: NetmapPeer = serde_json::from_str(json).unwrap();
        assert_eq!(p.udp_dialer_ok, None);
        let s = serde_json::to_string(&p).unwrap();
        assert!(
            !s.contains("udp_dialer_ok"),
            "absent verdict must be omitted on the wire: {s}"
        );
        let mut p2 = p.clone();
        p2.udp_dialer_ok = Some(false);
        let s2 = serde_json::to_string(&p2).unwrap();
        assert!(s2.contains(r#""udp_dialer_ok":false"#));
        assert_eq!(serde_json::from_str::<NetmapPeer>(&s2).unwrap(), p2);

        // The srflx trickle: an OLD agent's message (no field) parses to
        // `None`; a NEW agent's `Some(_)` round-trips.
        let old = r#"{"t":"rc:overlay.srflx","candidates":["1.2.3.4:5"],"nat":"cone"}"#;
        match serde_json::from_str::<ClientMsg>(old).unwrap() {
            ClientMsg::OverlaySrflx { udp_dialer_ok, .. } => assert_eq!(udp_dialer_ok, None),
            other => panic!("wrong variant: {other:?}"),
        }
        let new = ClientMsg::OverlaySrflx {
            candidates: vec!["1.2.3.4:5".into()],
            nat: Some("symmetric".into()),
            udp_dialer_ok: Some(false),
        };
        let wire = serde_json::to_string(&new).unwrap();
        assert!(wire.contains(r#""udp_dialer_ok":false"#));
        match serde_json::from_str::<ClientMsg>(&wire).unwrap() {
            ClientMsg::OverlaySrflx { udp_dialer_ok, .. } => {
                assert_eq!(udp_dialer_ok, Some(false));
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    /// Phase C — `srflx_nat` back-compat: a pre-Phase-C server/peer omits it →
    /// defaults `None` (unknown ⇒ punch attempted, never skipped); `None` is
    /// OMITTED on the wire; populated round-trips.
    #[test]
    fn netcheck_msg_and_netmap_caps_default_skip_and_roundtrip() {
        // Wire message: a populated vector round-trips; the tag is stable.
        let m = ClientMsg::OverlayNetcheck {
            caps: CapVectorWire {
                stun_udp: true,
                relay_band_udp: Some(false),
                derp_ws_ok: true,
            },
        };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains(r#""t":"rc:overlay.netcheck""#));
        assert!(s.contains(r#""relay_band_udp":false"#));
        match serde_json::from_str::<ClientMsg>(&s).unwrap() {
            ClientMsg::OverlayNetcheck { caps } => {
                assert!(caps.stun_udp && caps.derp_ws_ok);
                assert_eq!(caps.relay_band_udp, Some(false));
            }
            other => panic!("wrong variant: {other:?}"),
        }
        // Unmeasured relay_band is OMITTED on the wire; absent parses None.
        let partial = serde_json::to_string(&CapVectorWire {
            stun_udp: true,
            relay_band_udp: None,
            derp_ws_ok: false,
        })
        .unwrap();
        assert!(!partial.contains("relay_band_udp"));

        // NetmapPeer.caps: absent from a pre-B server ⇒ None + omitted on
        // the wire; populated round-trips.
        let json = r#"{
            "node_id":"507f1f77bcf86cd799439011",
            "overlay_ip":"100.64.0.4",
            "wg_public_key":"cGVlcg==",
            "reachable":true
        }"#;
        let p: NetmapPeer = serde_json::from_str(json).unwrap();
        assert!(p.caps.is_none());
        let s = serde_json::to_string(&p).unwrap();
        assert!(!s.contains("\"caps\""), "absent caps omitted: {s}");
        let mut p2 = p.clone();
        p2.caps = Some(CapVectorWire {
            stun_udp: false,
            relay_band_udp: Some(true),
            derp_ws_ok: true,
        });
        let s2 = serde_json::to_string(&p2).unwrap();
        assert_eq!(serde_json::from_str::<NetmapPeer>(&s2).unwrap(), p2);
    }

    #[test]
    fn netmap_peer_supports_derp_floor_default_and_roundtrip() {
        // Phase A back-compat: absent from a pre-floor server ⇒ false (the
        // lazy-mux rules apply); a set flag round-trips.
        let json = r#"{
            "node_id":"507f1f77bcf86cd799439011",
            "overlay_ip":"100.64.0.4",
            "wg_public_key":"cGVlcg==",
            "reachable":true
        }"#;
        let p: NetmapPeer = serde_json::from_str(json).unwrap();
        assert!(!p.supports_derp_floor, "pre-floor peer must default false");
        let mut p2 = p.clone();
        p2.supports_derp_floor = true;
        let s = serde_json::to_string(&p2).unwrap();
        assert_eq!(serde_json::from_str::<NetmapPeer>(&s).unwrap(), p2);
    }

    #[test]
    fn netmap_peer_srflx_nat_default_and_skip() {
        let json = r#"{
            "node_id":"507f1f77bcf86cd799439011",
            "overlay_ip":"100.64.0.4",
            "wg_public_key":"cGVlcg==",
            "reachable":true
        }"#;
        let p: NetmapPeer = serde_json::from_str(json).unwrap();
        assert_eq!(p.srflx_nat, None);
        let s = serde_json::to_string(&p).unwrap();
        assert!(
            !s.contains("srflx_nat"),
            "a None srflx_nat must be omitted on the wire: {s}"
        );
        let mut p2 = p.clone();
        p2.srflx_nat = Some("symmetric".into());
        let s2 = serde_json::to_string(&p2).unwrap();
        assert!(s2.contains(r#""srflx_nat":"symmetric""#));
        assert_eq!(serde_json::from_str::<NetmapPeer>(&s2).unwrap(), p2);
    }

    /// Phase B — `stun_urls` back-compat: a pre-Phase-B server omits it →
    /// defaults empty (no srflx gathering); empty is skipped on the wire;
    /// populated round-trips byte-for-byte.
    #[test]
    fn overlay_network_stun_urls_default_and_skip() {
        let json = r#"{"cidr":"100.64.0.0/10","mtu":1280}"#;
        let n: OverlayNetworkInfo = serde_json::from_str(json).unwrap();
        assert!(n.stun_urls.is_empty());
        let s = serde_json::to_string(&n).unwrap();
        assert!(
            !s.contains("stun_urls"),
            "empty stun_urls must be omitted: {s}"
        );
        let mut n2 = n.clone();
        n2.stun_urls = vec!["stun:5.9.157.221:3478".into()];
        let s2 = serde_json::to_string(&n2).unwrap();
        assert!(s2.contains(r#""stun_urls":["stun:5.9.157.221:3478"]"#));
        assert_eq!(serde_json::from_str::<OverlayNetworkInfo>(&s2).unwrap(), n2);
    }

    #[test]
    fn overlay_netmap_delta_removes_are_raw_hex() {
        let rm = ObjectId::parse_str("507f1f77bcf86cd799439012").unwrap();
        let m = ServerMsg::OverlayNetmapDelta {
            epoch: 8,
            upserts: vec![],
            removes: vec![rm],
        };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains(r#""t":"rc:overlay.netmap_delta""#));
        assert!(!s.contains("$oid"));
        assert!(s.contains("\"507f1f77bcf86cd799439012\""));
        match serde_json::from_str::<ServerMsg>(&s).unwrap() {
            ServerMsg::OverlayNetmapDelta { removes, epoch, .. } => {
                assert_eq!(epoch, 8);
                assert_eq!(removes, vec![rm]);
            }
            other => panic!("expected OverlayNetmapDelta, got {other:?}"),
        }
    }

    #[test]
    fn overlay_relay_grant_roundtrip() {
        let peer = ObjectId::parse_str("507f1f77bcf86cd799439013").unwrap();
        let m = ServerMsg::OverlayRelayGrant {
            ice_servers: vec![IceServer {
                urls: vec!["turns:coturn.roomler.ai:443?transport=tcp".into()],
                username: Some("u".into()),
                credential: Some("c".into()),
            }],
            peer_node_id: peer,
            pair_key: "a..b".into(),
        };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains(r#""t":"rc:overlay.relay_grant""#));
        match serde_json::from_str::<ServerMsg>(&s).unwrap() {
            ServerMsg::OverlayRelayGrant {
                peer_node_id,
                ice_servers,
                ..
            } => {
                assert_eq!(peer_node_id, peer);
                assert_eq!(ice_servers.len(), 1);
            }
            other => panic!("expected OverlayRelayGrant, got {other:?}"),
        }
    }
    /// FR-43 P2c — an absent `caps` must not appear on the wire at ALL.
    ///
    /// The whole design rests on caps riding the heartbeat only when they
    /// CHANGE: they are ~200 bytes against a frequent message, so a steady
    /// state that serialised `"caps":null` would cost every device every beat
    /// for nothing. And a reader must be able to tell "no news" from "no
    /// capabilities" — the same `None` vs `Some([])` rule `AgentCaps.permissions`
    /// already carries.
    #[test]
    fn heartbeat_caps_are_absent_from_the_wire_unless_they_changed() {
        let quiet = ClientMsg::AgentHeartbeat {
            rss_mb: 10,
            cpu_pct: 1.0,
            active_sessions: 0,
            sys: None,
            srflx_count: None,
            warm_relay: None,
            companion_version: None,
            caps: None,
        };
        let s = serde_json::to_string(&quiet).unwrap();
        assert!(
            !s.contains("caps"),
            "a steady-state heartbeat must not mention caps at all: {s}"
        );

        // …and when they DO change, they travel whole.
        let announcing = ClientMsg::AgentHeartbeat {
            rss_mb: 10,
            cpu_pct: 1.0,
            active_sessions: 0,
            sys: None,
            srflx_count: None,
            warm_relay: None,
            companion_version: None,
            caps: Some(Box::new(AgentCaps {
                permissions: Some(vec!["screen-capture".into(), "input".into()]),
                has_input_permission: true,
                ..Default::default()
            })),
        };
        let s = serde_json::to_string(&announcing).unwrap();
        assert!(s.contains(r#""screen-capture""#), "got {s}");
        let back: ClientMsg = serde_json::from_str(&s).unwrap();
        match back {
            ClientMsg::AgentHeartbeat { caps: Some(c), .. } => {
                assert_eq!(
                    c.permissions.as_deref(),
                    Some(&["screen-capture".to_string(), "input".to_string()][..]),
                    "the permissions a device announces must survive the round trip"
                );
                assert!(c.has_input_permission);
            }
            other => panic!("expected caps to survive, got {other:?}"),
        }
    }
}
