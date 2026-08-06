//! Agent on-disk configuration.
//!
//! Stored at `<user config dir>/roomler-agent/config.toml`. On Linux that
//! resolves to `$XDG_CONFIG_HOME/roomler-agent/` or `~/.config/roomler-agent/`.
//!
//! The file holds the enrolled agent's identity, its long-lived agent
//! token, and the server URL. It is the user's responsibility to keep
//! the file at mode 0600; on Linux/macOS we set that permission on write.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::tunnel::acl::AgentForwardAcl;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    /// Base URL of the Roomler API, e.g. `https://roomler.live`. No trailing slash.
    pub server_url: String,
    /// Derived WSS URL; recomputed from `server_url` if absent.
    #[serde(default)]
    pub ws_url: Option<String>,
    /// Opaque agent JWT issued by `/api/agent/enroll`.
    pub agent_token: String,
    /// Server-assigned agent id (hex ObjectId).
    pub agent_id: String,
    /// Server-assigned tenant id (hex ObjectId).
    pub tenant_id: String,
    /// Stable machine fingerprint. Persisted so re-enrollment maps to the
    /// same `agents` row.
    pub machine_id: String,
    /// User-friendly name shown in the admin UI.
    pub machine_name: String,
    /// Encoder preference: `auto` (default), `hardware`, or `software`.
    /// Can be overridden at launch by `ROOMLER_AGENT_ENCODER` env var or
    /// `--encoder` CLI flag.
    #[serde(default)]
    pub encoder_preference: EncoderPreferenceChoice,

    /// How often (hours) the auto-updater polls GitHub Releases.
    /// `None` keeps the built-in default (24 h, see
    /// `updater::CHECK_INTERVAL`). Override at launch via the
    /// `ROOMLER_AGENT_UPDATE_INTERVAL_H` env var. Setting this to a
    /// large value (e.g. 168 = weekly) is the recommended way to
    /// dampen update load on bandwidth-constrained fleets.
    #[serde(default)]
    pub update_check_interval_h: Option<u32>,

    /// Whether the agent answers `files:dir` (filesystem browse)
    /// requests from the browser controller. Default `true` to
    /// preserve self-controlled-host auto-grant semantics
    /// (`docs/remote-control.md` §11.2). Operators on org-controlled
    /// fleets can disable per-host via `config.toml`. When `false`,
    /// `files:dir` returns `dir-error { message: "remote browse
    /// disabled" }`. Single-file downloads (`files:get`) and uploads
    /// are NOT gated by this flag — they're consent-bound by the
    /// session itself.
    #[serde(default = "default_enable_remote_browse")]
    pub enable_remote_browse: bool,

    /// Whether incoming `rc:session.request` messages are
    /// auto-granted without operator interaction. Default `true` to
    /// match historical self-host behaviour (`docs/remote-control.md`
    /// §11.2 + signaling.rs's pre-Plan-3 auto-grant). Org-controlled
    /// fleets set this to `false` so every session start waits for
    /// an explicit operator decision via the `roomler-agent consent
    /// --session <hex> --approve|--deny` CLI fallback (or, in a
    /// future version, a tray prompt). 30 s timeout → auto-deny.
    /// Has NO effect on the file-DC path — uploads/downloads/dir
    /// browsing remain gated by `enable_remote_browse` + the
    /// agent's denylist.
    #[serde(default = "default_auto_grant_session")]
    pub auto_grant_session: bool,

    /// Fleet RPC — gate 4 of four. Whether this device will run a
    /// `rc:rpc.exec` command at all.
    ///
    /// Default **`false`**, unlike every other capability flag here. The
    /// other three gates (org kill-switch, caller permission, the device's
    /// server-side `ExecPolicy`) all live on the control plane; this one
    /// belongs to whoever physically holds the box, and it is the only
    /// refusal that survives a compromised server. Defaulting it on would
    /// make the other three the whole story.
    ///
    /// Commands inherit the daemon's identity — SYSTEM under a perMachine
    /// Windows install, root under systemd. Turning this on is granting
    /// root, and the admin UI says so.
    #[serde(default)]
    pub exec_enabled: bool,

    // ─── S2: env-bridged operator knobs ──────────────────────────────────
    // Each mirrors an env var read through `tunnel_core::env::node_env`
    // (precedence: env — either prefix — > this config key > built-in
    // default). `None` keeps the default. All are applied at daemon
    // startup, so changes take effect on the next service restart.
    /// QUIC-over-TURN overlay carrier (`ROOMLER_NODE_OVERLAY_QUIC`).
    /// Built-in default: off.
    #[serde(default)]
    pub overlay_quic: Option<bool>,
    /// Direct (LAN / hole-punched) overlay carriers
    /// (`ROOMLER_NODE_OVERLAY_DIRECT`). Built-in default: on.
    #[serde(default)]
    pub overlay_direct: Option<bool>,
    /// DERP (WS-relay) overlay fallback tier (`ROOMLER_NODE_OVERLAY_DERP`).
    /// Built-in default: on.
    #[serde(default)]
    pub overlay_derp: Option<bool>,
    /// Make-before-break carrier upgrades (`ROOMLER_NODE_OVERLAY_MBB`).
    /// Built-in default: on.
    #[serde(default)]
    pub overlay_mbb: Option<bool>,
    /// LAN-gather virtual-interface filter — drop WSL/Hyper-V/other-VPN
    /// adapters from advertised LAN endpoints
    /// (`ROOMLER_NODE_OVERLAY_LAN_IFACE_FILTER`). Built-in default: on.
    #[serde(default)]
    pub overlay_lan_iface_filter: Option<bool>,
    /// Overlay PathMonitor engagement (`ROOMLER_NODE_OVERLAY_PATHMON`):
    /// `on` (authoritative — the built-in default since PR-D's two green
    /// soaks) | `shadow` (fed + compared, legacy decides — the per-host
    /// revert rail) | `off`. Multi-state, so a string key, not a tribool.
    #[serde(default)]
    pub overlay_pathmon: Option<String>,
    /// P4 — event-driven route guard (OS route-table change subscription:
    /// NotifyRouteChange2 / `ip monitor route`)
    /// (`ROOMLER_NODE_OVERLAY_ROUTE_EVENTS`). Built-in default: on.
    #[serde(default)]
    pub overlay_route_events: Option<bool>,
    /// P4 demotion — route-guard blind-tick seconds while the route-event
    /// subscription is live (`ROOMLER_NODE_OVERLAY_ROUTE_TICK_SECS`, 2–300).
    /// Built-in default: 30 (the demoted heartbeat); `2` = the pre-demotion
    /// war cadence (operator revert). Without a live subscription the tick
    /// is always 2 s regardless of this key.
    #[serde(default)]
    pub overlay_route_tick_secs: Option<u32>,
    /// Force overlay coturn allocations onto the TURNS/TCP (TLS) tier —
    /// the corp-VPN field probe (`ROOMLER_NODE_OVERLAY_RELAY_TLS`).
    /// Built-in default: off.
    #[serde(default)]
    pub overlay_relay_tls: Option<bool>,
    /// Stable Wintun adapter identity — constant requested GUID + boot
    /// stray-adapter sweep, Windows only
    /// (`ROOMLER_NODE_OVERLAY_TUN_STABLE_GUID`). Built-in default: on.
    #[serde(default)]
    pub overlay_tun_stable_guid: Option<bool>,
    /// Route-war eviction — delete competing VPN-installed routes for
    /// overlay prefixes (peer + own `/32`s), Windows only
    /// (`ROOMLER_NODE_OVERLAY_ROUTE_EVICT`). Built-in default: on.
    #[serde(default)]
    pub overlay_route_evict: Option<bool>,
    /// Keep the overlay TUN device alive across signaling reconnects
    /// (process-lifetime cache in the agent's TUN factory)
    /// (`ROOMLER_NODE_OVERLAY_TUN_PERSIST`). Built-in default: on.
    #[serde(default)]
    pub overlay_tun_persist: Option<bool>,
    /// Install defended peer `/32`s (and the ULA `/96` + connected `/10`) at
    /// route metric 0 so they outrank a corp VPN's metric-1 mirror routes,
    /// Windows only (`ROOMLER_NODE_OVERLAY_ROUTE_METRIC0`). Built-in
    /// default: **off** — AnyConnect's route monitor deletes routes that
    /// would out-rank its own, which leaves the prefix unrouted (rc.289);
    /// opt-in experiment, auto-yields to metric 1 when it doesn't stick.
    #[serde(default)]
    pub overlay_route_metric0: Option<bool>,
    /// Stable UDP port for the overlay's direct sockets
    /// (`ROOMLER_NODE_OVERLAY_DIRECT_PORT`). Built-in default: **43648**
    /// (per-interface LAN sockets; the public/srflx dialer takes base+32; a
    /// swallowed base walks an 8-port band);
    /// `0` = ephemeral ports (the pre-rc.307 behavior). Rationale: stateful
    /// corp firewalls (Check Point) grandfather UDP flows that predate the
    /// VPN's session table — with a stable port a rebuilt carrier (agent
    /// update, control-WS reconnect) reproduces the SAME 5-tuple and keeps
    /// riding the grandfathered session instead of relay-locking until the
    /// next VPN-off window (CORPLAP-1, 2026-08-05). A bind conflict falls back
    /// to an ephemeral port with a WARN, restoring the old behavior.
    #[serde(default)]
    pub overlay_direct_port: Option<u32>,
    /// Loopback-TURN corp-relay for co-located controllers
    /// (`ROOMLER_AGENT_LOCAL_TURN`). Built-in default: on.
    #[serde(default)]
    pub local_turn: Option<bool>,
    /// MagicDNS AAAA answers for derived overlay v6
    /// (`ROOMLER_AGENT_DNS_AAAA`). Built-in default: on.
    #[serde(default)]
    pub dns_aaaa: Option<bool>,
    /// The periodic self-updater (`ROOMLER_AGENT_AUTO_UPDATE`).
    /// Built-in default: on. Disabling also ignores web-pushed
    /// forced updates.
    #[serde(default)]
    pub auto_update: Option<bool>,
    /// Disable the centralized log uploader
    /// (`ROOMLER_AGENT_LOGS_UPLOAD_DISABLED`). `Some(true)` = uploads
    /// OFF. Built-in default: uploads on.
    #[serde(default)]
    pub logs_upload_disabled: Option<bool>,
    /// Per-codec maxrate ceiling factor overrides, percent (50–400).
    /// (`ROOMLER_NODE_RATE_FACTOR_H264` etc.) Built-ins: H264 150,
    /// HEVC 125, VP9 100, AV1 100.
    #[serde(default)]
    pub rate_factor_h264: Option<u32>,
    #[serde(default)]
    pub rate_factor_hevc: Option<u32>,
    #[serde(default)]
    pub rate_factor_vp9: Option<u32>,
    #[serde(default)]
    pub rate_factor_av1: Option<u32>,
    /// Media-ICE follow-renomination policy
    /// (`ROOMLER_ICE_FOLLOW_RENOMINATION`, raw env in the vendored
    /// webrtc-ice — bridged via a set_var-if-unset shim in `run_cmd`).
    /// `None`/"auto" = built-in upward-only+stale policy (rc.268);
    /// `Some(true)`/"always" = legacy follow-everything (rc.260 —
    /// thrash-prone, diagnostics only); `Some(false)`/"never" = pin to
    /// first nomination (rc.262 semantics).
    #[serde(default)]
    pub ice_follow_renomination: Option<bool>,
    /// Warm-standby keepalive pings on validated-but-unselected media
    /// ICE pairs (`ROOMLER_ICE_WARM_STANDBY`, vendored-crate raw env —
    /// same shim). Built-in default: on.
    #[serde(default)]
    pub ice_warm_standby: Option<bool>,
    /// Deprioritize overlay-TUN host candidates in media ICE
    /// (`ROOMLER_ICE_OVERLAY_HOST_DEPRIORITIZE`, vendored-crate raw
    /// env — same shim). Built-in default: on.
    #[serde(default)]
    pub ice_overlay_host_deprioritize: Option<bool>,
    /// Overlay-carrier-aware constrained detection in the media pumps
    /// (`ROOMLER_AGENT_OVERLAY_TIER_DETECT`). Built-in default: on.
    #[serde(default)]
    pub overlay_tier_detect: Option<bool>,
    /// B1 — feed the 15 s overlay RTT probes into the PathMonitor's
    /// quality plane (`ROOMLER_AGENT_OVERLAY_RTT_Q`). Q-only — never
    /// eligibility. Built-in default: on (kill switch).
    #[serde(default)]
    pub overlay_rtt_q: Option<bool>,
    /// Multi-region relay PoPs — probe the server-pushed region list (timed
    /// STUN binding per PoP) and report RTTs so the server derives this
    /// node's `relay_home` (`ROOMLER_NODE_RELAY_PROBE`). Built-in default:
    /// on (kill switch).
    #[serde(default)]
    pub relay_probe: Option<bool>,
    /// 2026-08-04 - KeyText modifier neutralization: temporarily release
    /// physically-held Shift/Ctrl/Alt that the remote layout does NOT
    /// want around a scancode tap (fixes '+'->'*' / dead '|' on non-US
    /// hosts), restoring them after (`ROOMLER_AGENT_TEXT_MOD_NEUTRALIZE`).
    /// Built-in default: on (kill switch).
    #[serde(default)]
    pub text_mod_neutralize: Option<bool>,
    /// B2 — score-driven demotion of degraded-but-live direct carriers
    /// (`ROOMLER_AGENT_OVERLAY_DEMOTE`): `off` | `shadow` (compute +
    /// count, never act — the built-in default) | `on` (voluntary MBB
    /// demotions execute). Multi-state, so a string key, not a tribool.
    #[serde(default)]
    pub overlay_demote: Option<String>,
    /// B3 — mid-tier upward probing: srflx/public incumbents probe an
    /// eligible HIGHER tier every ≥120 s via the MBB machinery
    /// (`ROOMLER_AGENT_OVERLAY_UPWARD_PROBE`). Built-in default: on.
    #[serde(default)]
    pub overlay_upward_probe: Option<bool>,
    /// Multi-user P3 — concurrent remote-control sessions this agent
    /// advertises (`ROOMLER_AGENT_RC_MAX_SESSIONS`, 1–8). Built-in
    /// default: 2. With the P5 shared encoder (default on) same-profile
    /// DC viewers share one capture + encoder; distinct profiles still
    /// run their own — weak-GPU hosts may prefer 1. The server keeps
    /// concurrent INPUT to one holder regardless.
    #[serde(default)]
    pub rc_max_sessions: Option<u32>,
    /// Multi-user P5 — shared-floor encoder for concurrent DC viewers
    /// (`ROOMLER_AGENT_SHARED_ENCODER`): same-profile sessions share ONE
    /// capture + encoder with floor-merged rate/dials; `off` reverts to
    /// the rc.302 pump-per-session behaviour. Built-in default: on.
    #[serde(default)]
    pub shared_encoder: Option<bool>,
    /// P4 — ingress filtering of INBOUND overlay packets
    /// (`ROOMLER_NODE_OVERLAY_RPF`): `off` | `warn` (count + log, still
    /// deliver — the built-in default) | `enforce` (drop). Two checks share
    /// the mode: a packet whose SOURCE the sending peer does not own
    /// (forgery), and one whose DESTINATION lies outside the subnets this
    /// node advertises (aiming past a subnet router). Multi-state, so a
    /// string key, not a tribool.
    #[serde(default)]
    pub overlay_rpf: Option<String>,

    /// Most recent version that ran for at least
    /// `CLEAN_RUN_THRESHOLD` seconds before exiting cleanly (or
    /// crashing — the threshold is what gates updates here, not exit
    /// reason). Used by [`should_rollback`] to pick a fallback
    /// target when the current version crash-loops on cold start.
    /// `None` on a fresh install (no prior version to roll back to).
    #[serde(default)]
    pub last_known_good_version: Option<String>,

    /// Consecutive cold-start crashes within `CRASH_WINDOW_SECS` of
    /// each other. Bumped at startup by [`record_crash_at`]; reset
    /// to 0 by [`record_clean_run_at`] once a run survives long
    /// enough.
    #[serde(default)]
    pub crash_count: u32,

    /// Unix timestamp (seconds) of the most recent crash. Compared
    /// against the current time to decide whether the next crash
    /// "extends" the current crash window or starts a new one.
    #[serde(default)]
    pub last_crash_unix: u64,

    /// Set by the rollback path when it fires once. Cleared on next
    /// successful clean run (i.e. when the new-old version has
    /// proven itself stable). Prevents an oscillation loop between
    /// two equally-bad versions: we roll back at most once per
    /// install cycle.
    #[serde(default)]
    pub rollback_attempted: bool,

    /// `true` when the previous run started but never reached the
    /// clean-run threshold AND didn't exit gracefully via Ctrl-C.
    /// Read at startup to decide whether the previous run counts
    /// as a crash for [`record_crash_at`]. Set true by
    /// [`mark_run_starting`] at the top of every run; flipped back
    /// to false by [`record_clean_run_at`] (after the threshold)
    /// or by the graceful-shutdown path (Ctrl-C handler).
    ///
    /// Default `false` so a brand-new install isn't treated as a
    /// crash on its first run.
    #[serde(default)]
    pub last_run_unhealthy: bool,

    /// Last config-schema version this file was migrated to. Used by
    /// [`migrate`] to decide which migration steps to apply at startup.
    /// `None` (or missing in TOML) on pre-rc.18 configs — those run
    /// through the rc.18 migration set and the field is then persisted.
    /// Forward-compat: future RCs key migrations off this string
    /// (e.g. `match version { Some("0.3.0-rc.18") => apply_rc18_to_rc19, … }`).
    #[serde(default)]
    pub config_schema_version: Option<String>,

    /// roomler-tunnel agent-side allowlist (T2.6). Default is
    /// `enabled` with an empty allowlist — meaning "trust the
    /// server's tenant policy on every `ServerMsg::TcpForwardForward`".
    /// Operators on org-controlled hosts narrow further by populating
    /// `forward_acl.allowlist` in the TOML or disable forwards
    /// entirely with `forward_acl.enabled = false`. See
    /// `agents/roomler-agent/src/tunnel/acl.rs` for the matching
    /// semantics.
    #[serde(default)]
    pub forward_acl: AgentForwardAcl,

    /// Remote app selection & launch on virtual-desktop hosts. Default:
    /// enabled with a seeded bash/tmux entry so a headless VD host offers
    /// "New bash session" out of the box. Operators add htop/mc/… per host
    /// in the TOML. The browser only ever sends an allowlist KEY, never a
    /// command line. See `agents/roomler-agent/src/apps/`.
    #[serde(default)]
    pub virtual_desktop_apps: crate::apps::VirtualDesktopAppsConfig,

    /// Phase 3b: opt into the overlay L3 mesh. Default off — an
    /// `overlay-l3` build only joins the mesh when this is set.
    #[serde(default)]
    pub overlay_enabled: bool,

    /// Multi-org P2c: let SECONDARY `[[orgs]]` entries with
    /// `overlay_mode = "tun"` join their own tenant's mesh over the ONE
    /// shared TUN device (per-org facades + destination demux — see
    /// `tunnel_core::overlay::tun_mux`). Default off: the P1 stance
    /// (secondaries never join) until the operator opts in. Requires the
    /// secondary to share the primary's `server_url` (same control plane)
    /// and to hold its own WG key; a tenant still on the shared legacy
    /// `/10` can coexist with carved-block orgs, but TWO un-migrated
    /// tenants on one host are refused at TUN registration (renumber one —
    /// `docs/multi-org.md` §4.3). Mutually exclusive with netstack mode
    /// (`ROOMLER_AGENT_OVERLAY_NETSTACK_SOCKS`): when this is on, the OS
    /// TUN path is used regardless.
    #[serde(default)]
    pub overlay_multi_org: bool,

    /// Phase 3b: this node's persisted WireGuard Curve25519 secret key
    /// (base64). Generated on the first overlay-enabled startup in `main`;
    /// the public key is what the netmap distributes. `None` until then.
    #[serde(default)]
    pub overlay_wg_secret_key: Option<String>,

    /// Phase 1 subnet router — CIDRs this node offers to route for overlay peers
    /// (e.g. `["192.168.1.0/24"]`). Sent on join as `advertised_routes`; each is
    /// gated behind admin approval server-side before any peer uses it. Empty =
    /// this node is not a subnet router.
    #[serde(default)]
    pub overlay_advertised_routes: Vec<String>,

    /// P5 exit-node: when set, this node advertises `0.0.0.0/0` (the default
    /// route) on the L3 overlay, offering to be an **exit node** — a mesh peer
    /// can route ALL its internet egress through this host (Tailscale exit-node
    /// style). Like every advertised route it stays admin-gated server-side (a
    /// DISTINCT Admin → Subnet routes "Exit node" toggle, not a stray `/0`
    /// checkbox in the CIDR grid) and no peer routes through it until BOTH this
    /// flag AND the admin approval are set. The advertised `0.0.0.0/0` unions
    /// with `overlay_advertised_routes` — see
    /// [`AgentConfig::effective_overlay_advertised_routes`]. The egress
    /// data-plane (NAT masquerade + FORWARD ACCEPT) engages automatically via
    /// the existing subnet-router NAT once `/0` is advertised. Off by default.
    #[serde(default)]
    pub overlay_exit_node_enabled: bool,

    /// P5 exit-node CLIENT opt-in: route THIS node's default internet egress
    /// (`0.0.0.0/0`) through the named mesh peer — its [`overlay`] `NetmapPeer`
    /// name or node-id hex — Tailscale "use exit node" style. `None` (default) =
    /// normal routing (egress via the local uplink). Only takes effect when the
    /// named peer is visible in the netmap, reachable with a live carrier, AND an
    /// admin-approved exit node; the client then installs a split-default with
    /// carrier-endpoint exemptions so the coordination + mesh path survives.
    /// DISTINCT from [`overlay_exit_node_enabled`](Self::overlay_exit_node_enabled),
    /// which makes THIS node OFFER to be an exit for others.
    #[serde(default)]
    pub overlay_exit_node: Option<String>,

    /// Tunnel mesh subnet-router — CIDRs this host advertises it can route for
    /// the SOCKS mesh (e.g. `["192.168.1.0/24"]`). Sent on `rc:agent.hello` as
    /// `advertised_routes`; an admin approves a subset (Admin → Agents → Subnet
    /// routes) before the mesh uses them. Separate from
    /// `overlay_advertised_routes` (the L3 overlay's own subnet router).
    #[serde(default)]
    pub advertise_routes: Vec<String>,

    /// Auto-detect this host's directly-connected IPv4 subnets and advertise
    /// them alongside `advertise_routes` (union). Default ON: a subnet router
    /// is zero-config — the admin sees each LAN the host is on as a suggestion
    /// (Admin → Agents → Subnet routes) and approves what should be routed. Set
    /// `false` to advertise only the explicit `advertise_routes`. Detected
    /// routes are UNTRUSTED until approved, so this is safe to leave on.
    #[serde(default = "default_true")]
    pub advertise_local_subnets: bool,

    /// P6: DECLARED, daemon-supervised tunnel routes (`[[tunnel_routes]]`).
    /// Each enabled entry is reconciled into a live daemon flow on every
    /// startup (and on change) by `tunnel::route_reconciler` — the
    /// persistent counterpart of the ephemeral LocalAPI `CreateForward`
    /// flows. The struct is `tunnel_core::localapi::RouteDescriptor`, one
    /// type for wire + disk. Managed via `roomler route add/rm/...` or the
    /// desktop Tunnels pane (the DAEMON writes this field — LocalAPI verbs
    /// persist through the daemon's config-write lock); hand-editing the
    /// TOML also works (picked up at the next daemon start).
    ///
    /// NB: a crash-rollback to a pre-P6 binary rewrites the config without
    /// this field (no unknown-field preservation) — declared routes do not
    /// survive an auto-rollback.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tunnel_routes: Vec<tunnel_core::localapi::RouteDescriptor>,

    /// Multi-org P1: SECONDARY enrollments (`[[orgs]]`). The top-level scalar
    /// identity fields (`server_url` / `agent_token` / `agent_id` /
    /// `tenant_id`) REMAIN the PRIMARY org — deliberately, so a rollback to a
    /// pre-multi-org binary keeps running the primary unchanged and simply
    /// never reads this table. Each enabled entry gets its own supervised
    /// signaling WS loop in `run_cmd`; `machine_id` / `machine_name` are
    /// machine-scoped and shared by every org (the server dedupes per tenant
    /// via the `agents.{tenant_id, machine_id}` unique index).
    ///
    /// Managed by `roomler-agent enroll` (appends when the enrollment
    /// resolves to a NEW (server, tenant) pair) and the `roomler-agent org`
    /// verbs. Deliberately NOT on the S2 config surface — same policy as
    /// `[[tunnel_routes]]`: identity + secrets live behind dedicated verbs.
    ///
    /// NB: same rollback caveat as `tunnel_routes` — a crash-rollback to a
    /// pre-multi-org binary rewrites the config without this field, so
    /// secondary enrollments do not survive an auto-rollback.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub orgs: Vec<OrgEntry>,
}

/// Multi-org P1 — one SECONDARY enrollment (see [`AgentConfig::orgs`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OrgEntry {
    /// Operator-facing slug, unique among entries and never
    /// [`PRIMARY_ORG_LABEL`]. Used by the `org` CLI verbs, log/tracing
    /// fields, the per-org watchdog pump name, and `[[tunnel_routes]].org`.
    pub label: String,
    /// Base URL of this org's Roomler API. May differ from the primary's
    /// (self-hosted servers); overlay `tun` mode is reserved for orgs sharing
    /// the primary's control plane (enforced in P2 — P1 forces `off`).
    pub server_url: String,
    /// Derived WSS URL; recomputed from `server_url` if absent.
    #[serde(default)]
    pub ws_url: Option<String>,
    /// This org's long-lived agent JWT.
    pub agent_token: String,
    /// This org's server-assigned agent id (hex ObjectId).
    pub agent_id: String,
    /// This org's tenant id (hex ObjectId).
    pub tenant_id: String,
    /// Soft-disable: a disabled org keeps its enrollment but gets no
    /// supervised WS loop until re-enabled (applied at the next daemon
    /// start, like every config key).
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// How this org joins the L3 overlay. P1 supervisors force `Off`
    /// regardless of this value; P2 honours `netstack`/`tun` (with `tun`
    /// restricted to same-control-plane orgs + tenant-block addressing).
    #[serde(default)]
    pub overlay_mode: OrgOverlayMode,
    /// This org's OWN WireGuard secret (base64). NEVER copied from the
    /// primary or another org — a shared public key would let two orgs
    /// correlate the same physical device. Minted at enroll-append time
    /// (or on this org's first overlay-enabled start, P2).
    #[serde(default)]
    pub overlay_wg_secret_key: Option<String>,
    /// Org-scoped twin of `overlay_advertised_routes` (P2).
    #[serde(default)]
    pub overlay_advertised_routes: Vec<String>,
    /// Org-scoped twin of `overlay_exit_node_enabled` (P2; exit offering is
    /// single-owner across orgs — arbitrated at bring-up).
    #[serde(default)]
    pub overlay_exit_node_enabled: bool,
    /// Org-scoped twin of `advertise_routes` (tunnel/SOCKS mesh subnet
    /// advertisements are per-tenant admin-approved).
    #[serde(default)]
    pub advertise_routes: Vec<String>,
}

impl OrgEntry {
    pub fn ws_url(&self) -> String {
        if let Some(url) = &self.ws_url {
            return url.clone();
        }
        derive_ws_url(&self.server_url)
    }
}

/// Multi-org P1 — per-org overlay participation. Multi-state, so an enum
/// (never a tribool): `off` (P1 default for secondaries) | `netstack`
/// (userspace stack — no TUN/OS routes; P2) | `tun` (full OS-TUN presence;
/// P2, same-control-plane orgs only).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OrgOverlayMode {
    #[default]
    Off,
    Netstack,
    Tun,
}

/// The reserved label naming the config's scalar (primary) identity.
pub const PRIMARY_ORG_LABEL: &str = "primary";

impl AgentConfig {
    /// Enabled secondary enrollments, in config order.
    pub fn enabled_secondary_orgs(&self) -> impl Iterator<Item = &OrgEntry> {
        self.orgs.iter().filter(|o| o.enabled)
    }

    /// Find a secondary org by label (case-sensitive).
    pub fn find_org(&self, label: &str) -> Option<&OrgEntry> {
        self.orgs.iter().find(|o| o.label == label)
    }

    /// Find a secondary org by its enrollment identity — the (server, tenant)
    /// pair that `enroll` resolves against.
    pub fn find_org_by_identity_mut(
        &mut self,
        server_url: &str,
        tenant_id: &str,
    ) -> Option<&mut OrgEntry> {
        self.orgs
            .iter_mut()
            .find(|o| o.server_url == server_url && o.tenant_id == tenant_id)
    }

    /// Does the (server, tenant) pair name the PRIMARY enrollment?
    pub fn is_primary_identity(&self, server_url: &str, tenant_id: &str) -> bool {
        self.server_url == server_url && self.tenant_id == tenant_id
    }

    /// The AgentConfig an org supervisor runs with: machine identity +
    /// operator knobs from `self`, enrollment identity + org-scoped fields
    /// from the entry.
    ///
    /// Overlay participation for a secondary (P2c lifts the P1 force-off):
    /// requires ALL of `overlay_multi_org` (the operator opt-in),
    /// `overlay_mode = "tun"` on the entry, the SAME `server_url` as the
    /// primary (one control plane — the shared TUN's demux is only decidable
    /// against blocks one registry carved), and the org's OWN WG key (minted
    /// at enroll-append; NEVER the primary's). Anything else keeps the P1
    /// behaviour: overlay off for that org. Netstack mode for secondaries
    /// stays unimplemented (the netstack statics are process-global), and
    /// exit-node roles stay primary-only (split-defaults are host-global).
    /// Declared tunnel routes are reconciled only by the primary-side
    /// reconciler.
    pub fn for_org(&self, org: &OrgEntry) -> AgentConfig {
        let mut c = self.clone();
        c.server_url = org.server_url.clone();
        c.ws_url = org.ws_url.clone();
        c.agent_token = org.agent_token.clone();
        c.agent_id = org.agent_id.clone();
        c.tenant_id = org.tenant_id.clone();
        c.overlay_enabled = self.overlay_multi_org
            && org.overlay_mode == OrgOverlayMode::Tun
            && org.server_url == self.server_url
            && org.overlay_wg_secret_key.is_some();
        c.overlay_wg_secret_key = org.overlay_wg_secret_key.clone();
        c.overlay_advertised_routes = org.overlay_advertised_routes.clone();
        c.overlay_exit_node_enabled = false;
        c.overlay_exit_node = None;
        c.advertise_routes = org.advertise_routes.clone();
        c.tunnel_routes = Vec::new();
        c.orgs = Vec::new();
        c
    }

    /// Per-entry runnability partition for the daemon supervisor: each
    /// `[[orgs]]` entry paired with `None` (runnable) or `Some(reason)` (a
    /// validation problem — the supervisor skips it and surfaces the reason
    /// in the LocalAPI `OrgStatus.terminal_error`). Never fatal: a bad
    /// hand-edited entry must not take down the healthy orgs.
    pub fn partition_runnable_orgs(&self) -> Vec<(OrgEntry, Option<String>)> {
        let mut out = Vec::with_capacity(self.orgs.len());
        let mut seen_labels = std::collections::HashSet::new();
        let mut seen_identities = std::collections::HashSet::new();
        for o in &self.orgs {
            let problem = if sanitize_org_label(&o.label).as_deref() != Some(o.label.as_str()) {
                Some(format!("invalid org label {:?}", o.label))
            } else if !seen_labels.insert(o.label.clone()) {
                Some(format!("duplicate org label {:?}", o.label))
            } else if self.is_primary_identity(&o.server_url, &o.tenant_id) {
                Some("duplicates the primary enrollment".to_string())
            } else if !seen_identities.insert((o.server_url.clone(), o.tenant_id.clone())) {
                Some("duplicates another entry's (server, tenant) pair".to_string())
            } else {
                None
            };
            out.push((o.clone(), problem));
        }
        out
    }

    /// Validate the `[[orgs]]` table. Returns human-readable problems; the
    /// caller logs them and SKIPS the offending entries (never fatal — a bad
    /// hand-edited entry must not take down the healthy orgs).
    pub fn validate_orgs(&self) -> Vec<String> {
        let mut problems = Vec::new();
        let mut seen_labels = std::collections::HashSet::new();
        let mut seen_identities = std::collections::HashSet::new();
        for o in &self.orgs {
            if sanitize_org_label(&o.label).as_deref() != Some(o.label.as_str()) {
                problems.push(format!(
                    "org label {:?} is invalid (lowercase alphanumeric + dashes, \
                     not {PRIMARY_ORG_LABEL:?})",
                    o.label
                ));
            }
            if !seen_labels.insert(o.label.clone()) {
                problems.push(format!("duplicate org label {:?}", o.label));
            }
            let identity = (o.server_url.clone(), o.tenant_id.clone());
            if self.is_primary_identity(&o.server_url, &o.tenant_id) {
                problems.push(format!(
                    "org {:?} duplicates the primary enrollment ({} / tenant {})",
                    o.label, o.server_url, o.tenant_id
                ));
            } else if !seen_identities.insert(identity) {
                problems.push(format!(
                    "org {:?} duplicates another entry's (server, tenant) pair",
                    o.label
                ));
            }
        }
        problems
    }
}

/// Multi-org P1 — swap a secondary enrollment into the PRIMARY (scalar)
/// slot; the demoted primary takes the promoted org's `[[orgs]]` position
/// under the promoted org's old label. Machine identity and operator knobs
/// stay put; the org-scoped fields (WG key, advertised routes, exit offer)
/// travel with their enrollments. The overlay TUN follows the primary slot,
/// so the swapped-in primary starts `overlay_enabled = false` (P1: the
/// operator opts back in explicitly) and the demoted entry parks at
/// `OrgOverlayMode::Off` until P2's per-org overlay lands.
pub fn promote_org_to_primary(cfg: &mut AgentConfig, label: &str) -> Result<()> {
    let Some(idx) = cfg.orgs.iter().position(|o| o.label == label) else {
        anyhow::bail!("no org labelled {label:?} — see `roomler-agent org ls`");
    };
    let entry = cfg.orgs.remove(idx);
    let demoted = OrgEntry {
        label: entry.label.clone(),
        server_url: cfg.server_url.clone(),
        ws_url: cfg.ws_url.clone(),
        agent_token: cfg.agent_token.clone(),
        agent_id: cfg.agent_id.clone(),
        tenant_id: cfg.tenant_id.clone(),
        enabled: true,
        overlay_mode: OrgOverlayMode::Off,
        overlay_wg_secret_key: cfg.overlay_wg_secret_key.clone(),
        overlay_advertised_routes: cfg.overlay_advertised_routes.clone(),
        overlay_exit_node_enabled: cfg.overlay_exit_node_enabled,
        advertise_routes: cfg.advertise_routes.clone(),
    };
    cfg.server_url = entry.server_url;
    cfg.ws_url = entry.ws_url;
    cfg.agent_token = entry.agent_token;
    cfg.agent_id = entry.agent_id;
    cfg.tenant_id = entry.tenant_id;
    cfg.overlay_wg_secret_key = entry.overlay_wg_secret_key;
    cfg.overlay_advertised_routes = entry.overlay_advertised_routes;
    cfg.overlay_exit_node_enabled = entry.overlay_exit_node_enabled;
    cfg.overlay_exit_node = None;
    cfg.advertise_routes = entry.advertise_routes;
    cfg.overlay_enabled = false;
    cfg.orgs.insert(idx, demoted);
    Ok(())
}

/// Normalize an operator-supplied org label: lowercase, spaces/`_` → `-`,
/// strip everything outside `[a-z0-9-]`, trim dashes, cap at 32 chars.
/// Returns `None` when nothing valid remains or the result would collide
/// with the reserved [`PRIMARY_ORG_LABEL`].
pub fn sanitize_org_label(raw: &str) -> Option<String> {
    let mut out = String::new();
    for ch in raw.trim().chars() {
        let ch = ch.to_ascii_lowercase();
        match ch {
            'a'..='z' | '0'..='9' => out.push(ch),
            ' ' | '_' | '-' | '.' if !out.ends_with('-') && !out.is_empty() => out.push('-'),
            _ => {}
        }
        if out.len() >= 32 {
            break;
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() || out == PRIMARY_ORG_LABEL {
        None
    } else {
        Some(out)
    }
}

impl AgentConfig {
    /// P5 — the effective set of CIDRs advertised on the L3 overlay: the
    /// configured [`overlay_advertised_routes`](Self::overlay_advertised_routes)
    /// unioned with the default route `0.0.0.0/0` when this node is an exit node
    /// ([`overlay_exit_node_enabled`](Self::overlay_exit_node_enabled)). This is
    /// what feeds the overlay runtime's `with_advertised_routes` — so the
    /// server sees `0.0.0.0/0` in `advertised_routes` and the admin UI offers
    /// the exit-node toggle. Order-preserving; `/0` is appended last and only if
    /// not already present (an operator may also list it explicitly).
    pub fn effective_overlay_advertised_routes(&self) -> Vec<String> {
        let mut routes = self.overlay_advertised_routes.clone();
        if self.overlay_exit_node_enabled {
            let default_route = "0.0.0.0/0".to_string();
            if !routes.contains(&default_route) {
                routes.push(default_route);
            }
        }
        routes
    }
}

/// serde default for `advertise_local_subnets` — auto-detect is ON by default.
fn default_true() -> bool {
    true
}

/// Current schema version. Bumped whenever [`migrate`] gains a new
/// step. Persisted into the config file by the migration so subsequent
/// runs short-circuit the migration check.
///
/// `0.3.0-rc.300` — multi-org P1: `[[orgs]]` secondary enrollments. No
/// structural rewrite is needed (the scalar identity fields REMAIN the
/// primary org, and `orgs` serde-defaults to empty), so the step is a
/// stamp-only pass that rewrites the file with the current in-memory
/// shape.
pub const CURRENT_SCHEMA_VERSION: &str = "0.3.0-rc.300";

/// Apply schema migrations to `cfg` in place. Returns `true` when the
/// caller should persist the mutated config via [`save`]. Safe to call
/// on a freshly-loaded config of any age — same-version configs return
/// `false` after a no-op pass.
///
/// Migrations applied (rc.18 set):
/// 1. Trim trailing slash on `server_url` (older enrollment flows
///    occasionally left one in; harmless until something concatenates
///    a path).
/// 2. If `last_known_good_version` is a pre-rc.18 string (any 0.1.x or
///    0.2.x), reset `crash_count = 0` so the historical counter
///    doesn't trip the rc.18 rollback path.
/// 3. Stamp `config_schema_version = Some(CURRENT_SCHEMA_VERSION)` if
///    not already current.
///
/// Note: defaults for new fields (`enable_remote_browse`,
/// `auto_grant_session`, etc.) are applied at deserialize time by
/// `#[serde(default = "fn")]`. The migration's job is to ensure the
/// on-disk file matches the in-memory shape — so a future operator
/// reading `config.toml` sees ALL fields the running agent actually
/// uses, not just the ones they explicitly set when first enrolling.
pub fn migrate(cfg: &mut AgentConfig) -> bool {
    if cfg
        .config_schema_version
        .as_deref()
        .is_some_and(|v| v == CURRENT_SCHEMA_VERSION)
    {
        // Already on this schema; no-op.
        return false;
    }

    let mut changed = false;

    // 1. Trim trailing slash on server_url.
    let trimmed = cfg.server_url.trim_end_matches('/').to_string();
    if trimmed != cfg.server_url {
        cfg.server_url = trimmed;
        changed = true;
    }

    // 2. Reset crash_count when last_known_good_version is from a
    //    pre-rc.18 branch. The rollback heuristic is keyed off this
    //    counter; carrying it across branches could trip rollback
    //    against a healthy rc.18 install.
    if let Some(ref v) = cfg.last_known_good_version
        && (v.starts_with("0.1.") || v.starts_with("0.2."))
        && cfg.crash_count > 0
    {
        cfg.crash_count = 0;
        cfg.last_crash_unix = 0;
        changed = true;
    }

    // 3. Stamp the new schema version. ALWAYS persist after any
    //    migration ran, even if no other field changed — that's how
    //    we mark the file as having been processed.
    if cfg.config_schema_version.as_deref() != Some(CURRENT_SCHEMA_VERSION) {
        cfg.config_schema_version = Some(CURRENT_SCHEMA_VERSION.to_string());
        changed = true;
    }

    changed
}

/// How long a fresh run must survive before we promote its version
/// to `last_known_good_version` and reset the crash counter. Five
/// minutes is enough to rule out "agent crashed in startup init"
/// while still catching "agent ran fine then deadlocked at session
/// 0" reasonably fast.
pub const CLEAN_RUN_THRESHOLD_SECS: u64 = 5 * 60;

/// How recent a prior crash has to be for the next crash to count
/// against the same window. Ten minutes — chosen so an agent that
/// dies on cold start, gets relaunched in 60 s, and dies again
/// within those ten minutes is recognised as a crash loop and
/// triggers rollback after a few iterations.
pub const CRASH_WINDOW_SECS: u64 = 10 * 60;

/// How many crashes inside `CRASH_WINDOW_SECS` trip the rollback
/// path. Three is the sweet spot — fewer would fire on a single
/// hardware glitch (driver crash, transient OOM); more leaves a
/// genuinely-broken release running longer than necessary.
pub const ROLLBACK_THRESHOLD_CRASHES: u32 = 3;

impl AgentConfig {
    pub fn ws_url(&self) -> String {
        if let Some(url) = &self.ws_url {
            return url.clone();
        }
        derive_ws_url(&self.server_url)
    }
}

/// Mark the start of a fresh run. Sets `last_run_unhealthy=true`
/// optimistically — flipped back to false by either
/// [`record_clean_run_at`] (after the clean-run threshold) or by
/// [`mark_clean_shutdown`] (Ctrl-C handler). Caller saves config.
pub fn mark_run_starting(cfg: &mut AgentConfig) {
    cfg.last_run_unhealthy = true;
}

/// Record that the current run survived long enough to be
/// considered healthy. Resets the crash counter, promotes the
/// running version to `last_known_good_version`, clears the
/// rollback-attempted flag (so future genuine crash loops can
/// trigger another rollback), and clears the unhealthy flag.
pub fn record_clean_run_at(cfg: &mut AgentConfig, current_version: &str) {
    cfg.crash_count = 0;
    cfg.last_crash_unix = 0;
    cfg.rollback_attempted = false;
    cfg.last_run_unhealthy = false;
    cfg.last_known_good_version = Some(current_version.to_string());
}

/// Mark a graceful shutdown. Equivalent to "the run was healthy
/// from the rollback-detector's POV" — clears the unhealthy flag
/// without resetting the crash counter (a brief healthy run after
/// 2 prior crashes shouldn't wipe history that hasn't yet hit the
/// rollback threshold).
pub fn mark_clean_shutdown(cfg: &mut AgentConfig) {
    cfg.last_run_unhealthy = false;
}

/// Record a crash at the given unix timestamp. Increments the
/// counter when the prior crash was within `CRASH_WINDOW_SECS` of
/// `now_unix`; otherwise starts a fresh crash window at 1.
pub fn record_crash_at(cfg: &mut AgentConfig, now_unix: u64) {
    let prior = cfg.last_crash_unix;
    let in_window = prior > 0 && now_unix.saturating_sub(prior) <= CRASH_WINDOW_SECS;
    cfg.crash_count = if in_window {
        cfg.crash_count.saturating_add(1)
    } else {
        1
    };
    cfg.last_crash_unix = now_unix;
}

/// Whether the current state recommends rolling back to
/// `last_known_good_version`. Caller is responsible for actually
/// invoking the rollback (we keep the predicate pure for testing).
pub fn should_rollback(cfg: &AgentConfig, current_version: &str, now_unix: u64) -> bool {
    if cfg.rollback_attempted {
        return false;
    }
    let Some(target) = cfg.last_known_good_version.as_deref() else {
        return false;
    };
    if target == current_version {
        return false;
    }
    if cfg.crash_count < ROLLBACK_THRESHOLD_CRASHES {
        return false;
    }
    cfg.last_crash_unix > 0 && now_unix.saturating_sub(cfg.last_crash_unix) <= CRASH_WINDOW_SECS
}

/// Mark that we just spawned a rollback installer. Sets
/// `rollback_attempted=true` so a same-cycle re-trigger is
/// suppressed.
pub fn mark_rollback_attempted(cfg: &mut AgentConfig) {
    cfg.rollback_attempted = true;
}

/// TOML-friendly mirror of `encode::EncoderPreference`. Kept separate so
/// the `encode` module stays CLI-independent and the config file survives
/// feature gating without needing the `mf-encoder` feature enabled to
/// parse.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EncoderPreferenceChoice {
    #[default]
    Auto,
    Hardware,
    Software,
}

/// Resolve the default config path. Can be overridden by `--config` on the CLI.
/// Default for `enable_remote_browse` — `true` so a 0.3.0 install
/// preserves self-controlled-host auto-grant semantics. Operators on
/// org-controlled fleets explicitly set `enable_remote_browse = false`
/// in `config.toml`. Hard-coded helper instead of a `bool::default()`
/// because that defaults to `false`.
fn default_enable_remote_browse() -> bool {
    true
}

/// Default for `auto_grant_session` — `true` for back-compat with
/// every pre-0.3.x agent which auto-granted unconditionally
/// (signaling.rs:365 TODO). Org-controlled fleets opt out via
/// `config.toml`. See [`AgentConfig::auto_grant_session`] for the
/// security model.
fn default_auto_grant_session() -> bool {
    true
}

/// Daemon-wide config-WRITE lock (P6). The daemon has several runtime
/// writers of `config.toml` — the clean-run promotion task, the graceful
/// shutdown path, and the route reconciler's LocalAPI verbs. Each does a
/// reload-modify-save; interleaved unlocked, one writer's full-struct save
/// silently drops another's just-written field. Every daemon-side runtime
/// writer must hold this lock across its load→mutate→save. (Cross-PROCESS
/// writers — tray, CLI, wizard — remain last-writer-wins on the file;
/// [`save`]'s atomic rename keeps a torn file impossible either way.)
pub type WriteLock = std::sync::Arc<tokio::sync::Mutex<()>>;

/// A fully-populated config for unit tests in other modules (the route
/// reconciler persists through real [`save`]/[`load`] round-trips).
#[cfg(test)]
pub fn test_fixture() -> AgentConfig {
    tests::fixture()
}

/// rc.280 — the operator-grade bool knobs bridged config→env (the S2
/// fallback map), ONE source: `main.rs` feeds
/// `tunnel_core::env::register_config_fallbacks` from this, and the
/// config-surface parity test (`env_bridge_pairs_have_surface_parity`) walks
/// it against the editable key list. Before this the list was a literal in
/// `main.rs` — a key added to the surface but missed there silently didn't
/// bridge (`roomler config set` wrote TOML the daemon then ignored).
/// Suffixes are the `ROOMLER_NODE_…` env suffixes (uppercase surface key).
pub fn env_bridge_bools(cfg: &AgentConfig) -> [(&'static str, Option<bool>); 21] {
    [
        ("SHARED_ENCODER", cfg.shared_encoder),
        ("OVERLAY_QUIC", cfg.overlay_quic),
        ("OVERLAY_DIRECT", cfg.overlay_direct),
        ("OVERLAY_DERP", cfg.overlay_derp),
        ("OVERLAY_MBB", cfg.overlay_mbb),
        ("OVERLAY_LAN_IFACE_FILTER", cfg.overlay_lan_iface_filter),
        ("OVERLAY_ROUTE_EVENTS", cfg.overlay_route_events),
        ("OVERLAY_RELAY_TLS", cfg.overlay_relay_tls),
        ("OVERLAY_TUN_STABLE_GUID", cfg.overlay_tun_stable_guid),
        ("OVERLAY_ROUTE_EVICT", cfg.overlay_route_evict),
        ("OVERLAY_TUN_PERSIST", cfg.overlay_tun_persist),
        ("OVERLAY_ROUTE_METRIC0", cfg.overlay_route_metric0),
        ("LOCAL_TURN", cfg.local_turn),
        ("DNS_AAAA", cfg.dns_aaaa),
        ("AUTO_UPDATE", cfg.auto_update),
        ("LOGS_UPLOAD_DISABLED", cfg.logs_upload_disabled),
        ("OVERLAY_TIER_DETECT", cfg.overlay_tier_detect),
        ("OVERLAY_RTT_Q", cfg.overlay_rtt_q),
        ("OVERLAY_UPWARD_PROBE", cfg.overlay_upward_probe),
        ("RELAY_PROBE", cfg.relay_probe),
        ("TEXT_MOD_NEUTRALIZE", cfg.text_mod_neutralize),
    ]
}

/// rc.280 — numeric twin of [`env_bridge_bools`] (decimal strings on the
/// same fallback map).
pub fn env_bridge_numerics(cfg: &AgentConfig) -> [(&'static str, Option<u32>); 6] {
    [
        ("RATE_FACTOR_H264", cfg.rate_factor_h264),
        ("RATE_FACTOR_HEVC", cfg.rate_factor_hevc),
        ("RATE_FACTOR_VP9", cfg.rate_factor_vp9),
        ("RATE_FACTOR_AV1", cfg.rate_factor_av1),
        ("RC_MAX_SESSIONS", cfg.rc_max_sessions),
        ("OVERLAY_DIRECT_PORT", cfg.overlay_direct_port),
    ]
}

pub fn default_config_path() -> Result<PathBuf> {
    let dirs =
        crate::appdirs::project_dirs().context("could not resolve a platform config directory")?;
    Ok(dirs.config_dir().join("config.toml"))
}

/// rc.52: machine-global config path —
/// `%PROGRAMDATA%\roomler\roomler-agent\config.toml`.
///
/// `default_config_path()` resolves to a per-USER profile
/// (`%APPDATA%` via `ProjectDirs`). A SystemContext worker runs as
/// LocalSystem and, crucially, must be able to load its config
/// BEFORE any interactive user logs in (the whole point of M3 A1
/// pre-logon control). A user-profile path is unreachable pre-logon;
/// `%PROGRAMDATA%` is machine-global and LocalSystem-readable with no
/// logged-in user. The perMachine + SystemContext installer writes
/// the enrolled config here; the worker's resolution ladder consults
/// it ahead of the (never-populated) SYSTEM-profile default.
///
/// Windows-only — there is no machine-global config concept on
/// Linux/macOS (the agent there is perUser). Returns `None` if
/// `%PROGRAMDATA%` can't be resolved (it always can on a sane
/// Windows install; the `C:\ProgramData` literal is the documented
/// fallback used elsewhere in the crate).
#[cfg(target_os = "windows")]
pub fn machine_global_config_path() -> PathBuf {
    crate::appdirs::machine_global_dir().join("config.toml")
}

pub fn load(path: &PathBuf) -> Result<AgentConfig> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading config at {}", path.display()))?;
    let cfg: AgentConfig =
        toml::from_str(&raw).with_context(|| format!("parsing config at {}", path.display()))?;
    Ok(cfg)
}

pub fn save(path: &PathBuf, cfg: &AgentConfig) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating parent dir {}", parent.display()))?;
    }
    let serialised = toml::to_string_pretty(cfg).context("serialising config")?;

    // ATOMIC (P6 hardening): write a sibling temp file, then rename over
    // the target. The file holds `agent_token` — a torn `fs::write`
    // (power loss / crash mid-write) used to brick enrollment. `rename`
    // within one directory is atomic on Unix and uses
    // MOVEFILE_REPLACE_EXISTING semantics on Windows. The temp name is
    // pid-suffixed so two PROCESSES (daemon + tray/CLI) can't collide on
    // the temp file itself; last-writer-wins on the rename is the
    // documented cross-process limitation.
    let tmp = path.with_extension(format!("toml.tmp.{}", std::process::id()));
    std::fs::write(&tmp, serialised)
        .with_context(|| format!("writing config temp file {}", tmp.display()))?;

    // Tighten permissions on Unix BEFORE the rename — the file holds a
    // bearer token, and the temp file must never be world-readable even
    // for an instant at the final path.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&tmp)?.permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(&tmp, perms)?;
    }

    std::fs::rename(&tmp, path).with_context(|| {
        // Best-effort cleanup so failed saves don't accrete temp files.
        let _ = std::fs::remove_file(&tmp);
        format!("renaming config into place at {}", path.display())
    })?;
    Ok(())
}

fn derive_ws_url(http_url: &str) -> String {
    let base = http_url.trim_end_matches('/');
    let ws = if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        base.to_string()
    };
    format!("{ws}/ws")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ws_url_from_https() {
        assert_eq!(
            derive_ws_url("https://roomler.live"),
            "wss://roomler.live/ws"
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn machine_global_config_path_under_programdata_roomler() {
        let p = machine_global_config_path();
        let s = p.to_string_lossy().to_lowercase();
        assert!(s.contains("roomler"), "path missing roomler: {s}");
        // S1b: appdirs resolves the NEW `roomler` segment on fresh/migrated
        // machines and the legacy `roomler-agent` on pre-migration hosts —
        // both are valid; the old exact-tail assertion failed on any clean
        // Windows box.
        assert!(
            s.ends_with(r"roomler\roomler-agent\config.toml")
                || s.ends_with(r"roomler\roomler\config.toml"),
            "unexpected tail: {s}"
        );
        // Distinct from the perUser default (which is under %APPDATA%).
        assert_ne!(p, default_config_path().unwrap());
    }

    #[test]
    fn ws_url_from_http_localhost() {
        assert_eq!(
            derive_ws_url("http://localhost:3000"),
            "ws://localhost:3000/ws"
        );
    }

    #[test]
    fn ws_url_strips_trailing_slash() {
        assert_eq!(
            derive_ws_url("https://roomler.live/"),
            "wss://roomler.live/ws"
        );
    }

    pub(super) fn fixture() -> AgentConfig {
        AgentConfig {
            server_url: "https://example.invalid".into(),
            ws_url: None,
            agent_token: "tok".into(),
            agent_id: "aid".into(),
            tenant_id: "tid".into(),
            machine_id: "mid".into(),
            machine_name: "host".into(),
            encoder_preference: EncoderPreferenceChoice::Auto,
            update_check_interval_h: None,
            enable_remote_browse: true,
            auto_grant_session: true,
            exec_enabled: false,
            overlay_quic: None,
            overlay_direct: None,
            overlay_derp: None,
            overlay_mbb: None,
            overlay_lan_iface_filter: None,
            overlay_pathmon: None,
            overlay_route_events: None,
            overlay_route_tick_secs: None,
            overlay_relay_tls: None,
            overlay_tun_stable_guid: None,
            overlay_route_evict: None,
            overlay_tun_persist: None,
            overlay_route_metric0: None,
            overlay_direct_port: None,
            local_turn: None,
            dns_aaaa: None,
            auto_update: None,
            logs_upload_disabled: None,
            rate_factor_h264: None,
            rate_factor_hevc: None,
            rate_factor_vp9: None,
            rate_factor_av1: None,
            ice_follow_renomination: None,
            ice_warm_standby: None,
            ice_overlay_host_deprioritize: None,
            overlay_tier_detect: None,
            overlay_rtt_q: None,
            relay_probe: None,
            text_mod_neutralize: None,
            overlay_demote: None,
            overlay_upward_probe: None,
            rc_max_sessions: None,
            shared_encoder: None,
            overlay_rpf: None,
            last_known_good_version: None,
            crash_count: 0,
            last_crash_unix: 0,
            rollback_attempted: false,
            last_run_unhealthy: false,
            config_schema_version: None,
            forward_acl: AgentForwardAcl::default(),
            virtual_desktop_apps: crate::apps::VirtualDesktopAppsConfig::default(),
            overlay_enabled: false,
            overlay_multi_org: false,
            overlay_wg_secret_key: None,
            overlay_advertised_routes: Vec::new(),
            overlay_exit_node_enabled: false,
            overlay_exit_node: None,
            advertise_routes: Vec::new(),
            advertise_local_subnets: true,
            tunnel_routes: Vec::new(),
            orgs: Vec::new(),
        }
    }

    /// A minimal valid secondary-org entry for tests.
    pub(super) fn org_fixture(label: &str, tenant: &str) -> OrgEntry {
        OrgEntry {
            label: label.to_string(),
            server_url: "https://example.invalid".into(),
            ws_url: None,
            agent_token: format!("tok-{label}"),
            agent_id: format!("aid-{label}"),
            tenant_id: tenant.to_string(),
            enabled: true,
            overlay_mode: OrgOverlayMode::Off,
            overlay_wg_secret_key: None,
            overlay_advertised_routes: Vec::new(),
            overlay_exit_node_enabled: false,
            advertise_routes: Vec::new(),
        }
    }

    #[test]
    fn orgs_toml_round_trip_preserves_entries() {
        let mut cfg = fixture();
        cfg.orgs = vec![
            org_fixture("acme", "tid-acme"),
            OrgEntry {
                enabled: false,
                overlay_mode: OrgOverlayMode::Netstack,
                overlay_wg_secret_key: Some("KEY-B".into()),
                overlay_advertised_routes: vec!["10.9.0.0/24".into()],
                advertise_routes: vec!["192.168.7.0/24".into()],
                ..org_fixture("beta-corp", "tid-beta")
            },
        ];
        let toml_str = toml::to_string_pretty(&cfg).expect("serialise");
        let back: AgentConfig = toml::from_str(&toml_str).expect("parse");
        assert_eq!(back.orgs, cfg.orgs, "orgs must survive a TOML round-trip");
        // The scalar (primary) identity is untouched by the table.
        assert_eq!(back.agent_id, cfg.agent_id);
        assert_eq!(back.tenant_id, cfg.tenant_id);
    }

    #[test]
    fn config_without_orgs_field_loads_and_serialises_without_it() {
        // Back-compat both ways: a legacy file has no `orgs` (defaults
        // empty), and a single-org config never writes the key (so a
        // rollback binary's parser has nothing to trip on).
        let raw = r#"
            server_url = "https://example.invalid"
            agent_token = "tok"
            agent_id = "aid"
            tenant_id = "tid"
            machine_id = "mid"
            machine_name = "host"
        "#;
        let cfg: AgentConfig = toml::from_str(raw).expect("legacy config must parse");
        assert!(cfg.orgs.is_empty());
        let out = toml::to_string_pretty(&cfg).expect("serialise");
        assert!(
            !out.contains("[[orgs]]"),
            "empty orgs table must not be written: {out}"
        );
    }

    #[test]
    fn for_org_swaps_identity_and_forces_overlay_off() {
        let mut cfg = fixture();
        cfg.overlay_enabled = true;
        cfg.overlay_wg_secret_key = Some("PRIMARY-KEY".into());
        cfg.overlay_exit_node = Some("exit-1".into());
        cfg.tunnel_routes = vec![tunnel_core::localapi::RouteDescriptor {
            id: "r1".into(),
            kind: tunnel_core::localapi::FlowKind::Forward,
            node: "aid".into(),
            local: 18080,
            remote: Some("127.0.0.1:80".into()),
            transport: String::new(),
            enabled: true,
            org: None,
        }];
        let mut org = org_fixture("acme", "tid-acme");
        org.overlay_wg_secret_key = Some("ACME-KEY".into());
        org.advertise_routes = vec!["10.1.0.0/24".into()];
        cfg.orgs = vec![org.clone()];

        let oc = cfg.for_org(&org);
        // Identity comes from the entry…
        assert_eq!(oc.server_url, org.server_url);
        assert_eq!(oc.agent_token, "tok-acme");
        assert_eq!(oc.agent_id, "aid-acme");
        assert_eq!(oc.tenant_id, "tid-acme");
        // …machine identity + operator knobs are shared…
        assert_eq!(oc.machine_id, cfg.machine_id);
        assert_eq!(oc.machine_name, cfg.machine_name);
        // …the org's own WG key rides along (never the primary's)…
        assert_eq!(oc.overlay_wg_secret_key.as_deref(), Some("ACME-KEY"));
        assert_eq!(oc.advertise_routes, vec!["10.1.0.0/24".to_string()]);
        // …and P1 forces the overlay + route surfaces off/empty.
        assert!(!oc.overlay_enabled, "P1: secondary overlay must be OFF");
        assert!(!oc.overlay_exit_node_enabled);
        assert!(oc.overlay_exit_node.is_none());
        assert!(oc.tunnel_routes.is_empty());
        assert!(oc.orgs.is_empty(), "no recursive org table");
    }

    /// P2c — the secondary-overlay gate lifts ONLY when every condition
    /// holds: the operator flag, `overlay_mode = tun`, the primary's own
    /// control plane, and the org's own WG key. Each missing leg keeps the
    /// P1 behaviour for exactly that org.
    #[test]
    fn for_org_lifts_overlay_only_under_the_p2c_gate() {
        let mut cfg = fixture();
        cfg.overlay_multi_org = true;
        let mut org = org_fixture("acme", "tid-acme");
        org.overlay_mode = OrgOverlayMode::Tun;
        org.server_url = cfg.server_url.clone(); // same control plane
        org.overlay_wg_secret_key = Some("ACME-KEY".into());

        // All four conditions hold ⇒ the secondary joins.
        assert!(cfg.for_org(&org).overlay_enabled, "full gate ⇒ ON");

        // Flag off ⇒ P1 behaviour, everything else unchanged.
        cfg.overlay_multi_org = false;
        assert!(!cfg.for_org(&org).overlay_enabled, "no operator opt-in");
        cfg.overlay_multi_org = true;

        // A foreign control plane can't ride the shared TUN (its blocks come
        // from a different registry — the demux would be undecidable).
        let mut foreign = org.clone();
        foreign.server_url = "https://other.invalid".into();
        assert!(!cfg.for_org(&foreign).overlay_enabled, "foreign server");

        // Netstack / off modes stay off (netstack statics are process-global).
        let mut ns = org.clone();
        ns.overlay_mode = OrgOverlayMode::Netstack;
        assert!(!cfg.for_org(&ns).overlay_enabled, "netstack secondary");
        let mut off = org.clone();
        off.overlay_mode = OrgOverlayMode::Off;
        assert!(!cfg.for_org(&off).overlay_enabled, "mode off");

        // No per-org WG key ⇒ can't join (and must never borrow the
        // primary's — cross-org pubkey correlation).
        let mut keyless = org.clone();
        keyless.overlay_wg_secret_key = None;
        let oc = cfg.for_org(&keyless);
        assert!(!oc.overlay_enabled, "keyless org stays off");
        assert!(
            oc.overlay_wg_secret_key.is_none(),
            "never the primary's key"
        );

        // Exit roles remain primary-only regardless of the gate.
        let on = cfg.for_org(&org);
        assert!(!on.overlay_exit_node_enabled);
        assert!(on.overlay_exit_node.is_none());
    }

    #[test]
    fn validate_orgs_flags_dupes_and_bad_labels() {
        let mut cfg = fixture();
        cfg.orgs = vec![
            org_fixture("acme", "tid-a"),
            org_fixture("acme", "tid-b"),    // dup label
            org_fixture("primary", "tid-c"), // reserved
            org_fixture("beta", "tid"), // duplicates the PRIMARY identity (fixture tenant "tid")
            org_fixture("gamma", "tid-a"), // dup (server, tenant) with the first entry
        ];
        let problems = cfg.validate_orgs();
        assert!(problems.iter().any(|p| p.contains("duplicate org label")));
        assert!(problems.iter().any(|p| p.contains("\"primary\"")));
        assert!(
            problems.iter().any(|p| p.contains("primary enrollment")),
            "{problems:?}"
        );
        assert!(
            problems
                .iter()
                .any(|p| p.contains("another entry's (server, tenant)")),
            "{problems:?}"
        );

        let clean = AgentConfig {
            orgs: vec![org_fixture("acme", "tid-a"), org_fixture("beta", "tid-b")],
            ..fixture()
        };
        assert!(clean.validate_orgs().is_empty());
    }

    #[test]
    fn promote_org_swaps_identity_and_org_scoped_fields() {
        let mut cfg = fixture(); // primary: example.invalid / tid / tok
        cfg.overlay_enabled = true;
        cfg.overlay_wg_secret_key = Some("PRIM-KEY".into());
        cfg.overlay_advertised_routes = vec!["10.0.0.0/24".into()];
        cfg.overlay_exit_node = Some("exit-1".into());
        cfg.encoder_preference = EncoderPreferenceChoice::Software;
        let mut acme = org_fixture("acme", "tid-acme");
        acme.server_url = "https://acme.invalid".into();
        acme.overlay_wg_secret_key = Some("ACME-KEY".into());
        acme.advertise_routes = vec!["192.168.9.0/24".into()];
        cfg.orgs = vec![org_fixture("zeta", "tid-z"), acme];

        promote_org_to_primary(&mut cfg, "acme").unwrap();

        // Promoted identity + org-scoped fields are now the scalars…
        assert_eq!(cfg.server_url, "https://acme.invalid");
        assert_eq!(cfg.agent_token, "tok-acme");
        assert_eq!(cfg.agent_id, "aid-acme");
        assert_eq!(cfg.tenant_id, "tid-acme");
        assert_eq!(cfg.overlay_wg_secret_key.as_deref(), Some("ACME-KEY"));
        assert_eq!(cfg.advertise_routes, vec!["192.168.9.0/24".to_string()]);
        // …the TUN opt-in resets (operator re-opts per the P1 contract)…
        assert!(!cfg.overlay_enabled);
        assert!(cfg.overlay_exit_node.is_none());
        // …operator knobs stay put…
        assert!(matches!(
            cfg.encoder_preference,
            EncoderPreferenceChoice::Software
        ));
        // …and the demoted primary sits in acme's old slot under acme's label.
        assert_eq!(cfg.orgs.len(), 2);
        assert_eq!(cfg.orgs[0].label, "zeta");
        let demoted = &cfg.orgs[1];
        assert_eq!(demoted.label, "acme");
        assert_eq!(demoted.server_url, "https://example.invalid");
        assert_eq!(demoted.tenant_id, "tid");
        assert_eq!(demoted.agent_token, "tok");
        assert_eq!(demoted.overlay_wg_secret_key.as_deref(), Some("PRIM-KEY"));
        assert_eq!(demoted.overlay_mode, OrgOverlayMode::Off);
        assert!(demoted.enabled);

        // Unknown label is a hard error, config untouched.
        assert!(promote_org_to_primary(&mut cfg, "ghost").is_err());
    }

    #[test]
    fn sanitize_org_label_rules() {
        assert_eq!(sanitize_org_label(" Acme Corp "), Some("acme-corp".into()));
        assert_eq!(sanitize_org_label("roomler.ai"), Some("roomler-ai".into()));
        assert_eq!(sanitize_org_label("Ümlaut__x"), Some("mlaut-x".into()));
        assert_eq!(sanitize_org_label("primary"), None, "reserved");
        assert_eq!(sanitize_org_label("PRIMARY"), None, "reserved, any case");
        assert_eq!(sanitize_org_label("###"), None);
        assert_eq!(sanitize_org_label(""), None);
    }

    #[test]
    fn effective_overlay_routes_appends_default_route_for_exit_node() {
        let mut cfg = fixture();
        // Not an exit node → advertised routes pass through unchanged.
        cfg.overlay_advertised_routes = vec!["192.168.1.0/24".into()];
        assert_eq!(
            cfg.effective_overlay_advertised_routes(),
            vec!["192.168.1.0/24".to_string()]
        );

        // Exit node → 0.0.0.0/0 unioned in, appended last.
        cfg.overlay_exit_node_enabled = true;
        assert_eq!(
            cfg.effective_overlay_advertised_routes(),
            vec!["192.168.1.0/24".to_string(), "0.0.0.0/0".to_string()]
        );

        // An explicitly-listed /0 is not duplicated.
        cfg.overlay_advertised_routes = vec!["0.0.0.0/0".into()];
        assert_eq!(
            cfg.effective_overlay_advertised_routes(),
            vec!["0.0.0.0/0".to_string()]
        );

        // Pure exit node (no subnet routes) → just the default route.
        cfg.overlay_advertised_routes = Vec::new();
        assert_eq!(
            cfg.effective_overlay_advertised_routes(),
            vec!["0.0.0.0/0".to_string()]
        );
    }

    #[test]
    fn record_clean_run_resets_counter_and_promotes_version() {
        let mut cfg = fixture();
        cfg.crash_count = 4;
        cfg.last_crash_unix = 1_000;
        cfg.rollback_attempted = true;
        record_clean_run_at(&mut cfg, "0.1.50");
        assert_eq!(cfg.crash_count, 0);
        assert_eq!(cfg.last_crash_unix, 0);
        assert!(!cfg.rollback_attempted);
        assert_eq!(cfg.last_known_good_version.as_deref(), Some("0.1.50"));
    }

    #[test]
    fn record_crash_starts_window_at_one() {
        let mut cfg = fixture();
        record_crash_at(&mut cfg, 1_000_000);
        assert_eq!(cfg.crash_count, 1);
        assert_eq!(cfg.last_crash_unix, 1_000_000);
    }

    #[test]
    fn record_crash_increments_when_within_window() {
        let mut cfg = fixture();
        record_crash_at(&mut cfg, 1_000_000);
        record_crash_at(&mut cfg, 1_000_060); // +60s, in window
        record_crash_at(&mut cfg, 1_000_300); // +300s, still in window (10 min)
        assert_eq!(cfg.crash_count, 3);
        assert_eq!(cfg.last_crash_unix, 1_000_300);
    }

    #[test]
    fn record_crash_resets_when_outside_window() {
        let mut cfg = fixture();
        record_crash_at(&mut cfg, 1_000_000);
        record_crash_at(&mut cfg, 1_000_060);
        // +700s = 11 min 40s — outside the 10-min window.
        record_crash_at(&mut cfg, 1_000_760);
        assert_eq!(cfg.crash_count, 1, "counter resets on a fresh window");
        assert_eq!(cfg.last_crash_unix, 1_000_760);
    }

    #[test]
    fn should_rollback_false_when_no_known_good() {
        let mut cfg = fixture();
        cfg.crash_count = 5;
        cfg.last_crash_unix = 1_000_000;
        assert!(!should_rollback(&cfg, "0.1.51", 1_000_001));
    }

    #[test]
    fn should_rollback_false_when_under_threshold() {
        let mut cfg = fixture();
        cfg.last_known_good_version = Some("0.1.50".into());
        cfg.crash_count = 2; // threshold is 3
        cfg.last_crash_unix = 1_000_000;
        assert!(!should_rollback(&cfg, "0.1.51", 1_000_001));
    }

    #[test]
    fn should_rollback_false_when_target_equals_current() {
        // Refusing this case prevents a same-version-rollback loop.
        let mut cfg = fixture();
        cfg.last_known_good_version = Some("0.1.51".into());
        cfg.crash_count = 5;
        cfg.last_crash_unix = 1_000_000;
        assert!(!should_rollback(&cfg, "0.1.51", 1_000_001));
    }

    #[test]
    fn should_rollback_false_when_window_expired() {
        // A flaky day that adds 3 unrelated crashes over a week
        // shouldn't trigger rollback.
        let mut cfg = fixture();
        cfg.last_known_good_version = Some("0.1.50".into());
        cfg.crash_count = 3;
        cfg.last_crash_unix = 1_000_000;
        // +700s — outside CRASH_WINDOW_SECS.
        assert!(!should_rollback(&cfg, "0.1.51", 1_000_700));
    }

    #[test]
    fn should_rollback_true_in_active_window_above_threshold() {
        let mut cfg = fixture();
        cfg.last_known_good_version = Some("0.1.50".into());
        cfg.crash_count = 3;
        cfg.last_crash_unix = 1_000_000;
        assert!(should_rollback(&cfg, "0.1.51", 1_000_030));
    }

    #[test]
    fn should_rollback_false_when_already_attempted() {
        let mut cfg = fixture();
        cfg.last_known_good_version = Some("0.1.50".into());
        cfg.crash_count = 5;
        cfg.last_crash_unix = 1_000_000;
        cfg.rollback_attempted = true;
        assert!(
            !should_rollback(&cfg, "0.1.51", 1_000_001),
            "must not oscillate between bad versions"
        );
    }

    #[test]
    fn mark_run_starting_sets_unhealthy_flag() {
        let mut cfg = fixture();
        assert!(!cfg.last_run_unhealthy);
        mark_run_starting(&mut cfg);
        assert!(cfg.last_run_unhealthy);
    }

    #[test]
    fn record_clean_run_clears_unhealthy_flag() {
        let mut cfg = fixture();
        mark_run_starting(&mut cfg);
        record_clean_run_at(&mut cfg, "0.1.50");
        assert!(!cfg.last_run_unhealthy);
        assert_eq!(cfg.last_known_good_version.as_deref(), Some("0.1.50"));
    }

    #[test]
    fn mark_clean_shutdown_clears_only_unhealthy() {
        // Clean shutdown after 2 prior crashes shouldn't wipe the
        // counter — those still represent a crash window that the
        // 3rd crash should escalate.
        let mut cfg = fixture();
        cfg.crash_count = 2;
        cfg.last_crash_unix = 1_000_000;
        mark_run_starting(&mut cfg);
        mark_clean_shutdown(&mut cfg);
        assert!(!cfg.last_run_unhealthy);
        assert_eq!(cfg.crash_count, 2, "clean shutdown preserves crash history");
        assert_eq!(cfg.last_crash_unix, 1_000_000);
    }

    // ---- Migration tests (rc.18 P4) ---------------------------------

    #[test]
    fn migrate_pre_rc18_stamps_schema_version() {
        // Old config has no version field. Migration runs the rc.18
        // step set and stamps CURRENT_SCHEMA_VERSION so subsequent
        // launches no-op.
        let mut cfg = fixture();
        assert!(cfg.config_schema_version.is_none());
        let changed = migrate(&mut cfg);
        assert!(changed, "first migration must rewrite the config");
        assert_eq!(
            cfg.config_schema_version.as_deref(),
            Some(CURRENT_SCHEMA_VERSION)
        );
    }

    #[test]
    fn migrate_same_schema_is_noop() {
        // Second launch on the same version: migrate returns false,
        // caller skips the save.
        let mut cfg = fixture();
        cfg.config_schema_version = Some(CURRENT_SCHEMA_VERSION.to_string());
        let changed = migrate(&mut cfg);
        assert!(!changed, "same-version migration must be a no-op");
    }

    #[test]
    fn migrate_trims_trailing_slash_on_server_url() {
        let mut cfg = fixture();
        cfg.server_url = "https://example.invalid/".into();
        let changed = migrate(&mut cfg);
        assert!(changed);
        assert_eq!(cfg.server_url, "https://example.invalid");
    }

    #[test]
    fn migrate_no_trailing_slash_to_trim() {
        // server_url already clean; ONLY the schema version stamp
        // counts as a change.
        let mut cfg = fixture();
        cfg.server_url = "https://example.invalid".into();
        let changed = migrate(&mut cfg);
        assert!(changed); // schema version stamp
        assert_eq!(cfg.server_url, "https://example.invalid");
    }

    #[test]
    fn migrate_resets_crash_count_from_pre_rc18_branch() {
        // last_known_good_version from 0.2.x with a live crash counter:
        // those crashes happened on a different branch, the counter
        // must not trip rollback against rc.18.
        let mut cfg = fixture();
        cfg.last_known_good_version = Some("0.2.7".into());
        cfg.crash_count = 2;
        cfg.last_crash_unix = 1_700_000_000;
        let changed = migrate(&mut cfg);
        assert!(changed);
        assert_eq!(cfg.crash_count, 0);
        assert_eq!(cfg.last_crash_unix, 0);
    }

    #[test]
    fn migrate_preserves_crash_count_for_same_branch() {
        // 0.3.x crash counter is still relevant when running 0.3.x —
        // don't wipe it.
        let mut cfg = fixture();
        cfg.last_known_good_version = Some("0.3.0-rc.16".into());
        cfg.crash_count = 1;
        cfg.last_crash_unix = 1_700_000_000;
        migrate(&mut cfg);
        assert_eq!(cfg.crash_count, 1, "0.3.x history must be preserved");
        assert_eq!(cfg.last_crash_unix, 1_700_000_000);
    }

    #[test]
    fn old_config_without_new_fields_loads_with_defaults() {
        // Backwards-compat: a config.toml written by a pre-0.1.51
        // agent must continue to load.
        let raw = r#"
            server_url = "https://example.invalid"
            agent_token = "tok"
            agent_id = "aid"
            tenant_id = "tid"
            machine_id = "mid"
            machine_name = "host"
        "#;
        let cfg: AgentConfig = toml::from_str(raw).expect("legacy config must parse");
        assert_eq!(cfg.crash_count, 0);
        assert_eq!(cfg.last_crash_unix, 0);
        assert!(!cfg.rollback_attempted);
        assert!(cfg.last_known_good_version.is_none());
    }
}
