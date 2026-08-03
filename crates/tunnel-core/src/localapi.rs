//! Roomler **LocalAPI** — the local control surface (P1: read-only).
//!
//! The unified daemon (`roomlerd`) will expose this over a local-only channel
//! (named pipe on Windows / unix socket elsewhere; ACL-authenticated — wired in
//! P1-cont) so thin clients — the CLI (`roomler`) and the desktop app (Roomler)
//! — can read live
//! node / peer / flow state without reaching into the daemon's internals. This
//! module is the **transport-agnostic protocol**: the request/response wire
//! types plus a pure [`handle`] dispatch over a [`LocalApiState`] snapshot. The
//! pipe listener + the daemon's `LocalApiState` impl (gathering real overlay /
//! tunnel / forward state) land in P1-cont; keeping the protocol pure here makes
//! it unit-testable with a mock and reusable by both the daemon and clients.
//!
//! Wire shape: newline-delimited JSON, adjacently tagged (`{"t":<verb>}` /
//! `{"t":<verb>,"d":<payload>}`) so a payload may be a struct OR a sequence.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::sync::watch;

/// How this node currently reaches a peer — the Tailscale-style connection
/// type shown per device in the UI. `Tunnel` is the userspace SOCKS/forward
/// path (used when a corp full-tunnel VPN captures the overlay's routes);
/// `Blocked` = a peer with no working carrier; `Offline` = not currently up.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionType {
    Direct,
    Relay,
    Tunnel,
    Blocked,
    Offline,
}

/// Which privilege mode the daemon is running in.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DaemonMode {
    /// SYSTEM service — full node (can *be accessed* + *reach others*).
    Service,
    /// Unprivileged user session — *reach others* only, no admin.
    User,
}

/// P5 exit-node — this node's default-route-egress status (Tailscale "use exit
/// node"). Published by the overlay runtime so `roomler status` / the desktop can
/// show whether this node is routing its internet egress through a mesh peer and,
/// crucially, WHY it isn't when it's configured but not active — the split-tunnel
/// signal (S4): the client withholds default routing rather than self-wedge when
/// the exit peer is absent / uncarriered / unapproved, or a carrier-endpoint
/// exemption can't be pinned.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ExitNodeStatus {
    /// The configured exit-node selector (a peer name or node-id hex).
    pub selector: String,
    /// `true` when the split-default is installed — this node's internet egress
    /// currently routes through the exit peer.
    pub active: bool,
    /// Human-readable reason routing is NOT active while `active == false`
    /// (e.g. "exit node not visible in the mesh yet", "not an approved exit
    /// node", "carrier-endpoint exemption unavailable"). `None` when `active`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub withheld_reason: Option<String>,
    /// P5/S3b — `true` when GLOBAL IPv6 egress ALSO routes through the exit. When
    /// `active` but this is `false`, v6 is fail-closed (blackholed, never leaked)
    /// — the exit is v6-incapable (e.g. Windows: WinNAT has no v6) or this host
    /// has no v6 uplink to exempt the coordination server. Defaults false for
    /// back-compat with a daemon/CLI that predates v6 egress.
    #[serde(default)]
    pub v6_active: bool,
    /// P5/S4b — `true` when the DEFAULT ("." catch-all) DNS namespace is steered
    /// through the exit, so every non-overlay query resolves from the exit's
    /// vantage (no DNS leak to the local ISP resolver). When `active` but this is
    /// `false`, DNS is NOT steered — the local overlay resolver failed to bind, or
    /// `resolvectl`/NRPT is unavailable — and queries may resolve via the local
    /// uplink; surfaced so it's never a SILENT leak. Defaults false for back-compat
    /// with a daemon/CLI that predates DNS steering.
    #[serde(default)]
    pub dns_steered: bool,
}

/// Snapshot of the local node.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct NodeStatus {
    pub node_id: String,
    pub name: String,
    pub version: String,
    pub mode: DaemonMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overlay_ip: Option<String>,
    /// The node's *derived* overlay IPv6 (`fd72:6f6f:6d6c::<v4>`), published by
    /// the overlay runtime alongside the v4. Absent on a v4-only daemon.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overlay_ip6: Option<String>,
    /// Connected to the coordination server.
    pub connected: bool,
    /// P5 exit-node routing status. `None` unless this node is configured as an
    /// exit-node CLIENT (`overlay_exit_node`). Backward-compatible (older CLIs
    /// ignore the extra field; a v4-only / non-exit daemon omits it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_node: Option<ExitNodeStatus>,
    /// S1b — the config file this daemon actually LOADED. Ends the desktop's
    /// guessing game about which of the per-user / machine-global copies is
    /// live (their resolution ladders disagreed for plain-SCM installs).
    /// Additive; absent from older daemons.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_path: Option<String>,
    /// S2 — MagicDNS status, copied verbatim from [`OverlayView::dns`].
    /// `None` when the overlay is off, MagicDNS is off for the tenant, or
    /// the daemon predates the field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dns: Option<DnsStatus>,
}

/// S2 — the MagicDNS state the overlay runtime publishes: what the
/// desktop's DNS section and `roomler status` render. Tenant-level
/// domain administration stays server-side (web admin); these are the
/// per-node facts.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct DnsStatus {
    /// The magic domain in effect (e.g. `tenant.roomler.net`).
    pub magic_domain: String,
    /// The local resolver bound `<overlay-ip>:53` (the split-DNS target).
    pub resolver_bound: bool,
    /// The OS was successfully pointed at our resolver for the magic
    /// domain (split-DNS steer active; reverted on daemon exit).
    pub os_steer_active: bool,
    /// Upstream resolver non-overlay queries forward to.
    pub upstream: String,
    /// AAAA (derived overlay IPv6) answers are enabled.
    pub answer_aaaa: bool,
}

/// A peer device as this node currently sees it.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct PeerInfo {
    pub node_id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overlay_ip: Option<String>,
    /// The peer's *derived* overlay IPv6 (`fd72:6f6f:6d6c::<their-v4>`),
    /// published by the overlay runtime alongside the v4.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overlay_ip6: Option<String>,
    pub online: bool,
    pub connection: ConnectionType,
    /// P8-cosmetics — a make-before-break direct-upgrade probe is currently in
    /// flight for this (relay-carried) peer: the CLI renders `upgrading`
    /// instead of `relay`, so a snapshot taken mid-transition reads as what it
    /// is rather than contradicting observed latency. `false` from older
    /// daemons (serde default) and for non-relay peers.
    #[serde(default)]
    pub upgrading: bool,
    /// rc.275 honesty — the carrier is installed but SILENTLY ONE-WAY (no
    /// completed WG handshake past the warm-up grace, or the one-way strike
    /// counter accumulating). The CLI renders `stalled` instead of a
    /// healthy-looking `direct`/`relay`. `#[serde(default)]` — absent from a
    /// pre-rc.275 daemon ⇒ `false` (wire-compatible both ways).
    #[serde(default)]
    pub stalled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rtt_ms: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen_ms: Option<u64>,
    /// The `agents._id` (hex) backing this peer, when it's an agent node
    /// (P3b-3). Carried from the netmap so the daemon can join this peer to a
    /// daemon-originated tunnel flow (keyed by agent id) and label it
    /// `ConnectionType::Tunnel`. `None` for a tunnel-client node / pre-P3b-3
    /// runtime. Not a display column — a join key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    /// rc.187 — for a RELAY peer: our own coturn-relayed address and the peer's
    /// relayed address we dial (both `host:port`). `None` for direct/blocked/
    /// offline. Lets `peers --json` show which coturn worker each end pinned —
    /// same-worker (the two IPs match) vs cross-worker — without a debug-log hunt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay_local: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay_dst: Option<String>,
    /// rc.276 diagnostics — the installed carrier's forensic snapshot (which
    /// flow each peer rides + whether it actually works), for `peers --json`
    /// only. `None` for a peer with no installed carrier or from a pre-rc.276
    /// daemon (wire-compatible both ways).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub debug: Option<PeerCarrierDebug>,
}

/// rc.276 — per-carrier forensic fields (see [`PeerInfo::debug`]). Built for
/// the pc50045-class corp-VPN investigations: two `peers --json` snapshots
/// 30 s apart show exactly which carriers move which counters, which flows
/// were initiated by whom, and whether the WG session ever completed —
/// one-shot field captures instead of multi-session log archaeology.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct PeerCarrierDebug {
    /// Carrier tier: `lan` / `public` / `srflx` / `relay`.
    pub tier: String,
    /// `true` = we initiated this flow (outbound dial / our allocation);
    /// `false` = adopted from the peer's authenticated inbound dial.
    pub initiated: bool,
    /// The WG session latch (either role) as of the last health sweep.
    pub hs_done: bool,
    /// Carrier socket's local address (relay: our relayed address).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local: Option<String>,
    /// Carrier send destination (direct: dial dst / accepted src; relay: the
    /// peer's relayed address).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dst: Option<String>,
    /// IP-data packets sent / received over this carrier (handshakes and
    /// keepalives touch NEITHER — `tx>0, rx==0` past the grace = one-way).
    pub tx: u64,
    pub rx: u64,
    /// Seconds since we last heard ANY authenticated packet (keepalives
    /// included) from this peer.
    pub last_rx_age_s: u64,
    /// Relay flavor (`turn` / `derp`); `None` for direct carriers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay_kind: Option<String>,
    /// P4 — inbound packets from this peer the ingress ACL refused: a forged
    /// SOURCE, a DESTINATION outside this node's advertised scope, or a
    /// cidr/port/proto the tenant's policies didn't grant. Monotonic since the
    /// carrier was installed.
    ///
    /// Under the default `overlay_rpf=warn` NOTHING is dropped, so a non-zero
    /// value is pure evidence: it is exactly what `enforce` WOULD have killed.
    /// Read this before flipping. Omitted when 0 so a healthy fleet's
    /// `peers --json` stays unchanged.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub rx_denied: u64,
}

fn is_zero_u64(v: &u64) -> bool {
    *v == 0
}

/// Whether a forward is a static `--remote` forward or a SOCKS5 listener.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FlowKind {
    Forward,
    Socks5,
}

/// One active forward / SOCKS5 listener with cumulative throughput. Sourced
/// from the per-flow `forward::FlowStats` the data plane already records.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct FlowInfo {
    pub id: String,
    pub kind: FlowKind,
    pub local_addr: String,
    /// `host:port` for a static forward; `None` for a SOCKS5 listener (its
    /// target is chosen per connection).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// Peer node this forward reaches (name or id).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    pub transport: String,
    pub active_flows: u32,
    pub bytes_in: u64,
    pub bytes_out: u64,
}

/// A DECLARED, daemon-supervised route (P6). Unlike an ephemeral flow
/// (created over the LocalAPI, gone when the daemon restarts), a route is
/// persisted in the daemon's config (`[[tunnel_routes]]` — this struct IS
/// the config-TOML shape, one type for wire + disk) and reconciled back
/// into a live flow on every daemon start until removed or disabled.
///
/// `node` is REQUIRED in v1: daemon-side SOCKS5 needs a concrete target
/// node (mesh-mode routes are a later slice, matching the CLI's own
/// "--daemon socks5 requires --agent" bail).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct RouteDescriptor {
    /// Operator-chosen slug, unique among routes. Empty on `RouteAdd` ⇒
    /// the daemon generates one (returned in [`Response::RouteAdded`]).
    #[serde(default)]
    pub id: String,
    pub kind: FlowKind,
    /// Target node (hex agent id).
    pub node: String,
    /// Local loopback listen port.
    pub local: u16,
    /// `host:port` for a static forward; `None` for a SOCKS5 route.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,
    /// `auto` (default) | `quic` | `webrtc`. Empty ⇒ `auto`.
    #[serde(default)]
    pub transport: String,
    /// Soft-disable without deletion. Defaults true so a hand-written
    /// config entry without the key is live.
    #[serde(default = "route_enabled_default")]
    pub enabled: bool,
}

fn route_enabled_default() -> bool {
    true
}

/// Runtime state of a declared route, computed by the daemon's reconciler.
/// `Failed` is TERMINAL: a permanent open-failure (enrollment revoked,
/// cross-tenant, ACL policy deny) stops supervision for the route until an
/// operator re-enables it — without this, a revoked route would hammer the
/// server with a TunnelOpen every backoff tick, across reboots, forever.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum RouteState {
    /// `enabled = false` — not supervised.
    Disabled,
    /// Declared and enabled; the reconciler hasn't (re)created its flow yet.
    Pending,
    /// Live flow exists.
    Active { flow_id: String },
    /// Flow creation failed retryably (port taken, WS down); the reconciler
    /// retries with backoff.
    Backoff {
        next_retry_secs: u64,
        last_error: String,
    },
    /// Permanent failure — requires operator re-enable (or remove).
    Failed { reason: String },
}

/// A declared route joined with its live runtime state — the
/// [`Request::RouteList`] row.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct RouteInfo {
    pub route: RouteDescriptor,
    pub state: RouteState,
}

/// One remote-control session awaiting an operator consent decision (rc.46).
/// Surfaced by [`Request::ConsentPending`] so the desktop app renders its
/// Approve/Deny modal over the LocalAPI instead of reading the daemon's private
/// sentinel dir — which lives in the daemon's profile and is unreachable to the
/// interactive-user app when the daemon runs as SYSTEM (P2b bug fix).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ConsentRequest {
    pub session_id: String,
    #[serde(default)]
    pub controller_name: String,
    /// Pipe-separated permission names (the agent's `Permissions` serde form).
    #[serde(default)]
    pub permissions: String,
    #[serde(default)]
    pub timeout_secs: u64,
}

/// A LocalAPI request. P1 exposed read-only verbs; P2b adds the (mutating)
/// consent verbs. Adjacently tagged on `t`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "t", content = "d", rename_all = "snake_case")]
pub enum Request {
    /// Local node status.
    Status,
    /// Peers with their current connection type.
    Peers,
    /// Active forwards / SOCKS5 listeners + throughput.
    Flows,
    /// Remote-control sessions awaiting an operator consent decision.
    ConsentPending,
    /// Approve (`allow=true`) or deny a pending consent, by session id.
    ConsentDecide { session_id: String, allow: bool },
    /// ICMP-ping an overlay peer (by name or IP) over the userspace netstack —
    /// the OS-free reachability probe. `timeout_ms` 0 ⇒ the daemon's default.
    Ping {
        target: String,
        #[serde(default)]
        timeout_ms: u64,
        /// Resolve a *name* target to the peer's derived overlay IPv6 instead
        /// of its v4 (`roomler ping -6`). Ignored for literal-IP targets.
        #[serde(default)]
        prefer_v6: bool,
    },
    /// Create a daemon-driven static forward: the daemon opens a tunnel to
    /// `node` (a hex agent id) over its own agent WS and listens on `local`,
    /// dialing `remote` (`host:port`) from the target. Mutating — like
    /// [`Request::ConsentDecide`], the pipe/socket ACL is the trust boundary
    /// (P3b-2). Returns [`Response::FlowCreated`] with the assigned flow id.
    CreateForward {
        node: String,
        local: u16,
        remote: String,
        /// `auto` (default) | `quic` | `webrtc`. Empty ⇒ `auto`.
        #[serde(default)]
        transport: String,
    },
    /// Create a daemon-driven SOCKS5 listener toward `node` (userspace mode —
    /// per-connection CONNECT target, no OS routing). Returns
    /// [`Response::FlowCreated`].
    CreateSocks5 {
        node: String,
        local: u16,
        #[serde(default)]
        transport: String,
    },
    /// Stop + deregister a daemon flow by its id. Returns
    /// [`Response::FlowKilled`] (`ok=false` if the id was unknown).
    KillFlow { id: String },
    /// Declared routes + their live runtime state (P6). Read-only.
    RouteList,
    /// Declare a new supervised route: the daemon persists it to config
    /// (surviving restarts) and reconciles it into a live flow. Mutating —
    /// the pipe/socket ACL is the trust boundary, same as
    /// [`Request::CreateForward`]. Returns [`Response::RouteAdded`] with
    /// the effective descriptor (id generated if empty).
    RouteAdd { route: RouteDescriptor },
    /// Remove a declared route by id: kills its live flow (if any) and
    /// deletes it from config. Returns [`Response::RouteRemoved`]
    /// (`ok=false` if the id was unknown).
    RouteRemove { id: String },
    /// Enable/disable a declared route by id without deleting it.
    /// Disabling kills the live flow; enabling clears a terminal
    /// `Failed` state and re-supervises. Returns
    /// [`Response::RouteUpdated`].
    RouteSetEnabled { id: String, enabled: bool },
    /// Rename this device: the daemon persists the new `machine_name` to ITS
    /// OWN config file — profile-correct by construction (under a SYSTEM/SCM
    /// install the machine-global config is writable by the daemon but NOT by
    /// an unelevated desktop app / CLI doing a direct file write). Mutating —
    /// the pipe/socket ACL is the trust boundary, like [`Request::RouteAdd`].
    /// The new name is announced on the next server reconnect. Returns
    /// [`Response::DeviceNameSet`] with the effective (trimmed) name.
    SetDeviceName { name: String },
    /// S1b — archive the STALE config copy on a host carrying both a
    /// per-user and a machine-global `config.toml` (the desktop's
    /// "Two configurations found" banner). The DAEMON does the work —
    /// it knows which copy it loaded and has the rights (an unelevated
    /// desktop app can't touch `%PROGRAMDATA%`). Guarded: both copies
    /// must parse, carry the SAME agent identity, and the stale copy is
    /// renamed aside (`config.toml.stale-<ts>`), never deleted. Mutating —
    /// the pipe/socket ACL is the trust boundary, like
    /// [`Request::SetDeviceName`]. Returns [`Response::ConfigCleaned`].
    ConfigCleanupStale,
    /// S2 — list every EDITABLE config key with its current value +
    /// editor metadata (the daemon reads its own config file, so pending
    /// not-yet-restarted edits show). Secrets are excluded by
    /// construction. Returns [`Response::ConfigEntries`].
    ConfigGet,
    /// S2 — set (or clear, `value: None`) one editable config key. The
    /// daemon validates per key, persists through its own config path +
    /// write lock (profile-correct under SYSTEM), and echoes the updated
    /// entry. Mutating — the pipe/socket ACL is the trust boundary.
    /// Returns [`Response::ConfigUpdated`] or [`Response::Error`].
    ConfigSet {
        key: String,
        #[serde(default)]
        value: Option<String>,
    },
    /// S2 — read the tail of one of the daemon's log files. `source` is
    /// `daemon` (this process's active rolling log), `service` (the
    /// machine-global SCM host log), or `panic` (the newest panic dump).
    /// `max_bytes` is clamped daemon-side (≤64 KiB) — bounded response,
    /// poll-based follow (no streaming). Returns [`Response::LogTail`].
    TailLog {
        source: String,
        #[serde(default)]
        max_bytes: Option<u64>,
    },
}

/// One editable config entry (S2 config surface). Values travel as
/// strings — bools as `"true"`/`"false"`, lists comma-separated,
/// structured keys as JSON; `kind` tells the client which editor to
/// render. Secrets are never in this surface.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ConfigEntry {
    pub key: String,
    /// Current value; `None` = unset (built-in default applies).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    /// Editor hint: `bool` | `tribool` (unset/on/off) | `string` |
    /// `enum:<a|b|c>` | `list` (comma-separated) | `json`.
    pub kind: String,
    /// The change only takes effect after a daemon restart.
    pub restart_required: bool,
    /// One-line operator help.
    pub description: String,
}

/// A LocalAPI response. Adjacently tagged so a payload may be a struct
/// (`Status`) or a sequence (`Peers` / `Flows`).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "t", content = "d", rename_all = "snake_case")]
pub enum Response {
    Status(NodeStatus),
    Peers(Vec<PeerInfo>),
    Flows(Vec<FlowInfo>),
    /// Sessions awaiting a consent decision.
    ConsentPending(Vec<ConsentRequest>),
    /// Result of a [`Request::ConsentDecide`] — `ok` = the decision was recorded.
    ConsentDecided {
        ok: bool,
    },
    /// Round-trip result of [`Request::Ping`] — the resolved overlay IP + RTT.
    Pong {
        target: String,
        overlay_ip: String,
        /// Round-trip time in microseconds. Integer keeps the wire type `Eq`;
        /// the client renders it as milliseconds.
        rtt_micros: u64,
    },
    /// A forward / SOCKS5 listener was created — carries its assigned flow id
    /// (usable with [`Request::KillFlow`] + shown by [`Request::Flows`]).
    FlowCreated {
        id: String,
    },
    /// Result of [`Request::KillFlow`] — `ok=false` if the id wasn't found.
    FlowKilled {
        ok: bool,
    },
    /// Declared routes + live state ([`Request::RouteList`]).
    Routes(Vec<RouteInfo>),
    /// A route was declared + persisted — carries the effective descriptor
    /// (id filled in if the request left it empty).
    RouteAdded {
        route: RouteDescriptor,
    },
    /// Result of [`Request::RouteRemove`] — `ok=false` if the id was unknown.
    RouteRemoved {
        ok: bool,
    },
    /// Result of [`Request::RouteSetEnabled`] — `ok=false` if the id was
    /// unknown.
    RouteUpdated {
        ok: bool,
    },
    /// The device was renamed + persisted ([`Request::SetDeviceName`]) —
    /// carries the effective (trimmed) name.
    DeviceNameSet {
        name: String,
    },
    /// Result of [`Request::ConfigCleanupStale`] — `ok=true` means the
    /// stale copy was archived; `detail` explains either outcome
    /// (archived-to path, or why nothing was touched).
    ConfigCleaned {
        ok: bool,
        detail: String,
    },
    /// The editable config surface ([`Request::ConfigGet`]).
    ConfigEntries(Vec<ConfigEntry>),
    /// One key was set/cleared + persisted ([`Request::ConfigSet`]) —
    /// echoes the updated entry (normalized value + restart flag).
    ConfigUpdated {
        entry: ConfigEntry,
    },
    /// The tail of a daemon log file ([`Request::TailLog`]). `size` is
    /// the file's TOTAL size — a client polls it to detect growth and
    /// re-request; `content` starts on a line boundary when the tail cut
    /// mid-line (lossy UTF-8).
    LogTail {
        path: String,
        size: u64,
        content: String,
    },
    /// The verb couldn't be served (bad request, state unavailable).
    Error {
        message: String,
    },
}

/// The overlay runtime's live view of the mesh, republished on a
/// [`tokio::sync::watch`] channel whenever the netmap / carrier state changes
/// (see `overlay::runtime`). NOT a wire type — it's the daemon-internal bridge
/// between the overlay runtime (which owns `by_node` inside its single
/// `select!` loop and has no other external accessor) and the daemon's
/// [`LocalApiState`] impl, which turns it into [`Response::Status`]'s
/// `overlay_ip` + [`Response::Peers`]. Kept here (not under the overlay
/// feature) so a daemon compiled WITHOUT `overlay-l3` can still hold an empty
/// `Default` one and answer `peers` with `[]`.
#[derive(Debug, Clone, Default)]
pub struct OverlayView {
    /// This node's assigned overlay IP (the netmap `self_ip`), once joined.
    pub self_ip: Option<String>,
    /// This node's *derived* overlay IPv6, filled by the overlay runtime
    /// alongside `self_ip` (the daemon never derives — it has no `overlay`
    /// feature guarantee).
    pub self_ip6: Option<String>,
    /// Peers as the runtime currently reaches them.
    pub peers: Vec<PeerInfo>,
    /// P5 exit-node routing status (S4), filled by the overlay runtime. `None`
    /// unless this node is configured as an exit-node client. The daemon copies
    /// it verbatim into [`NodeStatus::exit_node`].
    pub exit_node: Option<ExitNodeStatus>,
    /// S2 — MagicDNS status, filled by the overlay runtime after its DNS
    /// bring-up. `None` when MagicDNS is off. The daemon copies it
    /// verbatim into [`NodeStatus::dns`].
    pub dns: Option<DnsStatus>,
}

/// Read-only snapshot the daemon provides to [`handle`]. The daemon's impl
/// gathers this from its live overlay / tunnel / forward state; the trait keeps
/// the protocol unit-testable with a mock and free of daemon internals.
#[async_trait]
pub trait LocalApiState: Send + Sync {
    fn status(&self) -> NodeStatus;
    fn peers(&self) -> Vec<PeerInfo>;
    fn flows(&self) -> Vec<FlowInfo>;
    /// Remote-control sessions awaiting an operator consent decision (P2b).
    /// Default: none — so existing impls / mocks and the read-only contract are
    /// undisturbed; the agent daemon overrides this.
    fn consent_pending(&self) -> Vec<ConsentRequest> {
        Vec::new()
    }
    /// Apply an operator consent decision to `session_id` (P2b). Returns whether
    /// it was recorded. Default: no-op `false`.
    fn consent_decide(&self, _session_id: &str, _allow: bool) -> bool {
        false
    }
    /// ICMP-ping an overlay peer by name/IP over the userspace netstack, and
    /// return a [`Response::Pong`] (or [`Response::Error`]). Async — awaited by
    /// [`serve_connection`], not the sync [`handle`]. `prefer_v6` resolves a
    /// name target to the peer's derived overlay IPv6. Default: unsupported (a
    /// node not running the netstack has no OS-free ICMP path).
    async fn ping(&self, _target: &str, _timeout_ms: u64, _prefer_v6: bool) -> Response {
        Response::Error {
            message: "ping is not supported on this node (not running the userspace netstack)"
                .into(),
        }
    }
    /// Create a daemon-driven static forward (P3b-2). Async — awaited by
    /// [`serve_connection`], not the sync [`handle`]. Returns
    /// [`Response::FlowCreated`] or [`Response::Error`]. Default: unsupported
    /// (a node that can't originate tunnels, e.g. no agent WS).
    async fn create_forward(
        &self,
        _node: &str,
        _local: u16,
        _remote: &str,
        _transport: &str,
    ) -> Response {
        Response::Error {
            message: "forward origination is not supported on this node".into(),
        }
    }
    /// Create a daemon-driven SOCKS5 listener (P3b-2). Async; default
    /// unsupported.
    async fn create_socks5(&self, _node: &str, _local: u16, _transport: &str) -> Response {
        Response::Error {
            message: "socks5 origination is not supported on this node".into(),
        }
    }
    /// Stop + deregister a daemon flow by id (P3b-2). Returns whether a flow was
    /// found + killed. Default: no-op `false`.
    fn kill_flow(&self, _id: &str) -> bool {
        false
    }
    /// Declared routes + their live runtime state (P6). Default: none —
    /// a node without a route reconciler has no declared routes.
    fn route_list(&self) -> Vec<RouteInfo> {
        Vec::new()
    }
    /// Declare + persist a supervised route (P6). Async — config I/O.
    /// Default: unsupported.
    async fn route_add(&self, _route: RouteDescriptor) -> Response {
        Response::Error {
            message: "declared routes are not supported on this node".into(),
        }
    }
    /// Remove a declared route by id (P6). Async — config I/O. Default:
    /// unsupported.
    async fn route_remove(&self, _id: &str) -> Response {
        Response::Error {
            message: "declared routes are not supported on this node".into(),
        }
    }
    /// Enable/disable a declared route by id (P6). Async — config I/O.
    /// Default: unsupported.
    async fn route_set_enabled(&self, _id: &str, _enabled: bool) -> Response {
        Response::Error {
            message: "declared routes are not supported on this node".into(),
        }
    }
    /// Rename this device — persist the new name to the daemon's own config
    /// (async — config I/O). Default: unsupported, so existing impls / mocks
    /// and the read-only contract are undisturbed; the agent daemon overrides.
    async fn set_device_name(&self, _name: &str) -> Response {
        Response::Error {
            message: "renaming is not supported on this node".into(),
        }
    }
    /// S1b — archive the stale config copy on a split-config host
    /// (async — config I/O). Default: unsupported; the agent daemon
    /// overrides.
    async fn config_cleanup_stale(&self) -> Response {
        Response::Error {
            message: "config cleanup is not supported on this node".into(),
        }
    }
    /// S2 — list the editable config surface (async — reads the config
    /// file). Default: unsupported; the agent daemon overrides.
    async fn config_entries(&self) -> Response {
        Response::Error {
            message: "config editing is not supported on this node".into(),
        }
    }
    /// S2 — set/clear one editable config key (async — config I/O).
    /// Default: unsupported; the agent daemon overrides.
    async fn config_set(&self, _key: &str, _value: Option<&str>) -> Response {
        Response::Error {
            message: "config editing is not supported on this node".into(),
        }
    }
    /// S2 — tail a daemon log file (async — file I/O). Default:
    /// unsupported; the agent daemon overrides.
    async fn tail_log(&self, _source: &str, _max_bytes: Option<u64>) -> Response {
        Response::Error {
            message: "log tailing is not supported on this node".into(),
        }
    }
}

/// Pure dispatch: map a [`Request`] to a [`Response`] over a state snapshot.
/// No I/O — the pipe listener (P1-cont) reads a JSON line, deserialises a
/// [`Request`], calls this, and writes the [`Response`] back.
pub fn handle(req: &Request, state: &dyn LocalApiState) -> Response {
    match req {
        Request::Status => Response::Status(state.status()),
        Request::Peers => Response::Peers(state.peers()),
        Request::Flows => Response::Flows(state.flows()),
        Request::ConsentPending => Response::ConsentPending(state.consent_pending()),
        Request::ConsentDecide { session_id, allow } => Response::ConsentDecided {
            ok: state.consent_decide(session_id, *allow),
        },
        Request::KillFlow { id } => Response::FlowKilled {
            ok: state.kill_flow(id),
        },
        Request::RouteList => Response::Routes(state.route_list()),
        // `Ping` / `Create*` / the mutating `Route*` verbs are async —
        // intercepted in `serve_connection` before this sync dispatch runs.
        // These arms only satisfy match exhaustiveness.
        Request::Ping { .. }
        | Request::CreateForward { .. }
        | Request::CreateSocks5 { .. }
        | Request::RouteAdd { .. }
        | Request::RouteRemove { .. }
        | Request::RouteSetEnabled { .. }
        | Request::SetDeviceName { .. }
        | Request::ConfigCleanupStale
        | Request::ConfigGet
        | Request::ConfigSet { .. }
        | Request::TailLog { .. } => Response::Error {
            message: "this verb must be served on the async path".into(),
        },
    }
}

/// Serve one LocalAPI client connection to completion: read
/// newline-delimited JSON [`Request`]s, [`handle`] each against `state`,
/// write the newline-delimited JSON [`Response`] back, and loop until the
/// client closes the stream (EOF). A line that isn't a valid `Request`
/// gets an [`Response::Error`] and the connection stays open (so a client
/// can recover). **Transport-agnostic** — the platform listeners (Windows
/// named pipe with an ACL'd security descriptor, unix socket; P1-cont)
/// accept a connection and hand the accepted stream here. The daemon
/// spawns one task per connection: `serve_connection(stream, state.as_ref())`.
pub async fn serve_connection<S>(stream: S, state: &dyn LocalApiState) -> std::io::Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let (rd, mut wr) = tokio::io::split(stream);
    let mut lines = tokio::io::BufReader::new(rd).lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let resp = match serde_json::from_str::<Request>(&line) {
            // The async verbs — await them here; everything else is a pure sync
            // dispatch through `handle`.
            Ok(Request::Ping {
                target,
                timeout_ms,
                prefer_v6,
            }) => state.ping(&target, timeout_ms, prefer_v6).await,
            Ok(Request::CreateForward {
                node,
                local,
                remote,
                transport,
            }) => {
                state
                    .create_forward(&node, local, &remote, &transport)
                    .await
            }
            Ok(Request::CreateSocks5 {
                node,
                local,
                transport,
            }) => state.create_socks5(&node, local, &transport).await,
            Ok(Request::RouteAdd { route }) => state.route_add(route).await,
            Ok(Request::RouteRemove { id }) => state.route_remove(&id).await,
            Ok(Request::RouteSetEnabled { id, enabled }) => {
                state.route_set_enabled(&id, enabled).await
            }
            Ok(Request::SetDeviceName { name }) => state.set_device_name(&name).await,
            Ok(Request::ConfigCleanupStale) => state.config_cleanup_stale().await,
            Ok(Request::ConfigGet) => state.config_entries().await,
            Ok(Request::ConfigSet { key, value }) => state.config_set(&key, value.as_deref()).await,
            Ok(Request::TailLog { source, max_bytes }) => state.tail_log(&source, max_bytes).await,
            Ok(req) => handle(&req, state),
            Err(e) => Response::Error {
                message: format!("bad request: {e}"),
            },
        };
        // A Response always serialises; fall back to an Error line if a
        // custom serializer ever failed, so we never break the frame.
        let mut out = serde_json::to_vec(&resp).unwrap_or_else(|e| {
            serde_json::to_vec(&Response::Error {
                message: format!("encode error: {e}"),
            })
            .expect("Error response always serialises")
        });
        out.push(b'\n');
        wr.write_all(&out).await?;
        wr.flush().await?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Platform listener (unification P1-cont)
//
// The daemon (`roomlerd` / today's `roomler-agent`) calls [`serve`] once at
// startup. It binds the local-only control endpoint — a named pipe on Windows,
// a unix socket elsewhere — restricts it to trusted local principals via the
// pipe/socket ACL (no token: the OS enforces WHO can connect), and serves each
// accepted connection with [`serve_connection`]. Returns when `shutdown` flips
// true (or on a fatal bind error, which the daemon logs without dying).
// ---------------------------------------------------------------------------

/// Bind the platform LocalAPI endpoint and serve clients until `shutdown`
/// fires. Auth is the endpoint ACL, not a token:
/// - **Windows**: named pipe `\\.\pipe\roomler` with a security descriptor
///   granting only `SYSTEM`, `Administrators`, and the interactive user — so a
///   low-privilege local process can't read node state (and, once mutating
///   verbs land in P2, can't drive the daemon).
/// - **Unix**: socket at `$XDG_RUNTIME_DIR/roomler.sock` (per-user, 0700 dir),
///   chmod `0600` — owner-only.
///
/// Each accepted connection is served on its own task; a slow or misbehaving
/// client can't stall the accept loop or another client.
pub async fn serve(
    state: Arc<dyn LocalApiState>,
    shutdown: watch::Receiver<bool>,
) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        serve_windows(state, shutdown).await
    }
    #[cfg(not(windows))]
    {
        serve_unix(state, shutdown).await
    }
}

// ---- Windows: named pipe + SDDL security descriptor ----------------------

/// The LocalAPI named-pipe path. Fixed name so thin clients (CLI, desktop app)
/// know where to connect.
#[cfg(windows)]
const LOCALAPI_PIPE_NAME: &str = r"\\.\pipe\roomler";

/// SDDL for the pipe. DACL: allow (`A`) generic-all (`GA`) to Local `SY`stem,
/// `B`uiltin `A`dministrators, and `I`nteractive `U`sers — and, by omission,
/// deny everyone else. IU covers the desktop app / CLI running in the operator's
/// interactive session (including a non-elevated admin, whose Administrators SID
/// is deny-only but who still matches IU). No OWNER is set — a user-mode daemon
/// can't assign one it doesn't hold, and the creator is a valid owner anyway.
///
/// SACL `S:(ML;;NW;;;ME)` — a mandatory-integrity label at **Medium** with
/// No-Write-Up: a process **below** medium integrity (an AppContainer / sandboxed
/// browser child / low-IL malware) can't write to the pipe, so it can't send a
/// request at all — hardening the (mutating) consent verb against a low-IL
/// caller (P2b security review H1). The interactive user's tray + CLI run at
/// medium IL and SYSTEM above it, so both are unaffected. Setting a label at or
/// below the creator's own IL needs no privilege.
#[cfg(windows)]
const LOCALAPI_SDDL: &str = "D:(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;IU)S:(ML;;NW;;;ME)";

/// `SDDL_REVISION_1` — the only defined SDDL revision.
#[cfg(windows)]
const SDDL_REVISION_1: u32 = 1;

/// A security descriptor built from an SDDL string plus the
/// `SECURITY_ATTRIBUTES` that `create_with_security_attributes_raw` consumes.
/// Owns the `LocalAlloc`'d descriptor and `LocalFree`s it on drop.
#[cfg(windows)]
struct PipeSecurity {
    sa: windows_sys::Win32::Security::SECURITY_ATTRIBUTES,
    psd: windows_sys::Win32::Security::PSECURITY_DESCRIPTOR,
}

// SAFETY: the security descriptor is a plain `LocalAlloc`'d heap buffer with no
// thread affinity — moving ownership of `PipeSecurity` (and thus the pointer)
// to another thread and calling `LocalFree` there is sound. Needed because the
// accept loop holds it across `.await`, so `localapi::serve`'s future must be
// `Send` for `tokio::spawn`.
#[cfg(windows)]
unsafe impl Send for PipeSecurity {}

#[cfg(windows)]
impl PipeSecurity {
    fn new(sddl: &str) -> std::io::Result<Self> {
        use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
        use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};

        let wide: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0)).collect();
        let mut psd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        // SAFETY: `wide` is a NUL-terminated UTF-16 buffer; `psd` is a valid
        // out-pointer; the size-out argument is null (documented optional).
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide.as_ptr(),
                SDDL_REVISION_1,
                &mut psd,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: psd,
            bInheritHandle: 0,
        };
        Ok(Self { sa, psd })
    }

    /// Pointer to the `SECURITY_ATTRIBUTES`, valid while `self` lives. The OS
    /// copies the descriptor when the pipe instance is created, so reusing this
    /// across instances is fine.
    fn as_ptr(&mut self) -> *mut core::ffi::c_void {
        &raw mut self.sa as *mut core::ffi::c_void
    }
}

#[cfg(windows)]
impl Drop for PipeSecurity {
    fn drop(&mut self) {
        if !self.psd.is_null() {
            // SAFETY: `psd` was allocated by ConvertStringSecurityDescriptor…
            // (LocalAlloc); LocalFree is the documented release.
            unsafe {
                windows_sys::Win32::Foundation::LocalFree(self.psd as _);
            }
        }
    }
}

#[cfg(windows)]
async fn serve_windows(
    state: Arc<dyn LocalApiState>,
    shutdown: watch::Receiver<bool>,
) -> std::io::Result<()> {
    serve_windows_at(LOCALAPI_PIPE_NAME, state, shutdown).await
}

/// The named-pipe accept loop, parameterised on the pipe name so tests can use
/// a private one. Builds the ACL once, then serves clients: on each connect it
/// hands the connected instance to a task and pre-creates the next instance so
/// a second client racing the handoff isn't refused with `ERROR_PIPE_BUSY`.
#[cfg(windows)]
pub(crate) async fn serve_windows_at(
    pipe_name: &str,
    state: Arc<dyn LocalApiState>,
    mut shutdown: watch::Receiver<bool>,
) -> std::io::Result<()> {
    use tokio::net::windows::named_pipe::ServerOptions;

    if *shutdown.borrow() {
        return Ok(());
    }
    let mut security = PipeSecurity::new(LOCALAPI_SDDL)?;
    // Retry the FIRST-instance create instead of failing permanently. If another
    // process already holds `\\.\pipe\roomler` (a stale/leftover agent, or a
    // rogue squatter), a one-shot bind would leave the LocalAPI dead until the
    // daemon restarts — and a squatter keeps feeding thin clients FAKE data
    // (field-observed: a leftover test server made the tray show mock peers +
    // no consent prompts). Retrying every 30 s self-heals the moment the pipe
    // frees, with a loud warning so the operator sees the contention.
    //
    // SAFETY: `security.as_ptr()` stays valid for the lifetime of `security`,
    // which outlives every create call below.
    let mut server = loop {
        match unsafe {
            ServerOptions::new()
                .first_pipe_instance(true)
                .create_with_security_attributes_raw(pipe_name, security.as_ptr())
        } {
            Ok(s) => break s,
            Err(e) => {
                tracing::warn!(
                    pipe = pipe_name, error = %e,
                    "localapi: pipe bind failed — another process may hold the pipe; retrying in 30s"
                );
                tokio::select! {
                    biased;
                    _ = shutdown.changed() => {
                        if *shutdown.borrow() { return Ok(()); }
                    }
                    _ = tokio::time::sleep(std::time::Duration::from_secs(30)) => {}
                }
            }
        }
    };
    tracing::info!(
        pipe = pipe_name,
        "localapi: named-pipe listener up (SYSTEM + Administrators + interactive user)"
    );

    loop {
        tokio::select! {
            biased;
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    tracing::info!("localapi: shutdown; pipe listener exiting");
                    return Ok(());
                }
            }
            conn = server.connect() => match conn {
                Ok(()) => {
                    let connected = server;
                    // SAFETY: same invariant as the first create.
                    server = unsafe {
                        ServerOptions::new()
                            .create_with_security_attributes_raw(pipe_name, security.as_ptr())?
                    };
                    let st = state.clone();
                    tokio::spawn(async move {
                        if let Err(e) = serve_connection(connected, &*st).await {
                            tracing::debug!(error = %e, "localapi: pipe client ended");
                        }
                    });
                }
                Err(e) => {
                    tracing::warn!(error = %e, "localapi: pipe connect failed; retrying");
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                }
            }
        }
    }
}

// ---- Unix: socket in the per-user runtime dir, chmod 0600 -----------------

/// The LocalAPI socket file name (under the per-user runtime dir).
#[cfg(unix)]
const LOCALAPI_SOCKET_NAME: &str = "roomler.sock";

/// Resolve the LocalAPI socket path: `$XDG_RUNTIME_DIR/roomler.sock` when the
/// per-user runtime dir is set (systemd guarantees it's 0700 + user-owned — the
/// right home for a control socket), else a `roomler/` subdir under the temp
/// dir (locked to 0700 by the listener). The socket itself is chmod 0600.
#[cfg(unix)]
pub(crate) fn unix_socket_path() -> std::path::PathBuf {
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        return std::path::PathBuf::from(dir).join(LOCALAPI_SOCKET_NAME);
    }
    // No runtime dir (common on macOS, where temp_dir() is already per-user).
    std::env::temp_dir()
        .join("roomler")
        .join(LOCALAPI_SOCKET_NAME)
}

#[cfg(unix)]
async fn serve_unix(
    state: Arc<dyn LocalApiState>,
    shutdown: watch::Receiver<bool>,
) -> std::io::Result<()> {
    serve_unix_at(unix_socket_path(), state, shutdown).await
}

/// The unix-socket accept loop, parameterised on the path so tests can use a
/// private one.
#[cfg(unix)]
pub(crate) async fn serve_unix_at(
    path: std::path::PathBuf,
    state: Arc<dyn LocalApiState>,
    mut shutdown: watch::Receiver<bool>,
) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    use tokio::net::UnixListener;

    if *shutdown.borrow() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        // Lock the parent to owner-only when we own it (the temp-subdir case);
        // for $XDG_RUNTIME_DIR this is already true and the chmod is harmless
        // (ignored if we don't own it).
        let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
    }
    // A stale socket from an unclean exit makes bind fail with EADDRINUSE.
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    // Owner-only: no other local user can open the control socket.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    tracing::info!(path = %path.display(), "localapi: unix-socket listener up (0600)");

    loop {
        tokio::select! {
            biased;
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    let _ = std::fs::remove_file(&path);
                    tracing::info!("localapi: shutdown; unix listener exiting");
                    return Ok(());
                }
            }
            accept = listener.accept() => match accept {
                Ok((stream, _addr)) => {
                    let st = state.clone();
                    tokio::spawn(async move {
                        if let Err(e) = serve_connection(stream, &*st).await {
                            tracing::debug!(error = %e, "localapi: unix client ended");
                        }
                    });
                }
                Err(e) => {
                    tracing::warn!(error = %e, "localapi: unix accept failed; retrying");
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Client (unification P2)
//
// The thin clients — the CLI (`roomler`) and the desktop app — connect to the
// daemon's LocalAPI over the same platform endpoint the server binds and issue
// read-only requests. Lives in-module so it shares the endpoint constants
// (`LOCALAPI_PIPE_NAME` / `unix_socket_path`) and the wire types with the
// server: one source of truth, no re-declared pipe name.
// ---------------------------------------------------------------------------

/// A boxed local-endpoint stream (Windows named pipe or unix socket) — both are
/// `AsyncRead + AsyncWrite`, so the client is transport-agnostic like the
/// server's [`serve_connection`].
trait ClientStream: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> ClientStream for T {}

/// A connected LocalAPI client. Open with [`connect`], then issue requests one
/// at a time. The daemon serves multiple requests on one connection, so a
/// single `Client` is reused across a poll (e.g. `status()` then `peers()`).
pub struct Client {
    stream: tokio::io::BufReader<Box<dyn ClientStream>>,
}

/// Connect to the local daemon's LocalAPI endpoint (the fixed
/// `\\.\pipe\roomler` / `$XDG_RUNTIME_DIR/roomler.sock`). "Daemon not running"
/// surfaces as [`std::io::ErrorKind::NotFound`] — callers should render that as
/// "device service not running", not a hard failure.
pub async fn connect() -> std::io::Result<Client> {
    #[cfg(windows)]
    {
        connect_windows_at(LOCALAPI_PIPE_NAME).await
    }
    #[cfg(not(windows))]
    {
        connect_unix_at(unix_socket_path()).await
    }
}

/// Named-pipe connect, parameterised on the pipe name so tests can target a
/// private one. Retries ONCE on `ERROR_PIPE_BUSY` (the server is momentarily
/// between instances — it pre-creates the next on each accept, but there's a
/// sub-ms window); any other error (notably `ERROR_FILE_NOT_FOUND` = daemon not
/// running) propagates immediately. No multi-second wait — this is an
/// interactive path.
#[cfg(windows)]
pub(crate) async fn connect_windows_at(pipe_name: &str) -> std::io::Result<Client> {
    use tokio::net::windows::named_pipe::ClientOptions;
    const ERROR_PIPE_BUSY: i32 = 231;
    let mut retried = false;
    let pipe = loop {
        match ClientOptions::new().open(pipe_name) {
            Ok(p) => break p,
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) && !retried => {
                retried = true;
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            Err(e) => return Err(e),
        }
    };
    Ok(Client::new(Box::new(pipe)))
}

/// Unix-socket connect, parameterised on the path so tests can target a private
/// one.
#[cfg(unix)]
pub(crate) async fn connect_unix_at(path: std::path::PathBuf) -> std::io::Result<Client> {
    let stream = tokio::net::UnixStream::connect(path).await?;
    Ok(Client::new(Box::new(stream)))
}

impl Client {
    fn new(stream: Box<dyn ClientStream>) -> Self {
        Self {
            stream: tokio::io::BufReader::new(stream),
        }
    }

    /// One newline-JSON round-trip — write the request, read one response line.
    /// Mirrors the server's [`serve_connection`] framing.
    pub async fn request(&mut self, req: &Request) -> std::io::Result<Response> {
        let mut buf = serde_json::to_vec(req).map_err(std::io::Error::other)?;
        buf.push(b'\n');
        self.stream.write_all(&buf).await?;
        self.stream.flush().await?;

        let mut line = String::new();
        if self.stream.read_line(&mut line).await? == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "localapi: connection closed before a response",
            ));
        }
        serde_json::from_str(line.trim_end()).map_err(std::io::Error::other)
    }

    /// `Request::Status` → [`NodeStatus`]. A `Response::Error` maps to `Err`.
    pub async fn status(&mut self) -> std::io::Result<NodeStatus> {
        match self.request(&Request::Status).await? {
            Response::Status(s) => Ok(s),
            other => Err(unexpected_response(other)),
        }
    }

    /// `Request::Peers` → the peer list with connection types.
    pub async fn peers(&mut self) -> std::io::Result<Vec<PeerInfo>> {
        match self.request(&Request::Peers).await? {
            Response::Peers(p) => Ok(p),
            other => Err(unexpected_response(other)),
        }
    }

    /// `Request::Flows` → active forwards / SOCKS5 listeners (empty on the agent
    /// daemon until the tunnel-client folds in at P3).
    pub async fn flows(&mut self) -> std::io::Result<Vec<FlowInfo>> {
        match self.request(&Request::Flows).await? {
            Response::Flows(f) => Ok(f),
            other => Err(unexpected_response(other)),
        }
    }

    /// `Request::Ping` → the resolved `(overlay_ip, rtt_ms)`. A daemon
    /// [`Response::Error`] (unknown peer / not a netstack node / timeout)
    /// surfaces its message verbatim.
    pub async fn ping(
        &mut self,
        target: &str,
        timeout_ms: u64,
        prefer_v6: bool,
    ) -> std::io::Result<(String, f64)> {
        let req = Request::Ping {
            target: target.to_string(),
            timeout_ms,
            prefer_v6,
        };
        match self.request(&req).await? {
            Response::Pong {
                overlay_ip,
                rtt_micros,
                ..
            } => Ok((overlay_ip, rtt_micros as f64 / 1000.0)),
            Response::Error { message } => Err(std::io::Error::other(message)),
            other => Err(unexpected_response(other)),
        }
    }

    /// `Request::ConsentPending` → remote-control sessions awaiting a decision.
    pub async fn consent_pending(&mut self) -> std::io::Result<Vec<ConsentRequest>> {
        match self.request(&Request::ConsentPending).await? {
            Response::ConsentPending(v) => Ok(v),
            other => Err(unexpected_response(other)),
        }
    }

    /// `Request::ConsentDecide` → approve/deny a pending consent. Returns
    /// whether the daemon recorded the decision.
    pub async fn consent_decide(&mut self, session_id: &str, allow: bool) -> std::io::Result<bool> {
        let req = Request::ConsentDecide {
            session_id: session_id.to_string(),
            allow,
        };
        match self.request(&req).await? {
            Response::ConsentDecided { ok } => Ok(ok),
            other => Err(unexpected_response(other)),
        }
    }

    /// `Request::CreateForward` → the assigned flow id. A daemon
    /// [`Response::Error`] (bad node/remote, port unavailable, no agent WS)
    /// surfaces its message verbatim.
    pub async fn create_forward(
        &mut self,
        node: &str,
        local: u16,
        remote: &str,
        transport: &str,
    ) -> std::io::Result<String> {
        let req = Request::CreateForward {
            node: node.to_string(),
            local,
            remote: remote.to_string(),
            transport: transport.to_string(),
        };
        match self.request(&req).await? {
            Response::FlowCreated { id } => Ok(id),
            Response::Error { message } => Err(std::io::Error::other(message)),
            other => Err(unexpected_response(other)),
        }
    }

    /// `Request::CreateSocks5` → the assigned flow id.
    pub async fn create_socks5(
        &mut self,
        node: &str,
        local: u16,
        transport: &str,
    ) -> std::io::Result<String> {
        let req = Request::CreateSocks5 {
            node: node.to_string(),
            local,
            transport: transport.to_string(),
        };
        match self.request(&req).await? {
            Response::FlowCreated { id } => Ok(id),
            Response::Error { message } => Err(std::io::Error::other(message)),
            other => Err(unexpected_response(other)),
        }
    }

    /// `Request::KillFlow` → whether a flow with that id was found + killed.
    pub async fn kill_flow(&mut self, id: &str) -> std::io::Result<bool> {
        match self
            .request(&Request::KillFlow { id: id.to_string() })
            .await?
        {
            Response::FlowKilled { ok } => Ok(ok),
            other => Err(unexpected_response(other)),
        }
    }

    /// `Request::RouteList` → declared routes + live state.
    pub async fn route_list(&mut self) -> std::io::Result<Vec<RouteInfo>> {
        match self.request(&Request::RouteList).await? {
            Response::Routes(v) => Ok(v),
            other => Err(unexpected_response(other)),
        }
    }

    /// `Request::RouteAdd` → the effective persisted descriptor (id
    /// generated if the request left it empty). A daemon
    /// [`Response::Error`] (duplicate id/port, bad node, config write
    /// failure) surfaces its message verbatim.
    pub async fn route_add(&mut self, route: RouteDescriptor) -> std::io::Result<RouteDescriptor> {
        match self.request(&Request::RouteAdd { route }).await? {
            Response::RouteAdded { route } => Ok(route),
            other => Err(unexpected_response(other)),
        }
    }

    /// `Request::RouteRemove` → whether a route with that id existed.
    pub async fn route_remove(&mut self, id: &str) -> std::io::Result<bool> {
        match self
            .request(&Request::RouteRemove { id: id.to_string() })
            .await?
        {
            Response::RouteRemoved { ok } => Ok(ok),
            other => Err(unexpected_response(other)),
        }
    }

    /// `Request::RouteSetEnabled` → whether a route with that id existed.
    pub async fn route_set_enabled(&mut self, id: &str, enabled: bool) -> std::io::Result<bool> {
        match self
            .request(&Request::RouteSetEnabled {
                id: id.to_string(),
                enabled,
            })
            .await?
        {
            Response::RouteUpdated { ok } => Ok(ok),
            other => Err(unexpected_response(other)),
        }
    }

    /// `Request::SetDeviceName` → the effective (trimmed) name the daemon
    /// persisted. An old daemon that predates the verb answers with a
    /// bad-request [`Response::Error`], which surfaces here as `Err` — callers
    /// with a legacy direct-file path can fall back on that.
    pub async fn set_device_name(&mut self, name: &str) -> std::io::Result<String> {
        match self
            .request(&Request::SetDeviceName {
                name: name.to_string(),
            })
            .await?
        {
            Response::DeviceNameSet { name } => Ok(name),
            other => Err(unexpected_response(other)),
        }
    }

    /// S1b — ask the daemon to archive the stale config copy on a
    /// split-config host. Returns `(ok, detail)`; an older daemon
    /// answers with a clean `Error` (surfaced as `Err`) so callers can
    /// fall back to showing the manual instructions.
    pub async fn config_cleanup_stale(&mut self) -> std::io::Result<(bool, String)> {
        match self.request(&Request::ConfigCleanupStale).await? {
            Response::ConfigCleaned { ok, detail } => Ok((ok, detail)),
            other => Err(unexpected_response(other)),
        }
    }

    /// S2 — fetch the editable config surface (key + current value +
    /// editor metadata). An older daemon answers with a clean `Error`
    /// (surfaced as `Err`) so callers can hide the editor.
    pub async fn config_entries(&mut self) -> std::io::Result<Vec<ConfigEntry>> {
        match self.request(&Request::ConfigGet).await? {
            Response::ConfigEntries(entries) => Ok(entries),
            other => Err(unexpected_response(other)),
        }
    }

    /// S2 — set (`Some(value)`) or clear (`None`) one editable config
    /// key; the daemon validates + persists and echoes the updated entry.
    pub async fn config_set(
        &mut self,
        key: &str,
        value: Option<&str>,
    ) -> std::io::Result<ConfigEntry> {
        match self
            .request(&Request::ConfigSet {
                key: key.to_string(),
                value: value.map(str::to_string),
            })
            .await?
        {
            Response::ConfigUpdated { entry } => Ok(entry),
            other => Err(unexpected_response(other)),
        }
    }

    /// S2 — tail one of the daemon's log files (`daemon` / `service` /
    /// `panic`). Returns `(path, total_size, content)`; poll `size` to
    /// detect growth. An older daemon answers with a clean `Error`.
    pub async fn tail_log(
        &mut self,
        source: &str,
        max_bytes: Option<u64>,
    ) -> std::io::Result<(String, u64, String)> {
        match self
            .request(&Request::TailLog {
                source: source.to_string(),
                max_bytes,
            })
            .await?
        {
            Response::LogTail {
                path,
                size,
                content,
            } => Ok((path, size, content)),
            other => Err(unexpected_response(other)),
        }
    }
}

/// Map an error / mismatched response to an `io::Error` for the typed helpers.
fn unexpected_response(resp: Response) -> std::io::Error {
    match resp {
        Response::Error { message } => std::io::Error::other(format!("localapi error: {message}")),
        other => std::io::Error::other(format!("localapi: unexpected response: {other:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Mock;
    #[async_trait]
    impl LocalApiState for Mock {
        fn status(&self) -> NodeStatus {
            NodeStatus {
                node_id: "n1".into(),
                name: "neo16".into(),
                version: "0.3.0-rc.154".into(),
                mode: DaemonMode::Service,
                tenant_id: Some("t1".into()),
                overlay_ip: Some("100.64.0.2".into()),
                overlay_ip6: None,
                connected: true,
                exit_node: None,
                config_path: None,
                dns: None,
            }
        }
        fn peers(&self) -> Vec<PeerInfo> {
            vec![
                PeerInfo {
                    node_id: "n2".into(),
                    name: "pc50045".into(),
                    overlay_ip: Some("100.64.0.1".into()),
                    overlay_ip6: None,
                    online: true,
                    connection: ConnectionType::Tunnel,
                    upgrading: false,
                    stalled: false,
                    rtt_ms: Some(52),
                    last_seen_ms: Some(1000),
                    agent_id: Some("6a074fe5ef3ba556ab041966".into()),
                    relay_local: Some("94.130.141.74:10850".into()),
                    relay_dst: Some("5.9.157.226:12728".into()),
                    debug: None,
                },
                PeerInfo {
                    node_id: "n3".into(),
                    name: "home".into(),
                    overlay_ip: Some("100.64.0.9".into()),
                    overlay_ip6: None,
                    online: true,
                    connection: ConnectionType::Direct,
                    upgrading: false,
                    stalled: false,
                    rtt_ms: Some(3),
                    last_seen_ms: Some(1200),
                    agent_id: None,
                    relay_local: None,
                    relay_dst: None,
                    debug: None,
                },
            ]
        }
        fn flows(&self) -> Vec<FlowInfo> {
            vec![FlowInfo {
                id: "f1".into(),
                kind: FlowKind::Socks5,
                local_addr: "127.0.0.1:1080".into(),
                target: None,
                node: Some("pc50045".into()),
                transport: "quic-v1".into(),
                active_flows: 2,
                bytes_in: 4096,
                bytes_out: 8192,
            }]
        }
        fn consent_pending(&self) -> Vec<ConsentRequest> {
            vec![ConsentRequest {
                session_id: "sess-1".into(),
                controller_name: "alice".into(),
                permissions: "view|control".into(),
                timeout_secs: 30,
            }]
        }
        fn consent_decide(&self, session_id: &str, allow: bool) -> bool {
            // Test echo: proves both args crossed the wire (real impl writes a
            // sentinel). Records only a non-empty session that was approved.
            !session_id.is_empty() && allow
        }
        async fn create_forward(
            &self,
            node: &str,
            local: u16,
            _remote: &str,
            _transport: &str,
        ) -> Response {
            // Echo the args back as the flow id so the test proves they crossed.
            Response::FlowCreated {
                id: format!("{node}:{local}"),
            }
        }
        async fn create_socks5(&self, node: &str, local: u16, _transport: &str) -> Response {
            Response::FlowCreated {
                id: format!("socks-{node}:{local}"),
            }
        }
        fn kill_flow(&self, id: &str) -> bool {
            id == "f1"
        }
    }

    #[test]
    fn handle_dispatches_each_verb() {
        let s = Mock;
        match handle(&Request::Status, &s) {
            Response::Status(st) => {
                assert_eq!(st.overlay_ip.as_deref(), Some("100.64.0.2"));
                assert_eq!(st.mode, DaemonMode::Service);
            }
            other => panic!("expected Status, got {other:?}"),
        }
        match handle(&Request::Peers, &s) {
            Response::Peers(p) => {
                assert_eq!(p.len(), 2);
                assert_eq!(p[0].connection, ConnectionType::Tunnel);
                assert_eq!(p[1].connection, ConnectionType::Direct);
            }
            other => panic!("expected Peers, got {other:?}"),
        }
        match handle(&Request::Flows, &s) {
            Response::Flows(f) => {
                assert_eq!(f.len(), 1);
                assert_eq!(f[0].kind, FlowKind::Socks5);
                assert!(f[0].target.is_none());
            }
            other => panic!("expected Flows, got {other:?}"),
        }
    }

    #[test]
    fn handle_dispatches_consent_verbs() {
        let s = Mock;
        match handle(&Request::ConsentPending, &s) {
            Response::ConsentPending(v) => {
                assert_eq!(v.len(), 1);
                assert_eq!(v[0].session_id, "sess-1");
                assert_eq!(v[0].permissions, "view|control");
            }
            other => panic!("expected ConsentPending, got {other:?}"),
        }
        // The `allow` bit crosses the wire (Mock echoes it).
        match handle(
            &Request::ConsentDecide {
                session_id: "sess-1".into(),
                allow: true,
            },
            &s,
        ) {
            Response::ConsentDecided { ok } => assert!(ok),
            other => panic!("expected ConsentDecided, got {other:?}"),
        }
        match handle(
            &Request::ConsentDecide {
                session_id: "sess-1".into(),
                allow: false,
            },
            &s,
        ) {
            Response::ConsentDecided { ok } => assert!(!ok),
            other => panic!("expected ConsentDecided, got {other:?}"),
        }
        // Wire shape — locks the discriminators the tray/CLI depend on.
        assert_eq!(
            serde_json::to_string(&Request::ConsentDecide {
                session_id: "s".into(),
                allow: true,
            })
            .unwrap(),
            r#"{"t":"consent_decide","d":{"session_id":"s","allow":true}}"#
        );
        assert_eq!(
            serde_json::from_str::<Request>(r#"{"t":"consent_pending"}"#).unwrap(),
            Request::ConsentPending
        );
    }

    #[tokio::test]
    async fn create_and_kill_flow_verbs_dispatch_and_lock_wire_shape() {
        let s = Mock;
        // KillFlow is sync — through `handle`.
        assert!(matches!(
            handle(&Request::KillFlow { id: "f1".into() }, &s),
            Response::FlowKilled { ok: true }
        ));
        assert!(matches!(
            handle(&Request::KillFlow { id: "nope".into() }, &s),
            Response::FlowKilled { ok: false }
        ));
        // CreateForward / CreateSocks5 are async — awaited on the trait (the
        // `handle` sync arm returns the async-path Error, also asserted).
        match s.create_forward("aid", 5432, "db:5432", "auto").await {
            Response::FlowCreated { id } => assert_eq!(id, "aid:5432"),
            other => panic!("expected FlowCreated, got {other:?}"),
        }
        match s.create_socks5("aid", 1080, "quic").await {
            Response::FlowCreated { id } => assert_eq!(id, "socks-aid:1080"),
            other => panic!("expected FlowCreated, got {other:?}"),
        }
        assert!(matches!(
            handle(
                &Request::CreateForward {
                    node: "a".into(),
                    local: 1,
                    remote: "h:2".into(),
                    transport: String::new()
                },
                &s
            ),
            Response::Error { .. }
        ));

        // Wire shape — locks the discriminators the CLI depends on.
        assert_eq!(
            serde_json::to_string(&Request::CreateForward {
                node: "aid".into(),
                local: 5432,
                remote: "db:5432".into(),
                transport: "auto".into(),
            })
            .unwrap(),
            r#"{"t":"create_forward","d":{"node":"aid","local":5432,"remote":"db:5432","transport":"auto"}}"#
        );
        assert_eq!(
            serde_json::to_string(&Request::KillFlow { id: "f1".into() }).unwrap(),
            r#"{"t":"kill_flow","d":{"id":"f1"}}"#
        );
        assert_eq!(
            serde_json::to_string(&Response::FlowCreated { id: "f1".into() }).unwrap(),
            r#"{"t":"flow_created","d":{"id":"f1"}}"#
        );
        assert_eq!(
            serde_json::to_string(&Response::FlowKilled { ok: true }).unwrap(),
            r#"{"t":"flow_killed","d":{"ok":true}}"#
        );
        // `transport` defaults when omitted (older CLI / minimal request).
        assert_eq!(
            serde_json::from_str::<Request>(
                r#"{"t":"create_socks5","d":{"node":"aid","local":1080}}"#
            )
            .unwrap(),
            Request::CreateSocks5 {
                node: "aid".into(),
                local: 1080,
                transport: String::new(),
            }
        );
    }

    #[test]
    fn request_wire_shape_is_stable() {
        assert_eq!(
            serde_json::to_string(&Request::Status).unwrap(),
            r#"{"t":"status"}"#
        );
        assert_eq!(
            serde_json::from_str::<Request>(r#"{"t":"peers"}"#).unwrap(),
            Request::Peers
        );
    }

    /// rc.276 — `PeerInfo.debug` is wire-compatible: absent (pre-rc.276
    /// daemon) ⇒ `None`; a populated snapshot round-trips.
    #[test]
    fn peer_info_debug_wire_compat() {
        let old = r#"{"node_id":"n1","name":"pc","online":true,"connection":"relay"}"#;
        let p: PeerInfo = serde_json::from_str(old).unwrap();
        assert!(p.debug.is_none());
        let mut p2 = p.clone();
        p2.debug = Some(PeerCarrierDebug {
            tier: "relay".into(),
            initiated: true,
            hs_done: false,
            local: Some("94.130.141.74:10850".into()),
            dst: Some("5.9.157.226:12728".into()),
            tx: 42,
            rx: 0,
            last_rx_age_s: 7,
            relay_kind: Some("turn".into()),
            rx_denied: 0,
        });
        let s = serde_json::to_string(&p2).unwrap();
        assert!(s.contains(r#""tier":"relay""#) && s.contains(r#""tx":42"#));
        // P4 — a clean peer omits the counter entirely, so a healthy fleet's
        // `peers --json` is byte-identical to pre-P4.
        assert!(!s.contains("rx_denied"));
        let back: PeerInfo = serde_json::from_str(&s).unwrap();
        assert_eq!(back.debug, p2.debug);

        // …and a peer the ingress ACL refused surfaces the count.
        let mut p3 = p2.clone();
        if let Some(d) = p3.debug.as_mut() {
            d.rx_denied = 17;
        }
        let s3 = serde_json::to_string(&p3).unwrap();
        assert!(s3.contains(r#""rx_denied":17"#));
        let back3: PeerInfo = serde_json::from_str(&s3).unwrap();
        assert_eq!(back3.debug.unwrap().rx_denied, 17);
    }

    /// rc.275 — `PeerInfo.stalled` is wire-compatible in both directions: a
    /// pre-rc.275 daemon's JSON (no `stalled` key) deserialises to `false`,
    /// and a set flag round-trips.
    #[test]
    fn peer_info_stalled_wire_compat() {
        // Absent → default false (an old daemon talking to a new CLI).
        let old = r#"{"node_id":"n1","name":"pc","overlay_ip":null,"overlay_ip6":null,
                      "online":true,"connection":"relay","upgrading":false}"#;
        let p: PeerInfo = serde_json::from_str(old).unwrap();
        assert!(!p.stalled);
        // Set → survives a round-trip.
        let mut p2 = p.clone();
        p2.stalled = true;
        let s = serde_json::to_string(&p2).unwrap();
        assert!(s.contains(r#""stalled":true"#));
        assert!(serde_json::from_str::<PeerInfo>(&s).unwrap().stalled);
    }

    #[tokio::test]
    async fn set_device_name_wire_shape_and_default_unsupported() {
        // Wire lock: adjacently tagged, snake_case.
        assert_eq!(
            serde_json::to_string(&Request::SetDeviceName { name: "neo".into() }).unwrap(),
            r#"{"t":"set_device_name","d":{"name":"neo"}}"#
        );
        assert_eq!(
            serde_json::to_string(&Response::DeviceNameSet { name: "neo".into() }).unwrap(),
            r#"{"t":"device_name_set","d":{"name":"neo"}}"#
        );
        // Trait default: unsupported — a node without a config writer (mocks,
        // non-daemon impls) answers with a clean Error, and the sync `handle`
        // path refuses it (async-only verb).
        let s = Mock;
        assert!(matches!(
            s.set_device_name("neo").await,
            Response::Error { .. }
        ));
        assert!(matches!(
            handle(&Request::SetDeviceName { name: "neo".into() }, &s),
            Response::Error { .. }
        ));
    }

    #[tokio::test]
    async fn config_verbs_wire_shape_and_default_unsupported() {
        // Wire lock: adjacently tagged, snake_case; unset `value` is
        // omitted on the wire (both directions), a set value is a plain
        // string.
        assert_eq!(
            serde_json::to_string(&Request::ConfigGet).unwrap(),
            r#"{"t":"config_get"}"#
        );
        assert_eq!(
            serde_json::to_string(&Request::ConfigSet {
                key: "overlay_quic".into(),
                value: Some("true".into()),
            })
            .unwrap(),
            r#"{"t":"config_set","d":{"key":"overlay_quic","value":"true"}}"#
        );
        // Clearing a key: `value` may be omitted entirely by old/terse
        // clients — serde(default) maps that to None.
        assert_eq!(
            serde_json::from_str::<Request>(r#"{"t":"config_set","d":{"key":"overlay_quic"}}"#)
                .unwrap(),
            Request::ConfigSet {
                key: "overlay_quic".into(),
                value: None,
            }
        );
        let entry = ConfigEntry {
            key: "overlay_quic".into(),
            value: None,
            kind: "tribool".into(),
            restart_required: true,
            description: "d".into(),
        };
        assert_eq!(
            serde_json::to_string(&Response::ConfigUpdated {
                entry: entry.clone()
            })
            .unwrap(),
            r#"{"t":"config_updated","d":{"entry":{"key":"overlay_quic","kind":"tribool","restart_required":true,"description":"d"}}}"#
        );
        assert_eq!(
            serde_json::to_string(&Response::ConfigEntries(vec![])).unwrap(),
            r#"{"t":"config_entries","d":[]}"#
        );
        // Trait defaults: unsupported on non-daemon impls; the sync
        // `handle` path refuses both (async-only verbs).
        let s = Mock;
        assert!(matches!(s.config_entries().await, Response::Error { .. }));
        assert!(matches!(
            s.config_set("overlay_quic", None).await,
            Response::Error { .. }
        ));
        assert!(matches!(
            handle(&Request::ConfigGet, &s),
            Response::Error { .. }
        ));
    }

    #[tokio::test]
    async fn route_verbs_dispatch_and_lock_wire_shape() {
        let s = Mock;
        // RouteList is sync — through `handle`; the Mock has no reconciler,
        // so the default-impl empty list comes back.
        assert!(matches!(
            handle(&Request::RouteList, &s),
            Response::Routes(ref v) if v.is_empty()
        ));
        // The mutating Route* verbs are async-path only; the sync arm errors.
        assert!(matches!(
            handle(&Request::RouteRemove { id: "r".into() }, &s),
            Response::Error { .. }
        ));
        // Default trait impls report unsupported.
        assert!(matches!(s.route_remove("r").await, Response::Error { .. }));

        // Wire shape — locks the discriminators the CLI + desktop depend on.
        let route = RouteDescriptor {
            id: "pg-mars".into(),
            kind: FlowKind::Forward,
            node: "aabbcc".into(),
            local: 15432,
            remote: Some("db:5432".into()),
            transport: "auto".into(),
            enabled: true,
        };
        assert_eq!(
            serde_json::to_string(&Request::RouteAdd {
                route: route.clone()
            })
            .unwrap(),
            r#"{"t":"route_add","d":{"route":{"id":"pg-mars","kind":"forward","node":"aabbcc","local":15432,"remote":"db:5432","transport":"auto","enabled":true}}}"#
        );
        assert_eq!(
            serde_json::to_string(&Request::RouteSetEnabled {
                id: "pg-mars".into(),
                enabled: false,
            })
            .unwrap(),
            r#"{"t":"route_set_enabled","d":{"id":"pg-mars","enabled":false}}"#
        );
        assert_eq!(
            serde_json::to_string(&Response::RouteAdded {
                route: route.clone()
            })
            .unwrap(),
            r#"{"t":"route_added","d":{"route":{"id":"pg-mars","kind":"forward","node":"aabbcc","local":15432,"remote":"db:5432","transport":"auto","enabled":true}}}"#
        );
        let info = RouteInfo {
            route,
            state: RouteState::Backoff {
                next_retry_secs: 30,
                last_error: "bind: in use".into(),
            },
        };
        let s = serde_json::to_string(&Response::Routes(vec![info.clone()])).unwrap();
        assert!(
            s.contains(r#""state":{"state":"backoff","next_retry_secs":30"#),
            "got {s}"
        );
        assert_eq!(
            serde_json::from_str::<Response>(&s).unwrap(),
            Response::Routes(vec![info])
        );
        // A minimal hand-written config-style descriptor: id/remote/transport
        // omitted, enabled defaults TRUE (a bare [[tunnel_routes]] entry is
        // live), socks5 needs no remote.
        assert_eq!(
            serde_json::from_str::<RouteDescriptor>(
                r#"{"kind":"socks5","node":"aabbcc","local":1080}"#
            )
            .unwrap(),
            RouteDescriptor {
                id: String::new(),
                kind: FlowKind::Socks5,
                node: "aabbcc".into(),
                local: 1080,
                remote: None,
                transport: String::new(),
                enabled: true,
            }
        );
        // Terminal-state wire shape (the pane's re-enable affordance keys on
        // "failed").
        assert_eq!(
            serde_json::to_string(&RouteState::Failed {
                reason: "revoked".into()
            })
            .unwrap(),
            r#"{"state":"failed","reason":"revoked"}"#
        );
    }

    #[test]
    fn response_round_trips_struct_and_sequence_payloads() {
        // Adjacently-tagged so a sequence payload (Peers) is legal where an
        // internally-tagged enum would reject it — locks that choice.
        let peers = handle(&Request::Peers, &Mock);
        let s = serde_json::to_string(&peers).unwrap();
        assert!(s.starts_with(r#"{"t":"peers","d":["#), "got {s}");
        assert_eq!(serde_json::from_str::<Response>(&s).unwrap(), peers);

        let status = handle(&Request::Status, &Mock);
        let s = serde_json::to_string(&status).unwrap();
        assert!(s.contains(r#""t":"status""#));
        assert_eq!(serde_json::from_str::<Response>(&s).unwrap(), status);

        let err = Response::Error {
            message: "nope".into(),
        };
        assert_eq!(
            serde_json::to_string(&err).unwrap(),
            r#"{"t":"error","d":{"message":"nope"}}"#
        );

        // Connection types serialise snake_case (UI + wire contract).
        assert_eq!(
            serde_json::to_string(&ConnectionType::Tunnel).unwrap(),
            r#""tunnel""#
        );
    }

    #[tokio::test]
    async fn serve_connection_round_trips_and_recovers_from_garbage() {
        // In-memory duplex stands in for the named pipe / unix socket, so the
        // dispatch loop is transport-independently tested.
        let (client, server) = tokio::io::duplex(4096);
        let srv = tokio::spawn(async move {
            let state = Mock;
            serve_connection(server, &state).await
        });
        let (crd, mut cwr) = tokio::io::split(client);
        let mut clines = tokio::io::BufReader::new(crd).lines();

        cwr.write_all(b"{\"t\":\"status\"}\n").await.unwrap();
        let r = clines.next_line().await.unwrap().unwrap();
        assert!(matches!(
            serde_json::from_str::<Response>(&r).unwrap(),
            Response::Status(_)
        ));

        cwr.write_all(b"{\"t\":\"peers\"}\n").await.unwrap();
        let r = clines.next_line().await.unwrap().unwrap();
        assert!(matches!(
            serde_json::from_str::<Response>(&r).unwrap(),
            Response::Peers(p) if p.len() == 2
        ));

        // Garbage line → Error response, and the connection survives for the
        // next request (recoverable, not a frame break).
        cwr.write_all(b"not json\n").await.unwrap();
        let r = clines.next_line().await.unwrap().unwrap();
        assert!(matches!(
            serde_json::from_str::<Response>(&r).unwrap(),
            Response::Error { .. }
        ));

        // Client closes the stream → serve_connection returns Ok(()).
        drop(cwr);
        drop(clines);
        srv.await.unwrap().unwrap();
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn client_round_trips_over_named_pipe() {
        // Drives the real `Client` against `serve_windows_at` — exercises the
        // named pipe + the SDDL security descriptor (PipeSecurity::new must
        // convert the SDDL, or bind fails) + the client connect/request path. A
        // private pipe name avoids colliding with a real daemon on the box.
        let pipe = format!(r"\\.\pipe\roomler-test-{}", std::process::id());
        let (sd_tx, sd_rx) = watch::channel(false);
        let state: Arc<dyn LocalApiState> = Arc::new(Mock);
        let pipe_srv = pipe.clone();
        let srv = tokio::spawn(async move { serve_windows_at(&pipe_srv, state, sd_rx).await });

        // Retry connect until the first pipe instance exists.
        let mut client = None;
        for _ in 0..200 {
            match connect_windows_at(&pipe).await {
                Ok(c) => {
                    client = Some(c);
                    break;
                }
                Err(_) => tokio::time::sleep(std::time::Duration::from_millis(10)).await,
            }
        }
        let mut client = client.expect("connect to the LocalAPI pipe");

        let status = client.status().await.unwrap();
        assert_eq!(status.name, "neo16");
        assert!(status.connected);
        let peers = client.peers().await.unwrap();
        assert_eq!(peers.len(), 2);
        assert_eq!(peers[0].connection, ConnectionType::Tunnel);
        // A second request on the SAME connection works (the daemon loops).
        assert_eq!(client.peers().await.unwrap().len(), 2);

        // Consent verbs over the real pipe (P2b).
        let pending = client.consent_pending().await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].session_id, "sess-1");
        assert!(client.consent_decide("sess-1", true).await.unwrap());
        assert!(!client.consent_decide("sess-1", false).await.unwrap());

        sd_tx.send(true).unwrap();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), srv).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn client_round_trips_over_unix_socket_and_is_0600() {
        // Drives the real `Client` against `serve_unix_at` + asserts the socket
        // is owner-only.
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("roomler-lat-{}", std::process::id()));
        let path = dir.join("s.sock");
        let (sd_tx, sd_rx) = watch::channel(false);
        let state: Arc<dyn LocalApiState> = Arc::new(Mock);
        let p = path.clone();
        let srv = tokio::spawn(async move { serve_unix_at(p, state, sd_rx).await });

        for _ in 0..200 {
            if path.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let mut client = connect_unix_at(path.clone())
            .await
            .expect("connect to the LocalAPI socket");
        assert_eq!(client.status().await.unwrap().name, "neo16");
        assert_eq!(client.peers().await.unwrap().len(), 2);

        // The control socket must be private to the owner.
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "control socket must be 0600");

        sd_tx.send(true).unwrap();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), srv).await;
        let _ = std::fs::remove_dir_all(&dir);
    }
}
