use bson::{DateTime, oid::ObjectId};
use serde::{Deserialize, Serialize};

use crate::permissions::Permissions;

// ────────────────────────────────────────────────────────────────────────────
// Agent
// ────────────────────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OsKind {
    Linux,
    Macos,
    Windows,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Online,
    Offline,
    Unenrolled,
    Quarantined,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct DisplayInfo {
    pub index: u8,
    pub name: String,
    pub width_px: u32,
    pub height_px: u32,
    pub scale: f32,
    pub primary: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct AgentCaps {
    pub hw_encoders: Vec<String>,
    pub codecs: Vec<String>,
    pub has_input_permission: bool,
    pub supports_clipboard: bool,
    pub supports_file_transfer: bool,
    pub max_simultaneous_sessions: u8,
    /// Video transport modes the agent supports beyond the default
    /// WebRTC video track. Empty / unset means WebRTC video only
    /// (the legacy default; older agents that don't know about
    /// this field deserialize that way via serde default).
    ///
    /// Known value: `data-channel-vp9-444` — VP9 profile 1
    /// (8-bit 4:4:4) frames over an RTCDataChannel named
    /// `video-bytes`. Bypasses the browser's WebRTC video pipeline
    /// which enforces 4:2:0 across every codec. See
    /// `docs/vp9-444-plan.md` for the rationale and the wire
    /// format spec.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub transports: Vec<String>,
    /// File-DC v2 (0.3.0+) capability list. Replaces the
    /// coarse-grained `supports_file_transfer` bool with explicit
    /// per-feature flags. Recognised values:
    ///
    /// * `upload`   — browser → host file uploads (the v1 default).
    /// * `download` — host → browser single-file downloads.
    /// * `download-folder` — host → browser folder zip streams.
    /// * `browse`   — browser can navigate the host's filesystem
    ///   via `files:dir`. Conditional on the agent's
    ///   `enable_remote_browse` config flag.
    ///
    /// Empty / unset (older agents) deserialises to `[]`; browsers
    /// that see an empty list fall back to `supports_file_transfer`
    /// to determine just upload availability. New browsers that need
    /// download/browse functionality check this list and grey out
    /// the affected toolbar buttons when the capability is missing,
    /// instead of waiting for a 5 s timeout on an unanswered
    /// `files:get` / `files:dir`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
    /// P6 — multi-user input capabilities. Recognised values:
    ///
    /// * `arbiter` — the agent runs the InputArbiter (single fenced
    ///   injection worker); the SERVER lifts the P3 single-INPUT-holder
    ///   downgrade for concurrent sessions on agents advertising this.
    /// * `exclusive` — the arbiter supports the exclusive floor mode
    ///   (`rc:control.request` / `rc:control.mode` on the control DC).
    /// * `ghost-cursor` — other sessions' pointers are rebroadcast as
    ///   `cursor:peer` on the cursor DC.
    ///
    /// Empty / unset = a pre-P6 agent → the P3 single-holder rule stays.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub input: Vec<String>,
    /// Multi-org capabilities. Recognised values:
    ///
    /// * `join` — the agent honours `rc:agent.join_org`: it enrolls itself
    ///   into an ADDITIONAL org on the same server from a pushed enrollment
    ///   token and brings that org's supervisor up without a restart.
    ///
    /// Empty / unset = a pre-rc.310 agent. The admin UI greys out
    /// "Add to another organization" for those; the server refuses to mint a
    /// token it knows can't be consumed, rather than pushing a message that
    /// lands in the agent's unknown-variant debug branch and vanishes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub multi_org: Vec<String>,
    /// VP9 chroma format the agent will emit on the
    /// `data-channel-vp9-444` transport. Values: `"yuv444"` (default,
    /// current behaviour, VP9 profile 1) for sharpest text via
    /// ClearType chroma preservation, or `"yuv420"` (VP9 profile 0)
    /// for ~1.5× lower bandwidth at the cost of slight chroma loss
    /// on small Windows ClearType text.
    ///
    /// rc.61 — added so the browser-side `rc-vp9-444-worker.ts` can
    /// pick the right codec string (`vp09.01.10.08` vs `vp09.00.10.08`)
    /// when configuring its `VideoDecoder`. Mismatch leaves the canvas
    /// blank. Empty / older agents deserialise to `""`; browsers treat
    /// the empty value as `"yuv444"` for backward compat.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub vp9_chroma: String,
    /// Audio codecs the agent can stream on the WebRTC audio track
    /// (system / desktop audio, opt-in per session). Empty / unset
    /// (older agents, or agents built without the `audio` feature)
    /// means "no audio track offered" — the browser must not request
    /// `audio_enabled`. Known value: `"opus"` (audio/opus, 48 kHz
    /// stereo, PT 111 — the WebRTC default). Populated by the agent's
    /// caps builder only when the `audio` Cargo feature is compiled in.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub audio: Vec<String>,
    /// Remote app selection & launch on virtual-desktop hosts. Present +
    /// non-empty only when the agent can manage a desktop (Linux
    /// virtual-desktop mode) AND `virtual_desktop_apps.enabled`. Known
    /// values: `"list"`, `"focus"`, `"launch"`. Empty / unset (older
    /// agents, non-VD hosts) → the browser hides the Apps menu.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub apps: Vec<String>,
    /// Fleet-RPC capabilities. Recognised values:
    ///
    /// * `exec` — the agent honours `rc:rpc.exec` / `rc:rpc.cancel`: it runs
    ///   one bounded shell command and replies `rc:rpc.result`.
    /// * `originate` — the agent honours `rc:rpc.response`, i.e. its LocalAPI
    ///   can originate an exec against ANOTHER device (the `roomler exec`
    ///   CLI path).
    ///
    /// Empty / unset = a pre-fleet-RPC agent. Same rule as [`Self::multi_org`]:
    /// the server refuses the request with `412` rather than pushing a message
    /// that lands in the agent's unknown-variant debug branch and vanishes,
    /// leaving the caller to hang until its deadline.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rpc: Vec<String>,
    /// Clipboard-DC protocol-v2 capability list. Extends the coarse
    /// `supports_clipboard` bool (kept for older browsers) with
    /// per-feature flags. Recognised values:
    ///
    /// * `"ack"`    — the agent replies `clipboard:write-ack` to
    ///   `clipboard:write` / `clipboard:write-chunk` payloads that
    ///   carry an `id`, letting the browser gate its deferred Ctrl+V
    ///   keystroke on the host clipboard actually being written.
    /// * `"events"` — the agent accepts `clipboard:subscribe` and
    ///   pushes unsolicited `clipboard:event` / `clipboard:img-begin`
    ///   messages when the host clipboard changes (host→browser
    ///   auto-sync). Change *watching* for images is Windows-only;
    ///   text watching works everywhere arboard does.
    /// * `"images"` — the agent understands PNG image payloads on the
    ///   clipboard DC in both directions (`clipboard:img-begin` /
    ///   binary frames / `clipboard:img-end`, and `accept:["image"]`
    ///   on `clipboard:read`).
    ///
    /// Empty / unset (older agents, builds without the `clipboard`
    /// feature) deserialises to `[]`; browsers then fall back to the
    /// v1 button-driven text-only flow.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub clipboard: Vec<String>,
    /// Keyboard-layout integration (Windows hosts with input
    /// injection). Recognised values:
    ///
    /// * `"report"` — the agent pushes `rc:layout` snapshots (active +
    ///   installed layouts) over the control DC.
    /// * `"set"` — the agent accepts `rc:layout.set {hkl}` manual
    ///   switches from the viewer's layout picker.
    ///
    /// Empty / unset (older agents, non-Windows hosts, builds without
    /// `enigo-input`) → the browser hides all layout UI. The per-char
    /// auto-switch itself is agent-local and needs no browser
    /// involvement.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub layout: Vec<String>,
}

/// How consent is obtained before a controller may drive a device. Resolved
/// server-side per session from the device's [`AccessPolicy::consent_mode`]
/// (with `Prompt` — attended — as the system default), then carried to the agent
/// in `ServerMsg::Request` as a directive the agent obeys. Self-control
/// (`controller == owner_user_id`) short-circuits to `Auto` in the API gate.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ConsentMode {
    /// Unattended: grant immediately, no prompt. For self-owned / kiosk / server
    /// devices explicitly marked unattended.
    Auto,
    /// Attended (the default): the controlled host prompts (tray / CLI) and the
    /// person there must approve within the timeout.
    #[default]
    Prompt,
    /// Email an approve-link to the device owner; the session waits (Phase 4).
    Email,
    /// Push an in-app consent card to the device owner (Phase 4).
    Push,
    /// Prompt the host first; fall back to email if nobody answers (Phase 4).
    PromptThenEmail,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct AccessPolicy {
    /// How consent is obtained for a non-owner controller. `None` = inherit the
    /// system default ([`ConsentMode::Prompt`] — attended). Set per device by a
    /// `MANAGE_AGENTS` admin. (Replaces the legacy `require_consent` bool; old
    /// rows carrying that field deserialize to `None` → attended, the safe
    /// default.)
    #[serde(default)]
    pub consent_mode: Option<ConsentMode>,
    #[serde(default)]
    pub allowed_role_ids: Vec<ObjectId>,
    #[serde(default)]
    pub allowed_user_ids: Vec<ObjectId>,
    pub auto_terminate_idle_minutes: Option<u32>,
    /// P6 — multi-user input arbitration mode. `None` = the system default
    /// ([`InputMode::Free`] — free-for-all with agent-side fencing). Old rows
    /// deserialize to `None` via the default.
    #[serde(default)]
    pub input_mode: Option<InputMode>,
}

impl AccessPolicy {
    /// Effective consent mode for a NON-owner controller: the per-device mode,
    /// or the system default (`Prompt` = attended) when unset. Self-control is
    /// resolved to `Auto` by the caller before this is consulted.
    pub fn effective_consent_mode(&self) -> ConsentMode {
        self.consent_mode.unwrap_or(ConsentMode::Prompt)
    }
}

/// P6 — multi-user input arbitration mode for a device. `Free` (the default)
/// lets every INPUT-granted session inject, serialized + modifier-fenced by
/// the agent's InputArbiter; `Exclusive` funnels input through one explicit
/// floor holder (request/grant, idle takeover). Set per device via
/// `AccessPolicy.input_mode`; toggleable in-session on the control DC.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum InputMode {
    #[default]
    Free,
    Exclusive,
}

// ────────────────────────────────────────────────────────────────────────────
// Fleet RPC (remote command execution)
// ────────────────────────────────────────────────────────────────────────────

/// Execution bounds. The server clamps an incoming request to these before it
/// reaches the wire; the agent enforces them again on its own side, so a
/// forged or replayed frame can't ask for an unbounded run.
pub mod exec_limits {
    /// Wall-clock budget when the caller doesn't specify one.
    pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;
    /// Hard ceiling. Longer work belongs in a job the command kicks off, not
    /// in a request a caller is blocked on.
    pub const MAX_TIMEOUT_MS: u64 = 300_000;
    /// Combined stdout+stderr ceiling when the caller doesn't specify one.
    pub const DEFAULT_MAX_OUTPUT_BYTES: u64 = 256 * 1024;
    /// Hard ceiling on combined output.
    pub const MAX_OUTPUT_BYTES: u64 = 1024 * 1024;
    /// Concurrent commands one device will run. Beyond this the agent
    /// refuses rather than queues — a caller blocked on a deadline would
    /// rather get a fast "busy" than a slow timeout.
    pub const MAX_CONCURRENT_PER_AGENT: usize = 4;
    /// Requests per minute per (caller, device) pair.
    pub const RATE_LIMIT_PER_MINUTE: u32 = 30;

    /// Clamp a caller-supplied timeout into range, substituting the default
    /// for `None`/0.
    pub fn clamp_timeout_ms(requested: u64) -> u64 {
        match requested {
            0 => DEFAULT_TIMEOUT_MS,
            n => n.min(MAX_TIMEOUT_MS),
        }
    }

    /// Clamp a caller-supplied output ceiling into range, substituting the
    /// default for `None`/0.
    pub fn clamp_output_bytes(requested: u64) -> u64 {
        match requested {
            0 => DEFAULT_MAX_OUTPUT_BYTES,
            n => n.min(MAX_OUTPUT_BYTES),
        }
    }
}

/// Whether a device accepts remote command execution at all.
///
/// Default `Off` — every existing row deserialises to it, so enabling the
/// feature can never retroactively open a device.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ExecMode {
    /// Refuse every exec request. The default.
    #[default]
    Off,
    /// Accept exec from callers that clear every other gate.
    On,
}

/// Per-device remote-execution policy — gate 3 of four (see
/// `docs/fleet-rpc.md`). Deliberately NOT folded into [`AccessPolicy`]:
/// that grants screen-view, and "may watch your screen" must never be the
/// same checkbox as "may run a root shell".
///
/// ⚠️ On a perMachine Windows install the daemon runs as **SYSTEM**, and on a
/// systemd host as **root** — so `ExecMode::On` is root-equivalent by
/// construction. That is required for the diagnostics this exists for
/// (`Get-NetFirewallRule`, `netsh`, route tables, service state) and is stated
/// in the admin UI's opt-in copy rather than buried here.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct ExecPolicy {
    /// Does this device accept exec requests? Gate 3.
    #[serde(default)]
    pub mode: ExecMode,
    /// May this device *originate* exec against other devices (the
    /// `roomler exec` CLI path, where the daemon's own agent WS carries the
    /// request)? Default `false` — without this, compromising any enrolled
    /// laptop would inherit its owner's exec rights across the whole fleet.
    #[serde(default)]
    pub can_originate: bool,
    /// Restrict callers to these users. Empty = no user restriction (the
    /// permission bit + the other gates still apply).
    #[serde(default)]
    pub allowed_user_ids: Vec<ObjectId>,
    /// Restrict callers to holders of these roles. Empty = no role
    /// restriction.
    #[serde(default)]
    pub allowed_role_ids: Vec<ObjectId>,
    /// How consent is obtained before a command runs. `None` = the safe
    /// default ([`ConsentMode::Prompt`]); unattended servers set `Auto`.
    /// Only `Auto` and `Prompt` are honoured — the email/push variants are
    /// session-shaped and resolve to `Prompt`.
    #[serde(default)]
    pub consent_mode: Option<ConsentMode>,
    /// Shells the device will accept, e.g. `["pwsh", "powershell"]`. Empty =
    /// every shell the host supports.
    #[serde(default)]
    pub shells: Vec<String>,
}

impl ExecPolicy {
    /// Effective consent mode. Unset ⇒ [`ConsentMode::Prompt`] (attended);
    /// the session-shaped variants collapse to `Prompt` because there is no
    /// exec equivalent of an approve-link flow.
    pub fn effective_consent_mode(&self) -> ConsentMode {
        match self.consent_mode {
            Some(ConsentMode::Auto) => ConsentMode::Auto,
            _ => ConsentMode::Prompt,
        }
    }

    /// Is `shell` permitted by this policy? An empty list allows everything
    /// the host itself supports.
    pub fn allows_shell(&self, shell: &str) -> bool {
        self.shells.is_empty() || self.shells.iter().any(|s| s.eq_ignore_ascii_case(shell))
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Agent {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub tenant_id: ObjectId,
    /// The user who "owns" this device — consent for a non-self controller can
    /// route to them (email/push), and a controller equal to the owner
    /// self-controls (no external allowlist needed). Set to `enrolled_by` at
    /// enrollment; reassignable by a `MANAGE_AGENTS` admin.
    pub owner_user_id: ObjectId,
    /// The user whose enrollment token created this agent (audit; the initial
    /// `owner_user_id`). `#[serde(default)]` → older rows deserialize to `None`.
    #[serde(default)]
    pub enrolled_by: Option<ObjectId>,
    pub name: String,
    pub machine_id: String,
    pub os: OsKind,
    pub agent_version: String,
    pub agent_token_hash: String,
    pub status: AgentStatus,
    pub last_seen_at: DateTime,
    /// P4 — the presence value (`online`/`stale`/`offline`) most recently
    /// BROADCAST as a `device:presence` event. A cluster-wide transition
    /// ledger: every emit path (register, teardown, staleness sweep, on any
    /// pod) does a CAS on this field and fans out only when it actually
    /// changed, so a transition is announced exactly once no matter how many
    /// pods observe it. `None` on pre-P4 rows / fresh enrollments — the first
    /// observed state always emits. NOT the read-path truth (that stays the
    /// live hub + Redis directory + heartbeat derivation).
    #[serde(default)]
    pub last_presence: Option<String>,
    #[serde(default)]
    pub displays: Vec<DisplayInfo>,
    #[serde(default)]
    pub capabilities: AgentCaps,
    #[serde(default)]
    pub access_policy: AccessPolicy,
    /// Fleet-RPC policy for this device (gate 3). `#[serde(default)]` → every
    /// pre-feature row deserialises to [`ExecMode::Off`].
    #[serde(default)]
    pub exec_policy: ExecPolicy,
    /// Subnet-router CIDRs this agent is a gateway for (Phase 2). The SOCKS
    /// mesh longest-prefix-matches a LAN-IP target against these to pick the
    /// covering agent, which then dials the real IP (still gated by the
    /// tenant's `tunnel_policies`). Admin-configured. `#[serde(default)]` →
    /// older rows deserialize to no routes.
    #[serde(default)]
    pub routes: Vec<String>,
    /// Subnet CIDRs the AGENT itself advertises it can route (from its
    /// `advertise_routes` config, refreshed on each `rc:agent.hello`). These
    /// are untrusted SUGGESTIONS — an admin approves a subset into `routes`
    /// (what the mesh actually consumes). `#[serde(default)]` → older rows /
    /// pre-feature agents deserialize to none.
    #[serde(default)]
    pub advertised_routes: Vec<String>,
    /// Multi-region relay PoPs: the agent's nearest relay region (a
    /// `relay_regions` id), derived server-side from its STUN probe reports
    /// with hysteresis. `None` = never probed / all probes timed out — every
    /// issuance path then uses the default region. `#[serde(default)]` →
    /// older rows deserialize to `None`.
    #[serde(default)]
    pub relay_home: Option<String>,
    /// The agent's last full probe table (observability; the UI shows it on
    /// the device detail). `#[serde(default)]` → older rows deserialize to
    /// `None`.
    #[serde(default)]
    pub relay_rtt: Option<Vec<crate::signaling::RelayRegionRtt>>,
    pub created_at: DateTime,
    pub updated_at: DateTime,
    pub deleted_at: Option<DateTime>,
}

impl Agent {
    pub const COLLECTION: &'static str = "agents";
}

// ────────────────────────────────────────────────────────────────────────────
// Tunnel client (roomler-tunnel)
// ────────────────────────────────────────────────────────────────────────────

/// A laptop running `roomler-tunnel`. Mirrors [`Agent`] structurally
/// (same lifecycle, same `AgentStatus`, same `(tenant_id, machine_id)`
/// uniqueness for rehydrate-on-re-enroll) but slimmer — tunnel clients
/// don't capture screens or hold capability lists. The `_role_` is
/// inverted vs an agent: a tunnel client *initiates* forwards; an
/// agent *serves* them.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TunnelClient {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub tenant_id: ObjectId,
    /// User who installed + runs the CLI on this laptop. Carried into
    /// `TunnelClientClaims.owner_user_id` at enrollment time and
    /// recorded in every `tunnel_audit` row.
    pub owner_user_id: ObjectId,
    pub name: String,
    pub machine_id: String,
    pub os: OsKind,
    pub client_version: String,
    pub status: AgentStatus,
    pub last_seen_at: DateTime,
    pub created_at: DateTime,
    pub updated_at: DateTime,
    pub deleted_at: Option<DateTime>,
}

impl TunnelClient {
    pub const COLLECTION: &'static str = "tunnel_clients";
}

// ────────────────────────────────────────────────────────────────────────────
// Tunnel policy
// ────────────────────────────────────────────────────────────────────────────
//
// Single source of truth for ACL data shapes — DB rows AND the
// evaluator in `tunnel-core::policy` both consume these types. The
// evaluator re-exports them so callers have one import path; this
// keeps the DB schema authoritative without inverting the dep graph
// (`services` already depends on `remote_control` for `Agent` etc.).

/// Matches a destination hostname. Adjacently tagged so JSON wire
/// shape is `{"kind":"exact","value":"db.intranet"}` etc.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum HostPattern {
    /// Literal match — `"db.intranet"`.
    Exact(String),
    /// Glob — `"*.intranet"` matches one or more subdomains.
    Wildcard(String),
    /// CIDR range — `"10.0.0.0/24"`. Resolves against literal IPs only;
    /// hostnames must be resolved by the caller first.
    Cidr(String),
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct PortRange {
    /// Inclusive lower bound.
    pub low: u16,
    /// Inclusive upper bound. Equal to `low` for single-port rules.
    pub high: u16,
}

/// Which L4 protocol a [`DestinationRule`] permits. `Any` (the default,
/// and what pre-UDP stored rules deserialise to) matches both TCP
/// CONNECT forwards and UDP ASSOCIATE forwards; `Tcp` / `Udp` narrow a
/// rule to one. The forward gate evaluates the request's protocol
/// against this via [`ProtocolKind::permits`].
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ProtocolKind {
    Tcp,
    Udp,
    /// Matches any protocol. Default so a rule authored (or stored)
    /// without a `proto` field keeps its pre-UDP behaviour.
    #[default]
    Any,
}

impl ProtocolKind {
    /// Does a rule declaring `self` permit a forward request of
    /// protocol `req`? `Any` permits everything; otherwise the request
    /// must match exactly. `req` is always concrete (`Tcp` / `Udp`) —
    /// a request never carries `Any`.
    pub fn permits(self, req: ProtocolKind) -> bool {
        matches!(self, ProtocolKind::Any) || self == req
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct DestinationRule {
    pub host_pattern: HostPattern,
    pub port_range: PortRange,
    /// L4 protocol this rule permits. `#[serde(default)]` → `Any` for
    /// pre-UDP stored rules + omitting it on the wire. Gated in
    /// `tunnel_core::policy::evaluate`.
    #[serde(default)]
    pub proto: ProtocolKind,
}

/// Who a policy applies to. `{"kind":"all_users"}` is the catch-all
/// (default-allow lite — still scoped to the tenant). Externally
/// tagged would be cleaner but mixes object-vs-string on the wire
/// when one variant is a unit; `tag = "kind"` keeps everything as
/// objects.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PolicySubject {
    UserId {
        #[serde(rename = "id")]
        user_id: ObjectId,
    },
    RoleId {
        #[serde(rename = "id")]
        role_id: ObjectId,
    },
    TunnelClientId {
        #[serde(rename = "id")]
        tunnel_client_id: ObjectId,
    },
    /// A specific agent acting as a tunnel CLIENT (node-stack unification,
    /// P3b-2). Orthogonal to `PolicyTarget::AgentId` (which names the forward's
    /// DESTINATION): here the agent is the ORIGIN of the tunnel. Purely additive
    /// — old policy docs never carry this variant, so no migration is needed.
    AgentId {
        #[serde(rename = "id")]
        agent_id: ObjectId,
    },
    /// Every user in the policy's tenant.
    AllUsers,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PolicyTarget {
    AgentId {
        #[serde(rename = "id")]
        agent_id: ObjectId,
    },
    /// Every agent in the policy's tenant.
    AllAgents,
}

/// A tenant-scoped allowlist. Default-deny: a forward is permitted
/// only if at least one matching policy exists. See plan §"Security
/// model" + `tunnel-core::policy::evaluate` for the eval semantics.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TunnelPolicy {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub tenant_id: ObjectId,
    pub name: String,
    pub subjects: Vec<PolicySubject>,
    pub targets: Vec<PolicyTarget>,
    pub allowlist: Vec<DestinationRule>,
    /// Per-session concurrent-flow ceiling. `None` = unlimited.
    /// Default 64 in v1 (covers JDBC pools comfortably).
    pub max_concurrent_flows: Option<u32>,
    /// Per-session byte ceiling (sum of bytes_in + bytes_out).
    /// `None` = unlimited.
    pub max_bytes_per_session: Option<u64>,
    pub created_at: DateTime,
    pub updated_at: DateTime,
    pub deleted_at: Option<DateTime>,
}

impl TunnelPolicy {
    pub const COLLECTION: &'static str = "tunnel_policies";
}

// ────────────────────────────────────────────────────────────────────────────
// Overlay ACL
// ────────────────────────────────────────────────────────────────────────────

/// Who an [`OverlayPolicy`] applies to — the node ORIGINATING traffic.
///
/// Deliberately a separate enum from [`PolicySubject`]: the overlay is pure
/// L3, so the tunnel's client/agent-origin distinction is meaningless here,
/// and a node is addressed by its `overlay_nodes` id regardless of whether it
/// is backed by an agent or a tunnel client. Reusing `PolicySubject` would
/// offer variants that can never match an overlay node.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OverlaySelector {
    /// One specific overlay node.
    NodeId {
        #[serde(rename = "id")]
        node_id: ObjectId,
    },
    /// Every node owned by this user (resolved through the node's backing
    /// agent / tunnel-client `owner_user_id`).
    UserId {
        #[serde(rename = "id")]
        user_id: ObjectId,
    },
    /// Every node whose owner holds this role.
    RoleId {
        #[serde(rename = "id")]
        role_id: ObjectId,
    },
    /// Every node in the policy's tenant.
    AllNodes,
}

/// Which node a policy lets the sources reach — the peer, and (when it is a
/// subnet router) the gateway for its approved CIDRs.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OverlayTarget {
    NodeId {
        #[serde(rename = "id")]
        node_id: ObjectId,
    },
    /// Every node in the policy's tenant.
    AllNodes,
}

/// One destination grant: a CIDR plus an optional port/protocol narrowing.
///
/// ⚠️ `port_range` / `proto` are **stored and distributed but do not affect
/// netmap shaping**, because peer visibility and route lists are the only
/// primitives the netmap wire format can express — there is no port dimension
/// in [`crate::signaling::NetmapPeer`]. They take effect only once the
/// node-side ingress filter consumes them. Until then a port-narrowed rule
/// grants the whole peer at L3, so the admin UI must say so rather than imply
/// a narrowing that is not enforced.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct OverlayRule {
    /// Destination prefix, e.g. `"10.84.6.0/24"` or a peer's `"100.64.0.7/32"`.
    pub cidr: String,
    #[serde(default = "OverlayRule::all_ports")]
    pub port_range: PortRange,
    #[serde(default)]
    pub proto: ProtocolKind,
}

impl OverlayRule {
    fn all_ports() -> PortRange {
        PortRange {
            low: 1,
            high: u16::MAX,
        }
    }
}

/// How strictly a tenant's overlay ACL is applied. Stored on the tenant's
/// [`OverlayNetwork`] (one row per tenant) rather than on each policy — it is
/// a network-wide posture, and putting it there means the join path already
/// has it loaded.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum OverlayAclMode {
    /// Legacy behaviour: every node sees every peer and every approved route.
    /// The default, so enabling the feature never breaks a live mesh.
    #[default]
    Off,
    /// Evaluate and log what WOULD be denied, but ship the permissive netmap.
    /// The safe way to author policies against real traffic before cutting over.
    Warn,
    /// Evaluate and enforce.
    Enforce,
}

/// A tenant-scoped overlay access rule. Default-deny **once the tenant's
/// [`OverlayAclMode`] is `Enforce`**; `Off` (the default) preserves the
/// historical "same tenant + same network ⇒ full visibility" behaviour.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct OverlayPolicy {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub tenant_id: ObjectId,
    pub name: String,
    /// Lets an admin park a rule without deleting it.
    #[serde(default = "crate::models::default_true")]
    pub enabled: bool,
    pub sources: Vec<OverlaySelector>,
    pub via: Vec<OverlayTarget>,
    pub destinations: Vec<OverlayRule>,
    pub created_at: DateTime,
    pub updated_at: DateTime,
    pub deleted_at: Option<DateTime>,
}

impl OverlayPolicy {
    pub const COLLECTION: &'static str = "overlay_policies";
}

pub(crate) fn default_true() -> bool {
    true
}

// ────────────────────────────────────────────────────────────────────────────
// Tunnel audit
// ────────────────────────────────────────────────────────────────────────────

/// What happened. Drives the audit-log roll-up + the admin search
/// view in T4. Wire form is snake_case for consistency with every
/// other enum in this module. Distinct from the existing
/// `AuditKind` (remote-control sessions) — different collection,
/// different concerns, different consumers.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TunnelAuditKind {
    /// WebRTC peer was opened (one per `tunnel forward` invocation).
    PeerOpen,
    /// WebRTC peer was torn down (Ctrl-C, idle-timeout, etc.).
    PeerClose,
    /// `TcpForwardRequest` was allowed and the agent dialed dst
    /// successfully. Has flow_id + dst_host + dst_port set.
    TcpAccept,
    /// `TcpForwardRequest` was denied — by the server-side ACL gate
    /// OR by the agent's belt-and-suspenders allowlist. Reason +
    /// `RejectKind` carried in the `reason` field.
    TcpReject,
    /// Agent tried to dial dst, got a hard failure (timeout / refused
    /// / dns). Separate from `TcpReject` so the dashboard can
    /// distinguish "policy denied" from "network broken".
    TcpDialFailed,
    /// Flow closed cleanly or via I/O error.
    TcpClosed,
    /// Per-policy concurrency or byte ceiling hit.
    RateLimited,
    /// WS revocation re-check fired mid-session (admin set
    /// `Quarantined` or soft-deleted the row).
    StatusRevoke,
}

/// Which relay path the peer connection ended up using. Direct =
/// UDP hole punch worked; TurnUdp/Tcp = went through coturn (counts
/// against our bandwidth bill). Set on `PeerOpen` once ICE finishes
/// gathering, repeated on `PeerClose` for easy aggregation.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RelayMode {
    Direct,
    TurnUdp,
    TurnTcp,
}

/// Append-only audit event. One row per interesting happening, keyed
/// by `tunnel_session_id` so a single session reconstruct is
/// `find({tunnel_session_id: …}).sort({at: 1})`. 90 d TTL — see
/// `crates/db/src/indexes.rs::tunnel_audit`.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TunnelAuditEvent {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub tenant_id: ObjectId,
    /// Correlation key — every event for one peer lifetime shares
    /// this id. New ObjectId per `tunnel forward` invocation.
    pub tunnel_session_id: ObjectId,
    /// The originating tunnel-CLIENT row, set when a dedicated
    /// `roomler-tunnel` client opened this session. `None` for an
    /// agent-originated session (P3b-2), where `origin_agent_id` is set
    /// instead. Exactly one of `tunnel_client_id` / `origin_agent_id`
    /// is populated. Optional (not a bare `ObjectId`) since P3b-2 —
    /// old rows carry a bare id which still deserialises into `Some`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tunnel_client_id: Option<ObjectId>,
    /// The originating AGENT, set when an enrolled agent drove the
    /// tunnel-client role over its own WS (P3b-2). `None` for a
    /// dedicated-client session. `agent_id` below is always the TARGET
    /// of the tunnel, regardless of origin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_agent_id: Option<ObjectId>,
    pub agent_id: ObjectId,
    pub user_id: ObjectId,
    pub at: DateTime,
    pub kind: TunnelAuditKind,
    /// Set for per-flow events (TcpAccept / TcpReject /
    /// TcpDialFailed / TcpClosed / RateLimited); None for
    /// peer-lifetime events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flow_id: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dst_host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dst_port: Option<u16>,
    #[serde(default)]
    pub bytes_in: u64,
    #[serde(default)]
    pub bytes_out: u64,
    /// Inferred proxy for "amount of activity" — packet count proxy
    /// (DC messages received). Helps distinguish bulk transfer from
    /// interactive sessions in the dashboard.
    #[serde(default)]
    pub message_count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u32>,
    pub relay: RelayMode,
    /// Source IP of the tunnel client's WS connection (from
    /// X-Forwarded-For on the WS upgrade). Forensic baseline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_src_ip: Option<String>,
    /// Source port on the agent's outgoing TCP socket — lets the DB's
    /// own audit log be correlated with this row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_src_port: Option<u16>,
    pub client_version: String,
    pub client_os: OsKind,
    /// Free-form reason field (e.g. `"acl_denied: no matching policy"`,
    /// `"dial: connection refused"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl TunnelAuditEvent {
    pub const COLLECTION: &'static str = "tunnel_audit";
}

// ────────────────────────────────────────────────────────────────────────────
// Fleet-RPC audit
// ────────────────────────────────────────────────────────────────────────────

/// The result of one remote command — the Hub's currency, the HTTP response
/// body, and the payload of the cross-pod `exec.dispatch` reply. Mirrors
/// `ClientMsg::RpcResult` minus the correlation id.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct ExecOutcome {
    /// `None` when the command never ran or was killed — see
    /// [`Self::error`]. A caller must be able to tell that apart from
    /// "ran and exited 0".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Already redacted + capped by the agent.
    #[serde(default)]
    pub stdout: String,
    #[serde(default)]
    pub stderr: String,
    #[serde(default)]
    pub truncated: bool,
    #[serde(default)]
    pub duration_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Why an exec request was refused. `None` on the audit row means it ran.
///
/// Every denial is recorded — a refused exec is the interesting one, and
/// without it a probing caller leaves no trace.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecDenyReason {
    /// Gate 1 — the org's `remote_exec_enabled` kill-switch is off.
    OrgDisabled,
    /// Gate 2 — the caller lacks `EXEC_DEVICE`.
    NoPermission,
    /// Gate 3 — the target device's [`ExecPolicy::mode`] is `Off`.
    DeviceDisabled,
    /// Gate 3 — the caller isn't in the device's allowed users/roles.
    CallerNotAllowed,
    /// Gate 3 — the requested shell isn't in the device's allowed list.
    ShellNotAllowed,
    /// The CLI origin device isn't blessed with `can_originate`.
    OriginNotAllowed,
    /// The agent doesn't advertise the `exec` RPC capability.
    Unsupported,
    /// The device is offline.
    Offline,
    /// The operator denied the consent prompt (or it timed out).
    ConsentDenied,
    /// Per-(user, device) rate limit.
    RateLimited,
    /// Gate 4 — the agent's own `exec_enabled` config key is false. Reported
    /// by the agent rather than decided server-side.
    AgentDisabled,
}

/// One remote-execution attempt. Written on EVERY attempt, allowed or denied.
/// TTL-expired after 90 days like [`RemoteAuditEvent`].
///
/// `stdout` / `stderr` here are the REDACTED, truncated samples — the agent
/// sweeps its output for the agent token, `Bearer …`, and JWT-shaped strings
/// before anything leaves the host, so a secret echoed by a command never
/// reaches this collection.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ExecAuditEvent {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub tenant_id: ObjectId,
    /// The device the command was aimed at.
    pub agent_id: ObjectId,
    /// The acting principal. For a CLI-originated request this is the
    /// originating device's `owner_user_id`.
    pub user_id: ObjectId,
    /// Set when the request came from a device's LocalAPI (`roomler exec`)
    /// rather than an authenticated HTTP caller.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_agent_id: Option<ObjectId>,
    /// Correlates the audit row with the wire `request_id`.
    pub request_id: String,
    /// `cli` | `ui` | `api`.
    pub source: String,
    pub shell: String,
    /// The command verbatim, as submitted.
    pub command: String,
    pub at: DateTime,
    /// `None` when the request never ran (see [`Self::denied`]) or the agent
    /// failed to report one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// Refusal reason; `None` = the command ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub denied: Option<ExecDenyReason>,
    /// Redacted, truncated output sample (both streams, capped).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub output_sample: String,
    /// SHA-256 of the full redacted output, so a truncated sample can still
    /// be tied to what actually ran.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub output_sha256: String,
    #[serde(default)]
    pub output_bytes: u64,
    #[serde(default)]
    pub truncated: bool,
}

impl ExecAuditEvent {
    pub const COLLECTION: &'static str = "exec_audit";
    /// Cap on the persisted output sample. Keeps a busy fleet's audit
    /// collection bounded while retaining enough to read at a glance.
    pub const SAMPLE_BYTES: usize = 4096;
}

// ────────────────────────────────────────────────────────────────────────────
// Session
// ────────────────────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionPhase {
    Pending,
    AwaitingConsent,
    Negotiating,
    Active,
    Closed,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EndReason {
    ControllerHangup,
    AgentHangup,
    UserDenied,
    ConsentTimeout,
    AgentDisconnect,
    AdminTerminated,
    IdleTimeout,
    Error,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct SessionStats {
    pub bytes_sent: u64,
    pub bytes_recv: u64,
    pub peak_fps: f32,
    pub avg_rtt_ms: f32,
    pub keyframe_requests: u32,
    pub input_events: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RemoteSession {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub agent_id: ObjectId,
    pub tenant_id: ObjectId,
    pub controller_user_id: ObjectId,
    #[serde(default)]
    pub watchers: Vec<ObjectId>,
    pub permissions: Permissions,
    pub phase: SessionPhase,
    pub created_at: DateTime,
    pub started_at: Option<DateTime>,
    pub ended_at: Option<DateTime>,
    pub end_reason: Option<EndReason>,
    pub recording_url: Option<String>,
    #[serde(default)]
    pub stats: SessionStats,
}

impl RemoteSession {
    pub const COLLECTION: &'static str = "remote_sessions";
}

// ────────────────────────────────────────────────────────────────────────────
// Audit
// ────────────────────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AuditKind {
    /// Multi-user P3 — carries the ACTOR + effective grant (the plan's
    /// per-event attribution): with N concurrent sessions per agent, "who
    /// asked" is no longer derivable from the one live session. The same
    /// event now drives the `remote_sessions` row projection in the audit
    /// sink. Fields are serde-defaulted so pre-P3 rows still deserialize
    /// (defaults: zero ObjectId / empty name / default grant).
    SessionRequested {
        #[serde(default)]
        controller_user_id: ObjectId,
        #[serde(default)]
        controller_name: String,
        #[serde(default)]
        permissions: Permissions,
    },
    ConsentPrompted,
    ConsentGranted,
    ConsentDenied,
    ConsentTimedOut,
    SessionStarted,
    SessionEnded {
        reason: EndReason,
    },
    ClipboardWriteToHost {
        bytes: u32,
    },
    ClipboardReadFromHost {
        bytes: u32,
    },
    FileSentToHost {
        name: String,
        bytes: u64,
    },
    FileSentFromHost {
        name: String,
        bytes: u64,
    },
    KeyframeRequested,
    PermissionsChanged {
        permissions: Permissions,
    },
    WatcherJoined {
        user_id: ObjectId,
    },
    WatcherLeft {
        user_id: ObjectId,
    },
    Error {
        message: String,
    },
    /// An `ADMINISTRATOR` started this session via break-glass, skipping the
    /// device's consent mode. `reason` is operator-supplied and mandatory — the
    /// accountability record for a forced, unconsented session (docs §11.5).
    AdminOverride {
        reason: String,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RemoteAuditEvent {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub session_id: ObjectId,
    pub agent_id: ObjectId,
    pub tenant_id: ObjectId,
    pub at: DateTime,
    pub event: AuditKind,
}

impl RemoteAuditEvent {
    pub const COLLECTION: &'static str = "remote_audit";
}

// ────────────────────────────────────────────────────────────────────────────
// Agent crash report
// ────────────────────────────────────────────────────────────────────────────

/// Why the agent considers this a crash. Shared between the agent's
/// `crash_recorder` writer and the backend's ingest handler so a
/// future tag rename never silently breaks deserialisation.
///
/// Serialised as snake_case strings (`panic` / `watchdog_stall` /
/// `supervisor_detected`) — admin UI keys its chip-colour map off
/// these EXACT strings.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CrashReason {
    /// `std::panic::set_hook` fired in the worker process.
    Panic,
    /// `watchdog::force_exit_on_stall` was called — a registered
    /// pump's heartbeat gap exceeded its threshold (default 90 s).
    WatchdogStall,
    /// Windows SCM supervisor detected the worker process exited
    /// with a non-zero code (and the code wasn't `STALL_EXIT_CODE`,
    /// which is recorded at the watchdog site instead).
    SupervisorDetected,
}

/// Wire shape for the agent → roomler.ai crash-report upload AND the
/// on-disk sidecar the agent writes between crash + upload. `rename_
/// all = "camelCase"` so JS clients get `crashedAtUnix` etc. without
/// a translation step.
///
/// Size budget: 64 KiB total when JSON-serialised. The agent's
/// `crash_recorder::record` enforces this by trimming the
/// `log_tail` (oldest lines first) before write; the backend's
/// ingest route enforces it again with an 80 KiB body limit on the
/// HTTP request (small JSON overhead beyond the payload).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AgentCrashPayload {
    /// Unix seconds at the moment the crash was recorded ON THE
    /// AGENT. The backend stamps its own `reported_at` server-clock
    /// timestamp on ingest; admin UI shows both so clock-skewed
    /// hosts are visible.
    pub crashed_at_unix: i64,
    pub reason: CrashReason,
    /// One-line summary suitable for a list-view row (panic
    /// message, "pumps stalled (signaling=120s)", or "worker exit
    /// code 134"). May carry a trailing `[scrubbed N tokens]`
    /// marker if the scrub pipeline redacted credentials from the
    /// summary.
    pub summary: String,
    /// Last ~200 lines of the rolling agent log, after credential
    /// scrubbing. Truncated with a leading `[…log truncated to fit
    /// 64 KiB envelope…]\n` marker if the original tail wouldn't
    /// fit the size budget.
    pub log_tail: String,
    /// `env!("CARGO_PKG_VERSION")` at crash time.
    pub agent_version: String,
    /// `"windows"` / `"linux"` / `"macos"` — same string surface as
    /// `OsKind::serialize` would emit but kept as a plain String
    /// here so the payload doesn't depend on the OsKind enum
    /// position.
    pub os: String,
    /// Hostname at crash time.
    pub hostname: String,
    /// OS process id of the crashed worker (or supervisor, for the
    /// supervisor-detected branch).
    pub pid: u32,
    /// rc.51: how many crash sidecars were rate-limit-suppressed
    /// (`crash_recorder` 1/60 s throttle) between the previous
    /// successfully-written sidecar and this one. `0` in steady
    /// state; a high value means a tight crash-loop was in progress
    /// and most of its iterations went unrecorded — so this one
    /// sidecar represents `1 + suppressed_since_last` crashes.
    /// `#[serde(default)]` so pre-rc.51 sidecars (which lack the
    /// field) still deserialise.
    #[serde(default)]
    pub suppressed_since_last: u32,
}

/// Server-side persisted form of an agent crash report. The MongoDB
/// collection is `agent_crashes`; admin UI fetches via the protected
/// `GET /api/tenant/{tenant_id}/agent/{agent_id}/crash` endpoint.
///
/// Fields:
/// - `_id` / `tenant_id` / `agent_id` — server-attributed (resolved
///   from the agent JWT at ingest time).
/// - `reported_at` — server clock at ingest. Distinct from the
///   payload's `crashed_at_unix` (agent clock) so clock-skewed hosts
///   are visible in the admin UI.
/// - Everything else is flattened from [`AgentCrashPayload`] via
///   `#[serde(flatten)]`. The MongoDB BSON uses camelCase keys
///   matching the wire shape — no rename for the DB layer because
///   the payload's `rename_all = "camelCase"` carries through.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct AgentCrashRecord {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub tenant_id: ObjectId,
    pub agent_id: ObjectId,
    pub reported_at: DateTime,
    #[serde(flatten)]
    pub payload: AgentCrashPayload,
}

impl AgentCrashRecord {
    pub const COLLECTION: &'static str = "agent_crashes";
}

// ────────────────────────────────────────────────────────────────────────────
// Overlay network (Tailscale-style L3 mesh)
// ────────────────────────────────────────────────────────────────────────────
//
// An overlay node is the unifying layer above `Agent` and `TunnelClient`:
// either kind of host can join a per-tenant virtual LAN, get a stable
// overlay IP, and reach any permitted peer at L3 over WireGuard. The two
// underlying collections keep their distinct lifecycles/audiences; an
// `OverlayNode` references one of them via [`NodeRef`] and adds the
// overlay-specific identity (WG pubkey + overlay IP + endpoints).

/// Which underlying host an [`OverlayNode`] is. Adjacently tagged so the
/// BSON/JSON shape is `{"kind":"agent","id":<oid>}` — mirrors the
/// `PolicySubject` / `PolicyTarget` style. The `id` stays a native
/// ObjectId for DB rows (Mongo indexes rely on native encoding); the
/// wire/netmap exposes nodes by their `overlay_nodes._id`, not by this.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NodeRef {
    Agent {
        #[serde(rename = "id")]
        agent_id: ObjectId,
    },
    TunnelClient {
        #[serde(rename = "id")]
        tunnel_client_id: ObjectId,
    },
}

/// One member of a tenant's overlay network. Keyed for rehydrate-on-
/// re-enroll by `(tenant_id, machine_id)` exactly like [`Agent`] /
/// [`TunnelClient`], so a re-joining host keeps its overlay IP (and may
/// register a rotated WG key). The WG **private** key never leaves the
/// node; only `wg_public_key` is stored + distributed in the netmap.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct OverlayNode {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub tenant_id: ObjectId,
    pub node_ref: NodeRef,
    pub network_id: ObjectId,
    /// Rehydrate key — carried from the underlying agent/tunnel-client so
    /// a re-join finds the existing row (and its leased overlay IP).
    pub machine_id: String,
    /// Human-facing node name — denormalized from the underlying
    /// [`Agent`]/[`TunnelClient`] `name` at join, sanitized to a DNS label and
    /// made unique per network (collisions get a `-2`/`-3` suffix). This is the
    /// MagicDNS authority and the netmap's `name`. Empty on rows created before
    /// Phase 0 (Tailscale-style names).
    #[serde(default)]
    pub name: String,
    /// Leased overlay address, e.g. `"100.64.0.7"`. Stable for as long as the
    /// row is LIVE. On release (device removed from the fleet) the row is
    /// tombstoned and the host number goes back to `OverlayNetwork.free_hosts`
    /// for reuse — the address is KEPT here as the forensic record of who held
    /// it, which matters precisely because addresses now recycle. The
    /// `(tenant_id, network_id, overlay_ip)` unique index is scoped to live
    /// rows, so a tombstone holds nothing.
    pub overlay_ip: String,
    /// base64-encoded Curve25519 public key (WireGuard static key).
    pub wg_public_key: String,
    /// Bumped on key rotation (Phase 5). `0` at first join.
    #[serde(default)]
    pub key_epoch: u32,
    /// Current connectivity candidates (srflx / relay), as `host:port`
    /// strings the peer can dial. REPLACED on each `rc:overlay.endpoints`
    /// trickle from the relay coordinator.
    #[serde(default)]
    pub endpoints: Vec<String>,
    /// rc.135 — DIRECT LAN candidates, set from the agent's JOIN (kept in a
    /// SEPARATE bucket so the relay-endpoint trickle — which REPLACES
    /// `endpoints` — can't clobber them). The netmap a peer receives unions
    /// `lan_endpoints ∪ endpoints` so a same-subnet peer can always find the
    /// LAN address and go direct. (Field 2026-06-27: the trickle stripped
    /// `192.168.68.x` from nodes that had allocated a relay, forcing every
    /// peer onto the relay path.) Refreshed on each (re)join, so a DHCP IP
    /// change is picked up.
    #[serde(default)]
    pub lan_endpoints: Vec<String>,
    /// NAT-traversal Phase B — the node's SERVER-REFLEXIVE (srflx) candidates:
    /// its public `ip:port` as seen through its NAT, discovered by the node
    /// querying STUN on its own traffic sockets and trickled up via
    /// `rc:overlay.srflx`. A THIRD bucket, separate from `lan_endpoints` (public
    /// NIC) and `endpoints` (relay), so no trickle clobbers another's provenance
    /// (CC2). Surfaced VERBATIM in the netmap as `NetmapPeer.srflx_endpoints` so
    /// a peer behind a different NAT can dial this node directly (1:1 / cone NAT
    /// only). Reset to empty on each (re)join — the node re-gathers + re-trickles
    /// fresh srflx per connection, so a stale mapping never lingers.
    #[serde(default)]
    pub srflx_endpoints: Vec<String>,
    /// NAT-traversal Phase C — the node's probed NAT mapping type (`"cone"` /
    /// `"symmetric"`), trickled alongside its srflx via `rc:overlay.srflx` and
    /// surfaced as `NetmapPeer.srflx_nat`. A dialer skips the punch only when
    /// BOTH ends are `"symmetric"`. `None` = unknown (attempted, never skipped).
    /// Reset on each (re)join with `srflx_endpoints` — a prior session's NAT
    /// class is meaningless after a roam (A8).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub srflx_nat: Option<String>,
    /// Preferred relay region/home, if any (Phase 5 multi-relay).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay_home: Option<String>,
    /// rc.142 — the node advertised (on JOIN) that it can carry WG over a
    /// QUIC-over-TURN relay carrier. Echoed per-peer in the netmap so QUIC is
    /// only attempted when both ends support it (no silent QUIC/raw split).
    #[serde(default)]
    pub supports_quic: bool,
    /// Phase D — the node advertised (on JOIN) that it can run the v1
    /// single-relay carrier (one anchor allocation + a raw dialer). Echoed
    /// per-peer in the netmap so single-relay is only chosen when both ends
    /// support it (a mismatch would split the pair into anchor/dialer).
    #[serde(default)]
    pub supports_relay_single: bool,
    /// Phase D (DERP) — the node advertised (on JOIN) that it can carry WG over
    /// the pubkey-addressed `/derp` relay, the last-resort carrier for two
    /// BOTH-UDP-blocked peers. Echoed per-peer in the netmap so DERP is only
    /// chosen when both ends support it. Absent on a pre-DERP row ⇒ `false`.
    #[serde(default)]
    pub supports_derp: bool,
    /// P7 — the node advertised (on JOIN) that it honors the server's per-pair
    /// `rc:overlay.force_derp` escalation push. The broker only escalates a
    /// churning pair when BOTH ends carry this, so a mixed-version pair can
    /// never split tiers. Absent on a pre-P7 row ⇒ `false`.
    #[serde(default)]
    pub supports_forced_derp: bool,
    /// Phase 1 — subnet CIDRs this node CLAIMS it can route for peers (from its
    /// `--advertise-routes` config, refreshed on each join). Untrusted until an
    /// admin approves; see `approved_routes`.
    #[serde(default)]
    pub advertised_routes: Vec<String>,
    /// Phase 1 — the admin-APPROVED subset of `advertised_routes`, distributed
    /// to peers as the netmap `routes`. Empty = this node routes nothing for
    /// anyone. An admin manages this via the overlay-route approval UI.
    ///
    /// P5: a default route `0.0.0.0/0` here marks this node as an approved
    /// exit node (clients infer exit-nodes from an approved `/0` in the
    /// netmap). `/0` may ONLY enter this list via the exit-node toggle
    /// (`set_exit_node`), never the per-CIDR approval grid — see
    /// `set_approved_routes`'s `/0` guard — so an admin can't accidentally
    /// route the whole internet with a mis-clicked checkbox.
    #[serde(default)]
    pub approved_routes: Vec<String>,
    /// P5 — admin has designated this node as an exit node (routes
    /// `0.0.0.0/0` for opted-in clients). Kept as an explicit flag for the
    /// admin UI + approval semantics; the DATA-PLANE signal a client keys
    /// off is still the approved `0.0.0.0/0` in the netmap. Toggling this
    /// on requires the node to have advertised `0.0.0.0/0` and adds it to
    /// `approved_routes`; toggling off removes it.
    #[serde(default)]
    pub is_exit_node: bool,
    pub status: AgentStatus,
    pub last_seen_at: DateTime,
    pub created_at: DateTime,
    pub updated_at: DateTime,
    pub deleted_at: Option<DateTime>,
}

impl OverlayNode {
    pub const COLLECTION: &'static str = "overlay_nodes";
}

/// P5 — the IPv4 default route. Its presence in a node's `approved_routes`
/// is the wire signal that the node is an approved exit node; it may only be
/// added via the exit-node toggle, never the per-CIDR approval grid.
pub const DEFAULT_ROUTE_V4: &str = "0.0.0.0/0";

/// IPAM authority for one tenant's overlay. One row per tenant. The allocator
/// prefers a RECYCLED host from `free_hosts` (returned when a device is removed
/// from the fleet) and otherwise hands out the next number from the monotonic
/// `next_host` cursor (atomic `$inc`). A lease is stable for as long as its node
/// row is live.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct OverlayNetwork {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub tenant_id: ObjectId,
    /// CGNAT range per the Tailscale convention, e.g. `"100.64.0.0/10"`.
    pub cidr: String,
    /// Monotonic host cursor — the next host number to hand out. `1` for
    /// a fresh network (host `0` is the network address, reserved). Only
    /// bumped when [`free_hosts`](Self::free_hosts) is empty.
    pub next_host: u32,
    /// Host numbers RECYCLED from removed devices, oldest release first. A
    /// join pops the head before touching `next_host`, so a released address
    /// gets the longest possible cool-down before it is handed out again.
    ///
    /// `#[serde(default)]` is LOAD-BEARING: every `overlay_networks` row
    /// written before the release feature lacks this field, and without the
    /// default they would all fail to deserialize — taking the entire overlay
    /// down on the first `get_or_create`.
    #[serde(default)]
    pub free_hosts: Vec<u32>,
    /// Path MTU for the overlay. 1280 leaves headroom for the WG +
    /// carrier (UDP/relay) overhead under a 1500-byte underlay.
    pub mtu: u16,
    /// How strictly [`OverlayPolicy`] rows are applied for this tenant.
    ///
    /// `#[serde(default)]` is LOAD-BEARING for exactly the reason spelled out
    /// on [`free_hosts`](Self::free_hosts): every row written before the ACL
    /// feature lacks this field, and without the default they would all fail
    /// to deserialize and take the whole overlay down on the next
    /// `get_or_create`. The default is [`OverlayAclMode::Off`], so an existing
    /// mesh keeps its current behaviour until an admin opts in.
    #[serde(default)]
    pub acl_mode: OverlayAclMode,
    pub created_at: DateTime,
    pub updated_at: DateTime,
}

impl OverlayNetwork {
    pub const COLLECTION: &'static str = "overlay_networks";
    /// Default tenant overlay range (CGNAT block, like Tailscale).
    pub const DEFAULT_CIDR: &'static str = "100.64.0.0/10";
    /// Default overlay MTU.
    pub const DEFAULT_MTU: u16 = 1280;
    /// Ceiling on [`free_hosts`](Self::free_hosts). A `u32` array element costs
    /// ~9 bytes of BSON, so the 16 MB document cap is ~1.7 M entries — this cap
    /// sits far below it. Past the cap a release is DROPPED and the host leaks
    /// out of a 4.2 M-address `/10`: a leak, never a conflict.
    pub const MAX_FREE_HOSTS: usize = 65_536;

    /// This network's dotted address for `host`. See [`overlay_ip`].
    pub fn host_ip(&self, host: u32) -> Option<String> {
        overlay_ip(&self.cidr, host)
    }

    /// The host number `ip` was leased from in this network. See [`overlay_host`].
    pub fn host_of_ip(&self, ip: &str) -> Option<u32> {
        overlay_host(&self.cidr, ip)
    }

    /// Multi-org P2a — the highest host ordinal this network's CIDR can
    /// lease. See [`cidr_max_host`]; `0` (nothing leasable) on a malformed
    /// CIDR so a bad row FAILS CLOSED at allocation instead of handing out
    /// unbounded addresses.
    pub fn max_host(&self) -> u32 {
        cidr_max_host(&self.cidr).unwrap_or(0)
    }
}

/// Multi-org P2a — the highest leaseable host ordinal for `cidr`:
/// `2^(32-prefix) - 2` (host 0 is the network address, the block's last
/// address is reserved by convention), `None` for a malformed CIDR or a
/// prefix ≥ 31 (no leasable hosts either way).
///
/// The forward-compat point: `allocate_host`'s cursor previously grew
/// UNBOUNDED — under tenant-block addressing (P2b hands tenants sub-blocks
/// of `100.64.0.0/10`, e.g. a `/22`) a busy tenant's cursor would have
/// silently walked into the NEIGHBOR tenant's block: the exact cross-tenant
/// address collision the blocks exist to kill. Every allocation is now
/// bounded by the network's OWN CIDR — behaviour-neutral for today's
/// `/10` tenants (max host 4 194 302, unreachable), binding the moment a
/// tenant's `cidr` becomes a real block.
pub fn cidr_max_host(cidr: &str) -> Option<u32> {
    let (_base, prefix) = cidr.split_once('/')?;
    let prefix: u8 = prefix.parse().ok()?;
    if prefix >= 31 {
        return None;
    }
    // 2^(32-prefix) - 2; prefix 0 would overflow the shift — cap at /1.
    let size: u64 = 1u64 << (32 - prefix.min(31) as u64);
    u32::try_from(size - 2).ok()
}

/// `base_cidr` + host number → dotted overlay IP. e.g.
/// `("100.64.0.0/10", 7) → "100.64.0.7"`. No `ipnet` needed — the host
/// number is added to the network base address as a `u32`.
///
/// The base is taken from the LEFT of the `/` (not the masked network
/// address). [`overlay_host`] inverts this by parsing the same way, and the
/// round-trip test in this module pins the pair together — do not "fix" one
/// to mask without the other.
///
/// Multi-org P2a: `host` is bounded by [`cidr_max_host`] — a host past the
/// block renders `None` instead of an address in the NEIGHBOR block.
pub fn overlay_ip(cidr: &str, host: u32) -> Option<String> {
    if host > cidr_max_host(cidr)? {
        return None;
    }
    let (base, _prefix) = cidr.split_once('/')?;
    let base: std::net::Ipv4Addr = base.parse().ok()?;
    let addr = std::net::Ipv4Addr::from(u32::from(base).checked_add(host)?);
    Some(addr.to_string())
}

/// Exact inverse of [`overlay_ip`] — recover the host number a dotted overlay
/// address was leased from, so a removed device's number can be returned to the
/// network's free pool.
///
/// `None` when `cidr`/`ip` are malformed, when `ip` sorts BELOW the base, or
/// (multi-org P2a) ABOVE the block's last leaseable host: both directions are
/// how a row leased under a DIFFERENT, since-changed CIDR is rejected instead
/// of poisoning the pool with a bogus host number.
pub fn overlay_host(cidr: &str, ip: &str) -> Option<u32> {
    let (base, _prefix) = cidr.split_once('/')?;
    let base: std::net::Ipv4Addr = base.parse().ok()?;
    let addr: std::net::Ipv4Addr = ip.parse().ok()?;
    let host = u32::from(addr).checked_sub(u32::from(base))?;
    if host > cidr_max_host(cidr)? {
        return None;
    }
    Some(host)
}

// ---------------------------------------------------------------------------
// Multi-org P2b — tenant-block addressing
// ---------------------------------------------------------------------------

/// The CGNAT root every tenant block is carved from.
pub const OVERLAY_BLOCK_ROOT: &str = "100.64.0.0/10";

/// Base address of [`OVERLAY_BLOCK_ROOT`] as a `u32`.
const OVERLAY_BLOCK_ROOT_BASE: u32 = u32::from_be_bytes([100, 64, 0, 0]);

/// The allocation grid: every block is an ALIGNED run of `/22`s. A `/22`
/// (1024 addresses, 1022 leasable) is the smallest unit handed out, so the
/// registry can enforce non-overlap with one integer per block.
pub const OVERLAY_BLOCK_SLOT_PREFIX: u8 = 22;

/// Addresses per slot (`2^(32-22)` = 1024).
pub const OVERLAY_BLOCK_SLOT_SIZE: u32 = 1 << (32 - OVERLAY_BLOCK_SLOT_PREFIX as u32);

/// The first slot the allocator may hand out — slot 64, i.e. `100.65.0.0`.
///
/// The whole of `100.64.0.0/16` (slots 0..63) is RESERVED for LEGACY tenants:
/// every network created before blocks existed carries `100.64.0.0/10` with
/// its host cursor seeded at 1, so all of them sit in `100.64.0.x` and grow
/// upward. Starting new blocks above that reserve means a carved tenant can
/// never collide with a legacy one, no matter how many devices the legacy
/// tenant has leased (65 534 before it would reach slot 64).
pub const OVERLAY_BLOCK_FIRST_SLOT: u32 = 64;

/// Number of slots in the `/10` (`2^(22-10)`).
pub const OVERLAY_BLOCK_SLOT_COUNT: u32 = 1 << (OVERLAY_BLOCK_SLOT_PREFIX as u32 - 10);

/// Largest block a tenant may be carved (a `/16`, 64 slots, 65 534 hosts).
pub const OVERLAY_BLOCK_MIN_PREFIX: u8 = 16;

/// Smallest block a tenant may be carved — one slot.
pub const OVERLAY_BLOCK_MAX_PREFIX: u8 = OVERLAY_BLOCK_SLOT_PREFIX;

/// Lifecycle of a registry row.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum OverlayBlockState {
    /// Currently backing a tenant's overlay network.
    #[default]
    Assigned,
    /// Freed by a renumber. The slots are NEVER handed out again: a device
    /// that missed the migration (offline, or a stale binary) still believes
    /// it holds an address in here, and re-issuing the range to a different
    /// tenant would hand that stale node a live neighbour's address. The row
    /// is kept as the forensic record of who held the range.
    Quarantined,
}

/// One entry in the GLOBAL overlay block registry (multi-org P2b).
///
/// Blocks are allocated MONOTONICALLY upward from
/// [`OVERLAY_BLOCK_FIRST_SLOT`]: the allocator takes the highest `end_slot`
/// in the collection, rounds up to this block's alignment, and inserts with a
/// unique `slot`. Two racers either compute the SAME start (one loses the
/// unique index and retries against the winner's row) or compute
/// buddy-aligned, non-overlapping starts — so the registry never issues
/// overlapping ranges without needing a lock. Quarantined rows keep their
/// slots occupied, which is exactly what makes quarantine free.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct OverlayBlock {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    /// Offset from `100.64.0.0` in [`OVERLAY_BLOCK_SLOT_SIZE`] units.
    /// Globally unique — this is what makes overlap unrepresentable.
    pub slot: u32,
    /// How many contiguous slots this block spans (a power of two; the start
    /// is always a multiple of it).
    pub slots: u32,
    /// `slot + slots`, denormalized so the allocator's "highest end" probe is
    /// a single indexed `sort + limit 1` instead of a collection scan.
    pub end_slot: u32,
    /// The rendered range, e.g. `"100.65.0.0/22"`. This is copied to
    /// `OverlayNetwork.cidr` — the registry is the authority.
    pub cidr: String,
    pub tenant_id: ObjectId,
    pub network_id: ObjectId,
    pub state: OverlayBlockState,
    /// Why the block was quarantined (renumber, block grow, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub released_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub released_at: Option<DateTime>,
    pub created_at: DateTime,
    pub updated_at: DateTime,
}

impl OverlayBlock {
    pub const COLLECTION: &'static str = "overlay_blocks";

    /// Highest host ordinal this block can lease.
    pub fn max_host(&self) -> u32 {
        cidr_max_host(&self.cidr).unwrap_or(0)
    }
}

/// Slots consumed by a block of `prefix`, or `None` when the prefix is outside
/// the supported band ([`OVERLAY_BLOCK_MIN_PREFIX`] ..=
/// [`OVERLAY_BLOCK_MAX_PREFIX`]). Anything longer than a `/22` would have to
/// sub-divide a slot, which the registry deliberately does not model.
pub fn block_slots_for_prefix(prefix: u8) -> Option<u32> {
    if !(OVERLAY_BLOCK_MIN_PREFIX..=OVERLAY_BLOCK_MAX_PREFIX).contains(&prefix) {
        return None;
    }
    Some(1 << (OVERLAY_BLOCK_SLOT_PREFIX as u32 - prefix as u32))
}

/// The rendered CIDR for an aligned run of `slots` slots starting at `slot`.
/// `None` when the run is unaligned, empty, or would leave the `/10`.
pub fn block_cidr_for_slot(slot: u32, slots: u32) -> Option<String> {
    if slots == 0 || !slots.is_power_of_two() || !slot.is_multiple_of(slots) {
        return None;
    }
    let end = slot.checked_add(slots)?;
    if end > OVERLAY_BLOCK_SLOT_COUNT {
        return None;
    }
    let base = OVERLAY_BLOCK_ROOT_BASE.checked_add(slot.checked_mul(OVERLAY_BLOCK_SLOT_SIZE)?)?;
    let prefix = OVERLAY_BLOCK_SLOT_PREFIX as u32 - slots.trailing_zeros();
    Some(format!(
        "{}/{}",
        std::net::Ipv4Addr::from(base),
        prefix as u8
    ))
}

/// The lowest aligned start for a `slots`-wide block that begins at or after
/// `after`. Alignment (start is a multiple of the width) is what makes two
/// concurrently-computed allocations either identical or disjoint.
pub fn block_align_slot(after: u32, slots: u32) -> u32 {
    if slots == 0 {
        return after;
    }
    after.div_ceil(slots) * slots
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_policy_defaults_are_closed() {
        // The whole safety story rests on this: a device row that predates
        // Fleet RPC — i.e. every device in the fleet today — must deserialise
        // to a policy that refuses.
        let p: ExecPolicy = serde_json::from_str("{}").unwrap();
        assert_eq!(p.mode, ExecMode::Off);
        assert!(!p.can_originate);
        assert_eq!(p.effective_consent_mode(), ConsentMode::Prompt);

        // …and so must an Agent row with no `exec_policy` key at all.
        let caps_free = r#"{
            "tenant_id":"000000000000000000000001",
            "owner_user_id":"000000000000000000000002",
            "name":"old","machine_id":"m","os":"windows",
            "agent_version":"0.1.0","agent_token_hash":"",
            "status":"offline","last_seen_at":{"$date":{"$numberLong":"0"}},
            "created_at":{"$date":{"$numberLong":"0"}},
            "updated_at":{"$date":{"$numberLong":"0"}},
            "deleted_at":null
        }"#;
        let a: Agent = serde_json::from_str(caps_free).unwrap();
        assert_eq!(a.exec_policy.mode, ExecMode::Off);
    }

    #[test]
    fn exec_policy_consent_collapses_session_shaped_modes() {
        // Email / Push / PromptThenEmail are approve-link flows with no exec
        // equivalent. They must collapse to Prompt, never silently to Auto.
        for m in [
            ConsentMode::Email,
            ConsentMode::Push,
            ConsentMode::PromptThenEmail,
        ] {
            let p = ExecPolicy {
                consent_mode: Some(m),
                ..Default::default()
            };
            assert_eq!(p.effective_consent_mode(), ConsentMode::Prompt, "{m:?}");
        }
        let p = ExecPolicy {
            consent_mode: Some(ConsentMode::Auto),
            ..Default::default()
        };
        assert_eq!(p.effective_consent_mode(), ConsentMode::Auto);
    }

    #[test]
    fn exec_policy_shell_allowlist() {
        let open = ExecPolicy::default();
        assert!(open.allows_shell("pwsh"), "empty list allows everything");

        let narrowed = ExecPolicy {
            shells: vec!["pwsh".into()],
            ..Default::default()
        };
        assert!(narrowed.allows_shell("pwsh"));
        assert!(
            narrowed.allows_shell("PWSH"),
            "shell names are ASCII-insensitive"
        );
        assert!(!narrowed.allows_shell("cmd"));
    }

    #[test]
    fn exec_limits_clamp() {
        use exec_limits::*;
        // 0 means "unspecified" on the wire, not "instant timeout".
        assert_eq!(clamp_timeout_ms(0), DEFAULT_TIMEOUT_MS);
        assert_eq!(clamp_timeout_ms(1_000), 1_000);
        assert_eq!(clamp_timeout_ms(u64::MAX), MAX_TIMEOUT_MS);

        assert_eq!(clamp_output_bytes(0), DEFAULT_MAX_OUTPUT_BYTES);
        assert_eq!(clamp_output_bytes(1024), 1024);
        assert_eq!(clamp_output_bytes(u64::MAX), MAX_OUTPUT_BYTES);
    }

    #[test]
    fn overlay_ip_adds_host_to_cgnat_base() {
        let cidr = OverlayNetwork::DEFAULT_CIDR;
        assert_eq!(overlay_ip(cidr, 1).as_deref(), Some("100.64.0.1"));
        assert_eq!(overlay_ip(cidr, 7).as_deref(), Some("100.64.0.7"));
        assert_eq!(overlay_ip(cidr, 256).as_deref(), Some("100.64.1.0"));
        assert_eq!(overlay_ip("not-a-cidr", 1), None);
    }

    #[test]
    fn cidr_bounds_cap_host_ordinals_to_the_block() {
        // Multi-org P2a — the whole reason blocks can't bleed: /22 = 1024
        // addresses ⇒ hosts 1..=1022 leaseable.
        assert_eq!(cidr_max_host("100.68.12.0/22"), Some(1022));
        assert_eq!(cidr_max_host("100.64.0.0/10"), Some((1 << 22) - 2));
        assert_eq!(cidr_max_host("100.64.0.0/30"), Some(2));
        assert_eq!(cidr_max_host("100.64.0.0/31"), None, "no leasable hosts");
        assert_eq!(cidr_max_host("100.64.0.0/32"), None);
        assert_eq!(cidr_max_host("garbage"), None);

        // overlay_ip refuses to render past the block…
        assert_eq!(
            overlay_ip("100.68.12.0/22", 1022).as_deref(),
            Some("100.68.15.254")
        );
        assert_eq!(
            overlay_ip("100.68.12.0/22", 1023),
            None,
            "block's last address"
        );
        assert_eq!(overlay_ip("100.68.12.0/22", 1024), None, "neighbor block");
        // …and overlay_host rejects an address above it (a row leased under a
        // since-shrunk CIDR must not poison the pool).
        assert_eq!(overlay_host("100.68.12.0/22", "100.68.15.254"), Some(1022));
        assert_eq!(overlay_host("100.68.12.0/22", "100.68.16.1"), None);
        // The below-base rejection is unchanged.
        assert_eq!(overlay_host("100.68.12.0/22", "100.68.11.9"), None);
    }

    #[test]
    fn overlay_host_inverts_overlay_ip() {
        let cidr = OverlayNetwork::DEFAULT_CIDR;
        assert_eq!(overlay_host(cidr, "100.64.0.7"), Some(7));
        assert_eq!(overlay_host(cidr, "100.64.1.0"), Some(256));
        assert_eq!(overlay_host("10.0.0.0/8", "10.0.0.1"), Some(1));
    }

    /// The contract that keeps the free pool honest: whatever `overlay_ip`
    /// hands out, `overlay_host` must recover exactly. If someone later masks
    /// `overlay_ip` to the network address without fixing the inverse, released
    /// host numbers would be off and the pool would hand out wrong addresses.
    #[test]
    fn overlay_host_round_trips_for_every_host() {
        let cidr = OverlayNetwork::DEFAULT_CIDR;
        for host in [1u32, 2, 255, 256, 65_535, 1_000_000, 4_194_302] {
            let ip = overlay_ip(cidr, host).expect("forward");
            assert_eq!(overlay_host(cidr, &ip), Some(host), "round-trip for {host}");
        }
    }

    /// Both directions ignore the prefix identically, so a non-canonical CIDR
    /// (base not masked to the prefix) still round-trips.
    #[test]
    fn overlay_host_ignores_the_prefix_like_overlay_ip_does() {
        let cidr = "100.64.0.5/10";
        assert_eq!(overlay_ip(cidr, 2).as_deref(), Some("100.64.0.7"));
        assert_eq!(overlay_host(cidr, "100.64.0.7"), Some(2));
    }

    #[test]
    fn overlay_host_rejects_out_of_range_and_malformed() {
        let cidr = OverlayNetwork::DEFAULT_CIDR;
        // Below the base — a lease from a different, since-changed CIDR.
        assert_eq!(overlay_host(cidr, "10.0.0.1"), None);
        assert_eq!(overlay_host("not-a-cidr", "1.2.3.4"), None);
        assert_eq!(overlay_host(cidr, "nope"), None);
    }

    // --- P2b tenant blocks ------------------------------------------------

    #[test]
    fn block_cidr_renders_the_slot_grid() {
        // The first allocatable slot is 100.65.0.0 — everything below it is
        // the legacy reserve.
        assert_eq!(
            block_cidr_for_slot(OVERLAY_BLOCK_FIRST_SLOT, 1).as_deref(),
            Some("100.65.0.0/22")
        );
        assert_eq!(block_cidr_for_slot(65, 1).as_deref(), Some("100.65.4.0/22"));
        assert_eq!(
            block_cidr_for_slot(128, 1).as_deref(),
            Some("100.66.0.0/22")
        );
        // Wider blocks widen the prefix.
        assert_eq!(block_cidr_for_slot(64, 4).as_deref(), Some("100.65.0.0/20"));
        assert_eq!(
            block_cidr_for_slot(64, 64).as_deref(),
            Some("100.65.0.0/16")
        );
        // Unaligned / non-power-of-two / past the /10 are refused.
        assert_eq!(block_cidr_for_slot(65, 4), None, "unaligned");
        assert_eq!(block_cidr_for_slot(64, 3), None, "not a power of two");
        assert_eq!(block_cidr_for_slot(OVERLAY_BLOCK_SLOT_COUNT, 1), None);
        assert_eq!(block_cidr_for_slot(OVERLAY_BLOCK_SLOT_COUNT - 1, 2), None);
        // The last slot IS allocatable.
        assert_eq!(
            block_cidr_for_slot(OVERLAY_BLOCK_SLOT_COUNT - 1, 1).as_deref(),
            Some("100.127.252.0/22")
        );
    }

    #[test]
    fn block_prefix_maps_to_slot_width() {
        assert_eq!(block_slots_for_prefix(22), Some(1));
        assert_eq!(block_slots_for_prefix(20), Some(4));
        assert_eq!(block_slots_for_prefix(16), Some(64));
        assert_eq!(block_slots_for_prefix(23), None, "sub-slot");
        assert_eq!(block_slots_for_prefix(15), None, "wider than /16");
        assert_eq!(block_slots_for_prefix(0), None);
    }

    /// The allocator's safety property: starts are aligned and monotonic, so
    /// two racers computing from the same "highest end" either collide on the
    /// SAME slot (the unique index arbitrates) or claim disjoint ranges. This
    /// walks every width against every possible predecessor end and asserts
    /// the ranges never partially overlap.
    #[test]
    fn aligned_starts_are_never_partially_overlapping() {
        for after in 0u32..512 {
            let mut claims: Vec<(u32, u32)> = Vec::new();
            for prefix in OVERLAY_BLOCK_MIN_PREFIX..=OVERLAY_BLOCK_MAX_PREFIX {
                let slots = block_slots_for_prefix(prefix).expect("supported prefix");
                let start = block_align_slot(after, slots);
                assert!(start >= after, "monotonic: {start} >= {after}");
                assert_eq!(start % slots, 0, "aligned");
                assert!(
                    start - after < slots,
                    "no more than one width of waste (start {start}, after {after})"
                );
                claims.push((start, slots));
            }
            // Buddy property: any two claims are identical-start or disjoint.
            for (i, (s1, w1)) in claims.iter().enumerate() {
                for (s2, w2) in claims.iter().skip(i + 1) {
                    let overlap = s1.max(s2) < &(s1 + w1).min(s2 + w2);
                    assert!(
                        !overlap || s1 == s2,
                        "partial overlap: [{s1},{}) vs [{s2},{})",
                        s1 + w1,
                        s2 + w2
                    );
                }
            }
        }
    }

    /// A carved block's leasable range must stay strictly inside it — the
    /// whole point of the P2a ceiling.
    #[test]
    fn block_leases_stay_inside_the_block() {
        let cidr = block_cidr_for_slot(64, 1).expect("slot 64");
        assert_eq!(cidr, "100.65.0.0/22");
        assert_eq!(cidr_max_host(&cidr), Some(1022));
        assert_eq!(overlay_ip(&cidr, 1).as_deref(), Some("100.65.0.1"));
        assert_eq!(overlay_ip(&cidr, 1022).as_deref(), Some("100.65.3.254"));
        // …and the first address of the NEXT block is unreachable from here.
        assert_eq!(overlay_ip(&cidr, 1024), None);
        assert_eq!(overlay_host(&cidr, "100.65.4.0"), None);
    }
}
