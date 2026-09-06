// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
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
    /// FR-51 P4 — this daemon's PRIMARY enrollment is ephemeral: the server
    /// reaps the device after silence, and the daemon de-enrolls itself on a
    /// clean stop. Additive both ways: an older daemon omits it (⇒ false),
    /// an older CLI ignores it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub ephemeral: bool,
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
    /// NAT-traversal — the outcome of this node's srflx (STUN) gather, copied
    /// verbatim from [`OverlayView::srflx`]. `None` from a daemon that predates
    /// the field or one with the overlay off. An **empty `candidates`** list is
    /// the fleet-health signal: this node cannot hole-punch, and every peer
    /// reads it as UDP-blocked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub srflx: Option<SrflxStatus>,
    /// C4 stage 1 — the standing warm TURN/UDP allocation's state, copied
    /// verbatim from [`OverlayView::warm_relay`]. `None` from a daemon that
    /// predates the feature or has `overlay_warm_relay` off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warm_relay: Option<WarmRelayStatus>,
    /// FR-33 — LAN prefixes this host owns an address in whose traffic the OS
    /// routes through ANOTHER interface (a corp-VPN split-prefix capture: LAN
    /// handshakes arrive, our replies leave through the VPN and die, and the
    /// LAN tier can never latch while it lasts). `None` from a daemon that
    /// predates the field or has the probe off; `Some(empty)` = probed,
    /// nothing captured — the two must stay distinguishable, or an old daemon
    /// would read as "clear".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lan_captures: Option<Vec<LanCaptureStatus>>,
    /// FR-33 — whether the capture probe is switched ON on this daemon
    /// (`overlay_lan_capture_probe`, built-in default on). `Some(false)` =
    /// the operator turned it off, so there is NO capture verdict: `why`
    /// cannot say `lan-captured` and the RC pill cannot name a VPN. Field
    /// 2026-09-04: with the probe off the daemon sent `lan_captures:
    /// Some(empty)` and `status` printed `clear` — the same word as a
    /// genuinely clear host, which is exactly the misreading FR-33 exists to
    /// prevent. Now the daemon sends `lan_captures: None` + this flag, so an
    /// old CLI prints nothing (the documented contract) and a new one prints
    /// `probe OFF`. `None` from a daemon that predates the field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lan_capture_probe: Option<bool>,
    /// FR-47 — the last overlay join the server REFUSED, if any. `None` means
    /// no refusal has been seen this daemon lifetime (or the daemon predates
    /// the field). See [`JoinRefusalStatus`] for why it is not cleared once a
    /// later join succeeds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub join_refusal: Option<JoinRefusalStatus>,
    /// B4 (overlay v3) — the measured netcheck capability vector + its age
    /// (`roomler netcheck`). `None` from a pre-B4 daemon or before the
    /// first measurement completes (~45 s after start).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub netcheck: Option<NetcheckStatus>,
    /// Multi-org P1 — one row per enrollment (the primary first, then each
    /// `[[orgs]]` entry). Empty from a pre-multi-org daemon and omitted by a
    /// single-org one; the top-level scalar fields (`node_id` / `tenant_id`
    /// / `connected`) keep aliasing the PRIMARY enrollment so older tray /
    /// CLI builds render unchanged.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub orgs: Vec<OrgStatus>,
    /// Multi-org v2 retirement evidence — cumulative compensation-layer
    /// PR-B1 — per-bound-direct-socket receive liveness, copied verbatim from
    /// [`OverlayView::direct_socks`]. A socket whose `rx_pkts` is frozen while
    /// peers punch its advertised endpoint is the 2026-08-10 wedge signature
    /// (bound, reader-less, Recv-Q pegged). Empty from older daemons.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub direct_socks: Vec<DirectSockStatus>,
    /// #32 — `(unrouted, backpressure)` inbound `/derp` frames the mux could not
    /// hand to a consumer, cumulative for this process. `None` from older daemons.
    ///
    /// `unrouted > 0` means a peer is relaying to us while we hold it on a
    /// different carrier — the demote-follow's input, and the first thing to read
    /// when a pair is dark after a network transition. `backpressure > 0` means a
    /// LIVE consumer stopped draining.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub derp_inbound_drops: Option<(u64, u64)>,
    /// PR-B1 tripwire — direct-socket binds that walked off the stable base
    /// port this run (`tunnel_core::evidence::DIRECT_BIND_WALKS`). Nonzero on a
    /// host with a configured stable port = external squatter OR an
    /// in-process bind collision (bug signal). `None` from older daemons.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direct_bind_walks: Option<u64>,
    /// A3 — peer endpoints adopted via WG-style roaming since daemon start
    /// (`tunnel_core::evidence::ROAM_ADOPTIONS`). A few is normal (symmetric-NAT
    /// mapping learned); a steadily climbing count is endpoint thrash. `None`
    /// from older daemons.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub roam_adoptions: Option<u64>,
    /// FR-68 — route-guard evidence, all cumulative since daemon start:
    /// `(evictions, sibling_spares, waves, forced_revalidations)`.
    ///
    /// Grouped because no single one of them means anything alone. The pair
    /// that carries the #1237 signal is evictions vs sibling spares: on a
    /// healthy multi-org host spares climb while evictions stay flat, and with
    /// `OVERLAY_SIBLING_EXEMPT=0` that inverts. ⚠️ There is deliberately no
    /// "sibling evictions" number — after #1246 such a row is spared, so the
    /// count would read zero whether the fix works or was reverted.
    ///
    /// `None` from daemons predating FR-68; on a non-Windows host the route
    /// guard is a no-op, so evictions and spares stay 0 honestly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route_guard: Option<(u64, u64, u64, u64)>,
    /// #1282 — which ARM armed each route-defense wave: `(tick, event)`,
    /// cumulative.
    ///
    /// A SEPARATE field rather than two more slots on `route_guard`,
    /// deliberately: that tuple's shape already shipped in 0.4.58, and
    /// widening it would make a 0.4.58 CLI fail to parse a newer daemon's
    /// status — a mixed-version pair is the normal state during a fleet roll.
    /// Additive + `serde(default)` keeps both directions working.
    ///
    /// `route_guard.2 - (tick + event)` should be 0; anything else is a third
    /// caller of `run_defense_wave` that nobody attributed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route_wave_arms: Option<(u64, u64)>,
    /// #1328 — times the route guard STOOD DOWN from a prefix it kept losing
    /// (`tunnel_core::evidence::ROUTE_YIELDS`), cumulative.
    ///
    /// Its own scalar for the same compatibility reason as `route_wave_arms`:
    /// both tuples above already shipped, and widening a released tuple breaks
    /// an older CLI's parse against a newer daemon — the normal state mid-roll.
    ///
    /// ⚠️ Read it WITH `route_guard.0`. Yields climbing while evictions go flat
    /// is the stand-down working; BOTH climbing means the backoff is being
    /// out-paced and the cooldown is too short for that competitor. Zero on a
    /// healthy host, and zero on every non-Windows host (the guard is a no-op).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route_yields: Option<u64>,
    /// C1 (disco) — out-of-tunnel carrier echoes this node ANSWERED
    /// (`tunnel_core::evidence::DISCO_ANSWERED`). Nonzero on every node is the C1
    /// field gate: the fleet can answer, so a prober may ship next. `None`
    /// from a daemon predating the responder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disco_answered: Option<u64>,
    /// FR-19 — this node's org-relay probe responder, or `None` when it is not
    /// running (the default: `relay_server_enabled` is opt-in).
    ///
    /// ⚠️ `None` and a running-but-idle responder are **different states** and
    /// must stay distinguishable. "No relay here" and "a relay that has
    /// answered nothing" lead to opposite next actions, and collapsing them is
    /// how FR-18's `dropped_stale` became unevaluable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub org_relay: Option<OrgRelayStatus>,
    // RETIRED-NAME-ANCHOR: names the retired prefixes this field exists to DETECT.
    // It is live compatibility, not history: delete the legacy arm in `node_env` and
    // this field has nothing left to report, so it goes with it. docs/fr/FR-46
    /// FR-46 (#1051) — full env-var names this daemon has actually READ through
    /// a RETIRED prefix since it started (`ROOMLER_NODE_*`), sorted and deduped.
    /// `None` from a daemon predating the field. Since FR-46 P2b `ROOMLER_AGENT_*`
    /// can never appear here — it is not read at all; see `retired_env_present`.
    ///
    /// This exists because the warning was write-only. `env::note_legacy_use`
    /// logs once per variable near startup, so a `roomler logs` tail on a
    /// long-running daemon cannot find it, and "does any host still depend on a
    /// retired name?" had to be answered by sweeping env vars and registries by
    /// hand — which under-reported twice: once to a `tail -1`, once because
    /// Windows drops EMPTY variables from a process env block while leaving
    /// them in the registry a future start will read.
    ///
    /// ⚠️ **`Some([])` means "nothing retired has been read YET", NOT "this
    /// host sets none."** Knobs are read lazily and some sit on a code path
    /// that has not run. The positive is authoritative; the negative is weak
    /// evidence — the same asymmetry as `ssh_activity`, and it must not be
    /// collapsed into "clean".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub legacy_env_uses: Option<Vec<String>>,
    /// FR-46 P2b — retired-prefix variables that are SET on this host and are
    /// NOT read by anything. `None` from a daemon predating the field.
    ///
    /// ⚠️ Distinct from [`Self::legacy_env_uses`], and folding the two together
    /// would lose the distinction that matters: that one is "a non-current
    /// prefix was READ" (rename it, something depends on it), this one is "a
    /// retired prefix EXISTS and was ignored" (the host was configured for a
    /// spelling the daemon no longer honours). Opposite actions.
    ///
    /// This exists because the alternative to reporting is silence: the read
    /// chain simply stops seeing the variable, the daemon starts fine, and the
    /// host quietly runs without a setting its operator believes is applied.
    /// Two fleet sweeps found exactly that shape — systemd drop-ins no package
    /// upgrade rewrites, and machine-wide registry values in no runbook.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retired_env_present: Option<Vec<String>>,
}

/// FR-19 — the org-relay probe responder's live state (see
/// [`NodeStatus::org_relay`]).
///
/// This exists because the log was not enough in the field: the responder
/// summarises its counters every 300 s **and sleeps before the first report**,
/// so a freshly restarted relay is unreadable for five minutes — exactly the
/// window in which someone is asking "is it working?". Reading it out of
/// `roomler status` answers that immediately.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct OrgRelayStatus {
    /// The bound `ip:port`.
    ///
    /// ⚠️ Present means **bound**, which is NOT the same as reachable. A
    /// coturn-style DNAT can consume the port in `PREROUTING` while the socket
    /// sees nothing and `ss -ulnp` shows it free — measured on mars, where that
    /// confound nearly inverted FR-19's port decision. `answered > 0` is the
    /// evidence of reachability; this field is only evidence of a listener.
    pub listening: String,
    /// Probes answered since start.
    pub answered: u64,
    /// Datagrams refused for not being org-relay shaped at all.
    pub refused_not_shaped: u64,
    /// Org-relay shaped, but not a probe (wrong length, or a data frame).
    pub refused_not_probe: u64,
    /// Refused because the source had spent its per-window allowance.
    ///
    /// One counter per refusal REASON rather than a single "refused", because
    /// during a flood the reason is the whole diagnostic: an attack and a
    /// misconfigured peer look identical in a total.
    pub refused_rate_limited: u64,
}

/// PR-B1 — one bound direct socket's receive liveness (plane or per-device
/// demux). See [`NodeStatus::direct_socks`].
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct DirectSockStatus {
    /// The socket's bound `ip:port` (the wildcard public dialer is labeled).
    pub local: String,
    /// Datagrams the owning recv loop actually READ (cumulative since bind).
    pub rx_pkts: u64,
    /// Seconds since the last read datagram; `None` = never read one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_rx_age_s: Option<u64>,
}

/// Multi-org P1 — one enrollment's live state (see [`NodeStatus::orgs`]).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct OrgStatus {
    /// `primary` for the scalar identity, else the `[[orgs]]` label.
    pub label: String,
    pub server_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant_id: Option<String>,
    /// This org's server-assigned agent id (hex).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    pub primary: bool,
    /// Config-level soft-enable; a disabled org has no supervised WS loop.
    pub enabled: bool,
    /// This org's signaling WS is currently established.
    pub connected: bool,
    /// The org's supervisor stopped permanently this run (server goodbye /
    /// duplicate-instance escalation). Cleared by a daemon restart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_error: Option<String>,
    /// `rc:agent.update` pushes ignored on this org's WS because only the
    /// primary enrollment may drive the machine-wide self-updater.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub updates_ignored: u32,
    /// FR-49 — this enrollment's overlay participation: `off` | `netstack` |
    /// `tun` (`OrgOverlayMode::wire`).
    ///
    /// ⚠️ **Empty means "this daemon does not report it", NOT "off".** A
    /// secondary org defaults to `off` and its WS still connects, so an org
    /// with no mesh was indistinguishable from a healthy one on every surface
    /// an operator has — which is the whole reason this field exists. Rendering
    /// an absent value as `off` would put that same lie back, one layer up, and
    /// it is the identical trap to an absent age reading as 0 ms.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub overlay_mode: String,
}

fn is_zero_u32(v: &u32) -> bool {
    *v == 0
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

/// NAT-traversal: the outcome of this node's server-reflexive (STUN) gather.
///
/// Exists because the srflx tier failed **fleet-wide and silently** for an
/// unknown period (2026-08-06: coturn emitted its UDP replies with `TTL=1`, so
/// every forwarded reply was dropped before the FORWARD chain). Both failure
/// paths logged at `debug!`, so nothing above DEBUG ever said a word — while
/// every pair in the mesh silently degraded to the DERP carrier, its slowest
/// tier. An empty `candidates` here is the single most useful number for
/// "why is everything on relay?".
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct SrflxStatus {
    /// Public `ip:port` candidates STUN reported for our direct sockets.
    /// **Empty = the whole srflx tier is dead on this node**: no hole-punch,
    /// and every peer reads us as UDP-blocked.
    #[serde(default)]
    pub candidates: Vec<String>,
    /// The STUN server actually queried, once resolved. `None` = none of the
    /// netmap's `stun_urls` resolved to a usable v4 endpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stun_server: Option<String>,
    /// Our probed NAT class — `cone` (hole-punchable) / `symmetric` /
    /// `None` = unclassified (the punch is still attempted).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nat: Option<String>,
    /// R2 — the mapping was gathered via the wildcard public-dial socket
    /// (full-tunnel VPN egress rescue: every LAN-bound vantage was dead and
    /// the captured default route answered). Punches ride the tunnel path.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub via_public_dial: bool,
    /// Why the gather produced nothing, when it produced nothing. `None` on
    /// success. Rendered verbatim by `roomler status`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl SrflxStatus {
    /// Did the srflx tier come up at all?
    pub fn is_healthy(&self) -> bool {
        !self.candidates.is_empty()
    }
}

/// B4 (overlay v3) — the measured netcheck capability vector as surfaced by
/// `roomler netcheck`: what selection actually keys on, plus the
/// measurement's age (fresh under 60 min; stale vectors are treated as
/// absent by every consumer).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct NetcheckStatus {
    /// The srflx gather found a public mapping.
    pub stun_udp: bool,
    /// Raw UDP reaches coturn's relay band, measured over the exact
    /// single-relay dialer path. `None` = could not be measured (no creds /
    /// no srflx to permit) — absence of measurement is never evidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay_band_udp: Option<bool>,
    /// The central `/derp` WS is up + registered (the floor's health).
    pub derp_ws_ok: bool,
    /// Probed NAT class (`cone` / `symmetric`), when typed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nat: Option<String>,
    /// Seconds since the vector was measured.
    pub age_s: u64,
}

/// C4 stage 1 — the standing warm TURN/UDP allocation (measurement-only:
/// nothing routes over it yet). Exists so a VPN transition's effect on the
/// grandfathered relay flow is READABLE — "the allocation survived the VPN
/// connect" must be a status line, not log archaeology.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct WarmRelayStatus {
    /// `live` | `none` | `lost`. `none` = never established this run (or
    /// re-establish pending); `lost` = it existed and a probe/allocation
    /// failure ended it (see `detail`).
    pub state: String,
    /// The allocation's relayed transport address (`worker-ip:port`) —
    /// the future rendezvous `R`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relayed: Option<String>,
    /// C4 stage 2 — which transport the warm leg rides: `udp` (the
    /// grandfather-measurable flavor) or `tls` (TURNS/TCP:443 — the
    /// strict-corp fallback that survives a VPN capture). `None` from a
    /// pre-stage-2 daemon or while nothing is live.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flavor: Option<String>,
    /// Seconds since the allocation was established.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub age_s: Option<u64>,
    /// Seconds until the ephemeral credentials expire (negative = past
    /// due; stage 1 re-establishes fresh rather than re-allocating on the
    /// same socket, so expiry on a UDP-blocked network means `lost`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cred_expiry_in_s: Option<i64>,
    /// Seconds since the last successful liveness probe (a 1-byte
    /// permission assert through the allocation).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_probe_ok_s: Option<u64>,
    /// Why `none`/`lost`, when known. Rendered verbatim by `roomler status`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// A peer device as this node currently sees it.
// NOTE: no `Eq` — `PeerWhy` carries the selector's f64 scores, and rounding
// them to keep `Eq` would make the diagnostic lie about the numbers the
// selector actually used. Nothing compares `PeerInfo` for total equality.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct PeerInfo {
    pub node_id: String,
    pub name: String,
    /// Multi-org — the label of the org this peer belongs to (the `[[orgs]]`
    /// label, or `primary`). A device in N orgs runs N overlay engines with
    /// DISJOINT peer sets and disjoint address spaces, so a flat peer list is
    /// ambiguous: two orgs can each hold a peer named the same, and the same
    /// physical host appears once per shared org under different overlay IPs.
    ///
    /// Empty for a single-org device and for a daemon older than the field
    /// (`#[serde(default)]` — an older CLI renders its flat table unchanged).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub org: String,
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
    /// Which relay this peer's carrier rides, when relayed: `turn` | `derp`.
    ///
    /// A bare `relay` in the CONN column could not distinguish a 52 ms coturn
    /// hop from a 175 ms DERP one, nor a healthy PoP from a DEAD one — on
    /// 2026-08-12 a coturn worker was down for 90 minutes while agents
    /// crash-looped and this column said only "relay". Empty for direct peers
    /// and for daemons older than the field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay_kind: Option<String>,
    /// How that relay is reached: `udp` | `tcp`. Load-bearing for latency
    /// expectations, and for corporate paths where only 443/TCP survives.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay_transport: Option<String>,
    /// The relay server / PoP, when the carrier knows it (`host:port`). This
    /// is what you would check `kubectl -n coturn get pods` against.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay_server: Option<String>,
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
    /// F — the path decision for this peer, in readable form
    /// (`roomler why <peer>`). Always populated by a current daemon; `None`
    /// from an older one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub why: Option<PeerWhy>,
    /// A — what the disco prober measures for this peer's path(s). Empty when
    /// the prober is off, or before it has issued a round.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub probes: Vec<PathProbe>,
}

/// A — the disco prober's measurement of one path to this peer (C2).
///
/// Measurement ONLY: nothing in the selector reads this. It is here so an
/// operator (and `roomler why`) can see what the path actually does next to
/// the decision the selector made about it — the two disagreeing is the
/// interesting case, and until now the measurement was visible only in a
/// 5-minute digest log line.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct PathProbe {
    /// The remote endpoint probed (`host:port`).
    pub dst: String,
    /// Windowed loss 0.0..=1.0. `None` until enough rounds to judge — an
    /// UNMEASURED path must never read as a bad one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loss: Option<f64>,
    /// Smoothed round-trip.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rtt_ms: Option<f64>,
    /// 95th percentile of the raw window — the EWMA above smooths spikes away
    /// by design, and spikes are what make a path feel bad.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rtt_p95_ms: Option<f64>,
    /// Worst raw round-trip in the window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rtt_max_ms: Option<f64>,
}

/// F — why this peer sits on the tier it sits on (`roomler why <peer>`).
///
/// Populated on every view publish, like [`PeerCarrierDebug`], rather than
/// computed on demand: an incident capture is usually a `peers --json` taken
/// at the time, and the explanation is worth far more inside that snapshot
/// than in a command someone had to know to run while the fault was live.
///
/// `None` from a daemon older than the field (`#[serde(default)]`, wire
/// compatible both ways).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct PeerWhy {
    /// LAN, public, srflx, relay — already in ladder order.
    pub tiers: Vec<TierWhy>,
    /// Seconds left on the demote-follow hold-down, if open. While this is
    /// set, EVERY direct tier is ineligible regardless of its own health —
    /// the peer is relaying to us, so we follow it rather than fight it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relayed_instead_s: Option<u64>,
    /// Consecutive demote-follows inside the memory window — the rung of the
    /// escalation ladder. A climbing value is the signature of a pair whose
    /// two ends persistently disagree about the path, which is a different
    /// problem from a path that is simply bad.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub relayed_instead_strikes: u32,
    /// Seconds left on the server's forced-DERP pin, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forced_derp_s: Option<u64>,
    /// A direct-upgrade probe is in flight on this tier right now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probing: Option<String>,
}

/// One tier's row in [`PeerWhy`].
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct TierWhy {
    /// `lan` | `public` | `srflx` | `relay`.
    pub tier: String,
    /// May this tier be attempted at all right now?
    pub eligible: bool,
    /// When not eligible, which gate refused: `peer-relays-instead` |
    /// `lan-captured` (FR-33 — this host's route to the peer's LAN prefix
    /// leaves via another adapter, a VPN split-prefix capture) | `penalty`.
    /// Resolved in the same order the selector tests them, so it cannot
    /// contradict `eligible`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_by: Option<String>,
    /// The tier's fixed prior.
    pub base: f64,
    /// Measured-quality term (ranking only — never eligibility).
    pub q: f64,
    /// Decaying penalty from recent failures.
    pub penalty: f64,
    /// `base + q − penalty`; meaningful only among ELIGIBLE tiers.
    pub score: f64,
    /// Consecutive failures booked against this tier.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub fails: u32,
}

/// rc.276 — per-carrier forensic fields (see [`PeerInfo::debug`]). Built for
/// the winhost-a-class corp-VPN investigations: two `peers --json` snapshots
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
    /// The `Rpf(NoRoute)` subset of [`Self::rx_denied`]: packets from a source
    /// NO installed peer owns. Worse than a plain denial even under `warn` —
    /// the packet is delivered but replies to that source are unroutable from
    /// this node, so the flow fails silently. The signature of a multi-org
    /// sender whose OS picked the wrong org's overlay address as source.
    /// Omitted when 0.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub rx_denied_noroute: u64,
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
    /// Multi-org P1 — which enrollment this route rides. `None` (the wire
    /// default every pre-multi-org client sends) and `"primary"` both mean
    /// the config's scalar primary identity. A secondary org's label is
    /// accepted in the schema today but the P1 reconciler surfaces it as
    /// `Failed` (secondary-org route supervision lands with P2/P3 of the
    /// multi-org program).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub org: Option<String>,
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

/// One request awaiting an operator consent decision (rc.46).
/// Surfaced by [`Request::ConsentPending`] so the desktop app renders its
/// Approve/Deny modal over the LocalAPI instead of reading the daemon's private
/// sentinel dir — which lives in the daemon's profile and is unreachable to the
/// interactive-user app when the daemon runs as SYSTEM (P2b bug fix).
///
/// FR-27 — no longer remote-control only. Fleet-RPC `exec` and Roomler SSH have
/// always prompted through the same broker, but never wrote a marker, so their
/// prompts were invisible to every UI and answerable only by someone who greped
/// the daemon log for a request id inside 30 s. [`Self::kind`] is what lets one
/// modal serve all three without lying about which is which — "approve this"
/// means something very different for a screen share and for a root shell.
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
    /// FR-27 — which subsystem is asking: `rc` (screen control), `exec` (a
    /// command) or `ssh` (a shell). `#[serde(default)]` yields `""` from a
    /// pre-FR-27 daemon, which a reader should treat as `rc` — the only kind
    /// that existed then.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub kind: String,
    /// FR-27 — the one line that makes the decision answerable: the redacted
    /// command for `exec`, the principal + account mode for `ssh`, empty for
    /// `rc` (where `controller_name` and `permissions` already say everything).
    ///
    /// ⚠️ Redacted BY THE DAEMON before it is written. A consent prompt is
    /// rendered by a GUI process and can be screenshotted, and `exec` payloads
    /// routinely carry tokens — `exec::redactor()` runs first, as it does for
    /// exec output leaving the host.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
    /// FR-27 — unix-millis deadline, so a panel can show a real countdown
    /// instead of restarting one every time it re-reads the list. `0` from a
    /// pre-FR-27 daemon means "unknown"; fall back to `timeout_secs`.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub expires_at_ms: u64,
    /// FR-27 — who is ALREADY showing this prompt: `native` (the daemon's own
    /// overlay is up) or `companion` / absent (nobody is; the desktop app
    /// should).
    ///
    /// ⚠️ Load-bearing for the desktop app, not informational. The daemon
    /// writes a marker even when its native panel is up, because this list is
    /// also what `roomlerd consent --list` reads — so without this field the
    /// companion would pop a SECOND panel asking the same question, and two
    /// Approve buttons for one decision is how someone approves the wrong
    /// thing. A UI must LIST a `native` entry and not render a prompt for it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub surface: String,
    // NOTE: `org` stays LAST; the struct is built field-by-field at three call
    // sites and keeping the multi-org line at the end matches how it reads.
    /// Multi-org — the organization the request comes from, so the modal can
    /// say WHO is asking. On a device enrolled in two orgs, "Alice wants to
    /// control this machine" is not enough to decide on: Alice may be a
    /// colleague in one org and an outside contractor in the other.
    ///
    /// Empty for a single-org device, and for a daemon older than the field
    /// (`#[serde(default)]` — the desktop app simply shows no org line).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub org: String,
}

/// FR-27 — one remote-control session currently LIVE on this device.
///
/// The device side of "who is watching my screen". Distinct from
/// [`ConsentRequest`], which is about sessions not yet allowed to start: this
/// is what the "Being viewed by …" banner renders, and what its Disconnect
/// button acts on.
///
/// There was no LocalAPI verb for this at all before, which is why the banner
/// existed only as the Windows-native overlay inside the daemon — no thin
/// client could see a session, let alone end one.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct RcSessionInfo {
    /// Hex `ObjectId` — the handle [`Request::RcDisconnect`] takes.
    pub session_id: String,
    /// Display name of whoever is controlling.
    #[serde(default)]
    pub controller_name: String,
    /// Pipe-separated permission names — what they were actually granted, so
    /// a view-only watcher is distinguishable from someone typing.
    #[serde(default)]
    pub permissions: String,
    /// Asking organization on a multi-org device; empty otherwise.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub org: String,
    /// Unix millis when the session started, for an "N min" age in the banner.
    /// `0` = unknown.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub started_at_ms: u64,
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
    /// FR-27 — remote-control sessions currently LIVE on this device.
    RcSessions,
    /// FR-27 — end a live remote-control session from the device side. This is
    /// the Disconnect the "Being viewed by …" banner offers; mutating, and the
    /// pipe/socket ACL is the trust boundary, like [`Self::ConsentDecide`].
    RcDisconnect { session_id: String },
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
    /// Fleet RPC — run a command on ANOTHER device in this org and return its
    /// output (`roomler exec <device> -- <cmd>`).
    ///
    /// The daemon relays this over its own already-authenticated agent WS, so
    /// the CLI needs no user credentials of its own. The server resolves this
    /// device's owner as the acting principal and applies all four gates —
    /// the pipe/socket ACL only establishes that the caller is on THIS box,
    /// which is not by itself authority to run commands on another one.
    ///
    /// Mutating in the strongest sense available, so unlike the other verbs
    /// this one's trust boundary is deliberately NOT just the pipe ACL: the
    /// device must also carry `ExecPolicy::can_originate` server-side.
    /// Returns [`Response::ExecResult`].
    ExecRemote {
        /// Target device: a name (e.g. `winhost-a`) or a hex agent id.
        node: String,
        /// `pwsh` | `powershell` | `cmd` | `bash` | `sh`; empty ⇒ host default.
        #[serde(default)]
        shell: String,
        command: String,
        /// 0 ⇒ the server's default; clamped server- and agent-side.
        #[serde(default)]
        timeout_ms: u64,
    },
    /// Roomler SSH (P6b) — ask the server for a single-use session grant on
    /// ANOTHER device, so `roomler ssh <device>` can dial it.
    ///
    /// The daemon relays this over its already-authenticated agent WS; the CLI
    /// carries no user credentials of its own, exactly as with
    /// [`Request::ExecRemote`], and the same four gates apply server-side.
    ///
    /// **The caller supplies `public_key` and keeps the private half.** The
    /// daemon never sees it and the grant is bound to that key, so a grant
    /// intercepted anywhere on this path is useless without the private half
    /// that never left the requesting process.
    ///
    /// Returns [`Response::SshSession`].
    SshSession {
        /// Target device: a name (e.g. `winhost-a`) or a hex agent id.
        node: String,
        /// OpenSSH public key of the caller's EPHEMERAL, single-session
        /// keypair (`ssh-ed25519 AAAA… comment`).
        public_key: String,
        /// 0 ⇒ the server's ceiling; clamped server-side.
        #[serde(default)]
        session_secs: u64,
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
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "t", content = "d", rename_all = "snake_case")]
pub enum Response {
    /// Boxed: `NodeStatus` is ~408 bytes against a 137-byte second-largest,
    /// so EVERY `Response` (including the frequent `Peers` / `Pong`) paid for a
    /// variant only `status` uses. Serde is transparent through `Box`, so the
    /// wire format is unchanged.
    Status(Box<NodeStatus>),
    Peers(Vec<PeerInfo>),
    Flows(Vec<FlowInfo>),
    /// Sessions awaiting a consent decision.
    ConsentPending(Vec<ConsentRequest>),
    /// Result of a [`Request::ConsentDecide`] — `ok` = the decision was recorded.
    ConsentDecided {
        ok: bool,
    },
    /// FR-27 — live remote-control sessions.
    RcSessions(Vec<RcSessionInfo>),
    /// FR-27 — result of [`Request::RcDisconnect`]; `ok` = a session with that
    /// id was found and asked to stop.
    RcDisconnected {
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
    /// Result of [`Request::ExecRemote`]. `exit_code: None` + `error: Some`
    /// covers every way the command didn't run (any of the four gates, an
    /// offline device, a timeout) — a caller must be able to tell that from
    /// "ran and exited 0".
    ExecResult {
        /// Server-minted; addresses a later cancel.
        request_id: String,
        node: String,
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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// Answer to [`Request::SshSession`]. Either every dial field is present
    /// or `error` is — a caller must never be left with half a connection.
    SshSession {
        request_id: String,
        node: String,
        /// The target's overlay address.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        address: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        port: Option<u16>,
        /// The target's SSH host public key (P6a). **Absent means the device
        /// cannot prove itself** — never "any key is fine". A client that
        /// cannot verify should refuse rather than fall back to TOFU.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        host_pubkey: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        grant_id: Option<String>,
        /// Unix ms the grant stops being redeemable — dial before this.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expires_at_ms: Option<u64>,
        /// Set when the request was refused, naming which gate said no.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
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
    /// NAT-traversal — the srflx gather outcome, filled by the overlay runtime
    /// on every gather (success or failure). The daemon copies it verbatim
    /// into [`NodeStatus::srflx`]. `None` before the first gather.
    pub srflx: Option<SrflxStatus>,
    /// C4 stage 1 — the warm TURN/UDP allocation's state, filled by the
    /// overlay runtime on every view publish when `overlay_warm_relay` is
    /// on. The daemon copies it verbatim into [`NodeStatus::warm_relay`].
    pub warm_relay: Option<WarmRelayStatus>,
    /// PR-B1 — per-bound-direct-socket receive liveness, filled by the overlay
    /// runtime on every view publish (plane stats when the shared carrier
    /// plane is on, per-device demux stats otherwise). The daemon copies it
    /// verbatim into [`NodeStatus::direct_socks`].
    pub direct_socks: Vec<DirectSockStatus>,
    /// #32 — see [`NodeStatus::derp_inbound_drops`]. Filled by the overlay
    /// runtime on every view publish; the daemon copies it verbatim.
    pub derp_inbound_drops: Option<(u64, u64)>,
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
    /// FR-27 — remote-control sessions currently LIVE on this device. Distinct
    /// from [`Self::consent_pending`], which is about sessions not yet allowed
    /// to start. Default: none.
    fn rc_sessions(&self) -> Vec<RcSessionInfo> {
        Vec::new()
    }
    /// FR-27 — tear down a live remote-control session from the device side.
    /// Returns whether a session with that id was found and asked to stop.
    /// Default: no-op `false`.
    fn rc_disconnect(&self, _session_id: &str) -> bool {
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
    /// Fleet RPC — relay an exec request to another device over this daemon's
    /// agent WS and await the answer (async — a full server round-trip).
    /// Default: unsupported — a node with no agent WS can't originate one.
    async fn exec_remote(
        &self,
        _node: &str,
        _shell: &str,
        _command: &str,
        _timeout_ms: u64,
    ) -> Response {
        Response::Error {
            message: "this node cannot originate remote commands (no server connection)".into(),
        }
    }

    /// Ask the server for a single-use SSH grant on another device (P6b).
    /// Default: this node has no server connection, so it cannot originate.
    async fn ssh_session(&self, _node: &str, _public_key: &str, _session_secs: u64) -> Response {
        Response::Error {
            message: "this node cannot originate SSH sessions (no server connection)".into(),
        }
    }
}

/// Pure dispatch: map a [`Request`] to a [`Response`] over a state snapshot.
/// No I/O — the pipe listener (P1-cont) reads a JSON line, deserialises a
/// [`Request`], calls this, and writes the [`Response`] back.
pub fn handle(req: &Request, state: &dyn LocalApiState) -> Response {
    match req {
        Request::Status => Response::Status(Box::new(state.status())),
        Request::Peers => Response::Peers(state.peers()),
        Request::Flows => Response::Flows(state.flows()),
        Request::ConsentPending => Response::ConsentPending(state.consent_pending()),
        Request::ConsentDecide { session_id, allow } => Response::ConsentDecided {
            ok: state.consent_decide(session_id, *allow),
        },
        Request::RcSessions => Response::RcSessions(state.rc_sessions()),
        Request::RcDisconnect { session_id } => Response::RcDisconnected {
            ok: state.rc_disconnect(session_id),
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
        | Request::TailLog { .. }
        | Request::ExecRemote { .. }
        | Request::SshSession { .. } => Response::Error {
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
            Ok(Request::ExecRemote {
                node,
                shell,
                command,
                timeout_ms,
            }) => state.exec_remote(&node, &shell, &command, timeout_ms).await,
            Ok(Request::SshSession {
                node,
                public_key,
                session_secs,
            }) => state.ssh_session(&node, &public_key, session_secs).await,
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
// The daemon (`roomlerd` / today's `roomlerd`) calls [`serve`] once at
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

/// Well-known home for a SYSTEM (root) daemon's control socket.
///
/// A per-user path cannot serve a root daemon. On macOS `temp_dir()` is
/// already per-user (`/var/folders/…`), so a root daemon's socket would land
/// in ROOT's folder and a user-session `roomler peers` would look in its own
/// and find *nothing* — not a permission error, an apparent absence. That is
/// the trap the macOS LaunchDaemon split walks into, and this is the fix:
/// when the process can own `/var/run/roomler`, it is the daemon and the
/// socket goes somewhere every session can name.
#[cfg(unix)]
const LOCALAPI_SYSTEM_DIR: &str = "/var/run/roomler";

/// The PER-USER socket path — `$XDG_RUNTIME_DIR/roomler.sock` when the
/// runtime dir is set (systemd guarantees it's 0700 + user-owned — the right
/// home for a control socket), else a `roomler/` subdir under the temp dir
/// (locked to 0700 by the listener). The socket itself is chmod 0600.
#[cfg(unix)]
pub(crate) fn user_socket_path() -> std::path::PathBuf {
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        return std::path::PathBuf::from(dir).join(LOCALAPI_SOCKET_NAME);
    }
    // No runtime dir (common on macOS, where temp_dir() is already per-user).
    std::env::temp_dir()
        .join("roomler")
        .join(LOCALAPI_SOCKET_NAME)
}

/// The SYSTEM socket path, used by a daemon privileged enough to bind it.
#[cfg(unix)]
pub(crate) fn system_socket_path() -> std::path::PathBuf {
    std::path::PathBuf::from(LOCALAPI_SYSTEM_DIR).join(LOCALAPI_SOCKET_NAME)
}

/// Where a CLIENT looks, in order: a per-user daemon first (unchanged fast
/// path for every existing install), then the system daemon.
#[cfg(unix)]
pub(crate) fn unix_socket_candidates() -> [std::path::PathBuf; 2] {
    [user_socket_path(), system_socket_path()]
}

/// Bind the daemon's control socket, preferring the SYSTEM path.
///
/// Which path is right depends on whether this process is privileged, and
/// rather than ask (`geteuid` would mean a `libc` dependency for one call —
/// and a uid is only a PROXY for the thing we actually need) this TESTS the
/// capability: if `/var/run/roomler` can be created and bound, we are the
/// system daemon and belong there. An unprivileged daemon fails that and
/// keeps exactly its current per-user path.
///
/// A system daemon ALSO binds the per-user path, best-effort. That is
/// forward-compat for one release, the same shape as the P2a rollout: a
/// `roomler` CLI that predates this change only knows the per-user path, and
/// packaging does not guarantee the CLI and the daemon step forward in the
/// same instant. Drop the second bind once the fleet is past it.
#[cfg(unix)]
async fn serve_unix(
    state: Arc<dyn LocalApiState>,
    shutdown: watch::Receiver<bool>,
) -> std::io::Result<()> {
    let system = system_socket_path();
    match prepare_unix_listener(&system) {
        Ok(listener) => {
            tracing::info!(
                path = %system.display(),
                "localapi: system-daemon socket (privileged); a user session reaches it here"
            );
            let legacy = user_socket_path();
            if let Ok(l) = prepare_unix_listener(&legacy) {
                let st = state.clone();
                let sd = shutdown.clone();
                tracing::info!(
                    path = %legacy.display(),
                    "localapi: also serving the legacy per-user path (pre-split CLIs)"
                );
                tokio::spawn(async move { accept_unix(l, legacy, st, sd).await });
            }
            accept_unix(listener, system, state, shutdown).await
        }
        Err(e) => {
            tracing::debug!(
                path = %system.display(), error = %e,
                "localapi: not privileged for the system socket; using the per-user path"
            );
            serve_unix_at(user_socket_path(), state, shutdown).await
        }
    }
}

/// The unix-socket accept loop, parameterised on the path so tests can use a
/// private one.
#[cfg(unix)]
pub(crate) async fn serve_unix_at(
    path: std::path::PathBuf,
    state: Arc<dyn LocalApiState>,
    shutdown: watch::Receiver<bool>,
) -> std::io::Result<()> {
    if *shutdown.borrow() {
        return Ok(());
    }
    let listener = prepare_unix_listener(&path)?;
    accept_unix(listener, path, state, shutdown).await
}

/// Create the parent dir, clear a stale socket, bind, and lock the socket to
/// 0600. Split out from the accept loop so a caller can TEST whether a path is
/// bindable (the privileged-vs-per-user decision) without committing to
/// serving it forever.
#[cfg(unix)]
fn prepare_unix_listener(path: &std::path::Path) -> std::io::Result<tokio::net::UnixListener> {
    use std::os::unix::fs::PermissionsExt;
    use tokio::net::UnixListener;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        // Lock the parent to owner-only when we own it (the temp-subdir case);
        // for $XDG_RUNTIME_DIR this is already true and the chmod is harmless
        // (ignored if we don't own it).
        let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
    }
    // A stale socket from an unclean exit makes bind fail with EADDRINUSE.
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)?;
    // Owner-only: no other local user can open the control socket.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    tracing::info!(path = %path.display(), "localapi: unix-socket listener up (0600)");
    Ok(listener)
}

#[cfg(unix)]
async fn accept_unix(
    listener: tokio::net::UnixListener,
    path: std::path::PathBuf,
    state: Arc<dyn LocalApiState>,
    mut shutdown: watch::Receiver<bool>,
) -> std::io::Result<()> {
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
// (`LOCALAPI_PIPE_NAME` / `unix_socket_candidates`) and the wire types with the
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
        // Try a per-user daemon first (unchanged, and the common case), then
        // the system daemon. Without the second candidate a root daemon is
        // invisible to a user session on macOS, where the per-user path lives
        // under ROOT's `/var/folders/…` — the CLI would report "not running"
        // for a daemon that is running perfectly well.
        let mut last: Option<std::io::Error> = None;
        for path in unix_socket_candidates() {
            match connect_unix_at(path).await {
                Ok(client) => return Ok(client),
                Err(e) => last = Some(e),
            }
        }
        Err(last.unwrap_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "no LocalAPI socket")
        }))
    }
}

/// The OTHER daemon's socket, when this host runs two and [`connect`] would
/// have taken the per-user one.
///
/// macOS installs two halves by necessity — a per-user LaunchAgent for capture
/// and input, a root LaunchDaemon for the overlay — and they listen on
/// different sockets. `connect()` prefers the per-user one, so an unprivileged
/// `roomler status` on such a host answers for the half that has no overlay and
/// prints `overlay ip —`, while `roomler peers` prints nothing. Both are
/// truthful about the process they reached and completely misleading about the
/// machine, which reads as "the overlay is broken".
///
/// Returns `Some(path)` only when BOTH exist, i.e. exactly when a caller could
/// be looking at the wrong one.
#[cfg(unix)]
pub fn other_daemon_socket() -> Option<std::path::PathBuf> {
    let [user, system] = unix_socket_candidates();
    (user.exists() && system.exists()).then_some(system)
}

/// Windows runs a single daemon, so there is never another one to point at.
#[cfg(windows)]
pub fn other_daemon_socket() -> Option<std::path::PathBuf> {
    None
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
            Response::Status(s) => Ok(*s),
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

    /// FR-27 — `Request::RcSessions` → remote-control sessions currently live
    /// on this device.
    pub async fn rc_sessions(&mut self) -> std::io::Result<Vec<RcSessionInfo>> {
        match self.request(&Request::RcSessions).await? {
            Response::RcSessions(v) => Ok(v),
            other => Err(unexpected_response(other)),
        }
    }

    /// FR-27 — `Request::RcDisconnect` → end a live session from the device
    /// side. `false` = no session with that id (already gone, or never here).
    pub async fn rc_disconnect(&mut self, session_id: &str) -> std::io::Result<bool> {
        let req = Request::RcDisconnect {
            session_id: session_id.to_string(),
        };
        match self.request(&req).await? {
            Response::RcDisconnected { ok } => Ok(ok),
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

/// FR-47 — the last time the server REFUSED this node's overlay join, as
/// [`NodeStatus::join_refusal`] reports it.
///
/// The failure this exists for: before the refusal frame, a node that could
/// not be given an address simply waited for a netmap that never arrived, so
/// it was indistinguishable from one that was merely offline — on the host as
/// much as on the dashboard. The daemon log carries the reason now; this puts
/// it where an operator actually looks first.
///
/// ⚠️ Reported even after a LATER join succeeds, with its timestamp, rather
/// than being cleared: "we were refused twenty minutes ago and are fine now"
/// is a different and more useful fact than silence, and it is the only trace
/// left once the log has rotated. `connected` already says whether the node is
/// up right now, so this cannot be mistaken for the current state.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct JoinRefusalStatus {
    /// The enumerated reason, as the wire spelled it
    /// (`address_space_exhausted`, `network_unavailable`, `store_unavailable`,
    /// or `unknown` from a newer server).
    #[serde(default)]
    pub reason: String,
    /// The server's human-readable detail.
    #[serde(default)]
    pub detail: String,
    /// Unix seconds when the refusal arrived.
    #[serde(default)]
    pub at_unix: i64,
    /// Whether another attempt could plausibly succeed. A full block does not
    /// empty on retry; a transient store fault might.
    #[serde(default)]
    pub retryable: bool,
}

/// FR-33 — one captured LAN prefix, as [`NodeStatus::lan_captures`] reports it.
/// Detect-and-report only: the daemon never routes around a capture.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct LanCaptureStatus {
    /// The captured prefix, `a.b.c.d/n`.
    #[serde(default)]
    pub prefix: String,
    /// The interface that owns our address in it (name).
    #[serde(default)]
    pub owner: String,
    /// The interface a packet to a neighbour in the prefix actually leaves by
    /// (ifindex on Windows, device name on Unix).
    #[serde(default)]
    pub via: String,
    /// That interface's name when the daemon could resolve it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via_name: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// WIRE SHAPE. A node that is not serving relay probes must produce
    /// **byte-identical** status JSON to one built before FR-19 existed —
    /// otherwise every older `roomler status` / desktop reader has its shape
    /// changed by a feature it does not have.
    ///
    /// And the `Some` case must carry the counters even when they are zero:
    /// "serving, has answered nothing" is a real and important state, and it
    /// is the one a freshly-restarted relay is in while someone is asking
    /// whether it works.
    #[test]
    fn org_relay_is_absent_when_not_serving_and_explicit_when_it_is() {
        let mut s = Mock.status();
        s.org_relay = None;
        let json = serde_json::to_string(&s).unwrap();
        assert!(
            !json.contains("org_relay"),
            "a non-serving node must not emit the key at all: {json}"
        );

        s.org_relay = Some(OrgRelayStatus {
            listening: "0.0.0.0:3478".into(),
            answered: 0,
            refused_not_shaped: 0,
            refused_not_probe: 0,
            refused_rate_limited: 0,
        });
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains(r#""listening":"0.0.0.0:3478""#), "{json}");
        assert!(
            json.contains(r#""answered":0"#),
            "zero counters must be PRESENT, not skipped -- 'serving and idle' \
             is not the same state as 'not serving': {json}"
        );

        // Round trip, so an older field order or a rename is caught here.
        let back: NodeStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(back.org_relay, s.org_relay);
    }

    /// FR-33 — the probe-off state must be distinguishable on the wire from
    /// both "probed, clear" and "old daemon": `lan_capture_probe: false`
    /// travels, and a daemon that predates the field reads as `None` (so the
    /// CLI keeps its old silence rather than inventing a verdict).
    #[test]
    fn lan_capture_probe_flag_is_additive_on_the_wire() {
        let mut s = Mock.status();
        s.lan_captures = None;
        s.lan_capture_probe = Some(false);
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains(r#""lan_capture_probe":false"#), "{json}");
        assert!(
            !json.contains("lan_captures"),
            "probe off ⇒ no capture list at all, not an empty one: {json}"
        );
        let back: NodeStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(back.lan_capture_probe, Some(false));
        assert_eq!(back.lan_captures, None);

        // An older daemon omits both fields.
        let old: NodeStatus =
            serde_json::from_str(&json.replace(r#","lan_capture_probe":false"#, "")).unwrap();
        assert_eq!(old.lan_capture_probe, None);
        assert_eq!(old.lan_captures, None);
    }

    struct Mock;
    #[async_trait]
    impl LocalApiState for Mock {
        fn status(&self) -> NodeStatus {
            NodeStatus {
                node_id: "n1".into(),
                name: "devbox".into(),
                version: "0.3.0-rc.154".into(),
                mode: DaemonMode::Service,
                tenant_id: Some("t1".into()),
                overlay_ip: Some("100.64.0.2".into()),
                overlay_ip6: None,
                connected: true,
                ephemeral: false,
                exit_node: None,
                config_path: None,
                dns: None,
                srflx: None,
                warm_relay: None,
                lan_captures: None,
                lan_capture_probe: None,
                join_refusal: None,
                orgs: Vec::new(),
                direct_socks: Vec::new(),
                direct_bind_walks: None,
                roam_adoptions: None,
                route_guard: None,
                route_wave_arms: None,
                route_yields: None,
                disco_answered: None,
                org_relay: None,
                legacy_env_uses: Some(Vec::new()),
                retired_env_present: Some(Vec::new()),
                derp_inbound_drops: None,
                netcheck: None,
            }
        }
        fn peers(&self) -> Vec<PeerInfo> {
            vec![
                PeerInfo {
                    node_id: "n2".into(),
                    name: "winhost-a".into(),
                    org: String::new(),
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
                    relay_kind: None,
                    relay_transport: None,
                    relay_server: None,
                    why: None,
                    probes: Vec::new(),
                    debug: None,
                },
                PeerInfo {
                    node_id: "n3".into(),
                    name: "home".into(),
                    org: String::new(),
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
                    relay_kind: None,
                    relay_transport: None,
                    relay_server: None,
                    why: None,
                    probes: Vec::new(),
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
                node: Some("winhost-a".into()),
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
                kind: "rc".into(),
                detail: String::new(),
                expires_at_ms: 0,
                surface: String::new(),
                org: "Acme".into(),
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

    /// F/A — `why` and `probes` are wire-compatible in BOTH directions, which
    /// is the property a mixed fleet actually depends on: an old daemon sends
    /// neither field (a current CLI must read that as "unknown", not as "no
    /// hold-down and a clean path"), and an old CLI must not choke on a
    /// current daemon that sends both.
    #[test]
    fn peer_info_why_and_probes_wire_compat() {
        // An old daemon's payload: both fields absent.
        let old = r#"{"node_id":"n1","name":"pc","online":true,"connection":"relay"}"#;
        let p: PeerInfo = serde_json::from_str(old).unwrap();
        assert!(
            p.why.is_none(),
            "absent must be UNKNOWN, not 'nothing wrong'"
        );
        assert!(p.probes.is_empty());

        let mut p2 = p.clone();
        p2.why = Some(PeerWhy {
            tiers: vec![TierWhy {
                tier: "lan".into(),
                eligible: false,
                blocked_by: Some("peer-relays-instead".into()),
                base: 400.0,
                q: 0.0,
                penalty: 0.0,
                score: 400.0,
                fails: 0,
            }],
            relayed_instead_s: Some(174),
            relayed_instead_strikes: 3,
            forced_derp_s: None,
            probing: None,
        });
        p2.probes = vec![PathProbe {
            dst: "192.168.68.129:43881".into(),
            loss: Some(0.0),
            rtt_ms: Some(7.8),
            rtt_p95_ms: Some(98.0),
            rtt_max_ms: Some(169.0),
        }];
        let round: PeerInfo = serde_json::from_str(&serde_json::to_string(&p2).unwrap()).unwrap();
        assert_eq!(round, p2);

        // An OLD reader (no such fields) must still parse a current payload.
        #[derive(serde::Deserialize)]
        #[allow(dead_code)]
        struct OldPeerInfo {
            node_id: String,
            name: String,
            online: bool,
        }
        let as_old: OldPeerInfo = serde_json::from_str(&serde_json::to_string(&p2).unwrap())
            .expect("a current payload must not choke an older reader");
        assert_eq!(as_old.node_id, "n1");

        // A zero strike count and an empty probe list stay OFF the wire, so a
        // healthy fleet's `peers --json` is unchanged by this feature.
        let mut quiet = p.clone();
        quiet.why = Some(PeerWhy {
            tiers: vec![],
            relayed_instead_s: None,
            relayed_instead_strikes: 0,
            forced_derp_s: None,
            probing: None,
        });
        let json = serde_json::to_string(&quiet).unwrap();
        assert!(!json.contains("relayed_instead_strikes"), "{json}");
        assert!(!json.contains("probes"), "{json}");
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
            rx_denied_noroute: 0,
        });
        let s = serde_json::to_string(&p2).unwrap();
        assert!(s.contains(r#""tier":"relay""#) && s.contains(r#""tx":42"#));
        // P4 — a clean peer omits the counter entirely, so a healthy fleet's
        // `peers --json` is byte-identical to pre-P4.
        assert!(!s.contains("rx_denied"));
        let back: PeerInfo = serde_json::from_str(&s).unwrap();
        assert_eq!(back.debug, p2.debug);

        // …and a peer the ingress ACL refused surfaces the count, with the
        // NoRoute subset beside it (omitted when 0, absent-tolerant on read).
        let mut p3 = p2.clone();
        if let Some(d) = p3.debug.as_mut() {
            d.rx_denied = 17;
            d.rx_denied_noroute = 5;
        }
        let s3 = serde_json::to_string(&p3).unwrap();
        assert!(s3.contains(r#""rx_denied":17"#));
        assert!(s3.contains(r#""rx_denied_noroute":5"#));
        let back3: PeerInfo = serde_json::from_str(&s3).unwrap();
        let d3 = back3.debug.unwrap();
        assert_eq!(d3.rx_denied, 17);
        assert_eq!(d3.rx_denied_noroute, 5);
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
            id: "pg-buildhost".into(),
            kind: FlowKind::Forward,
            node: "aabbcc".into(),
            local: 15432,
            remote: Some("db:5432".into()),
            transport: "auto".into(),
            enabled: true,
            org: None,
        };
        assert_eq!(
            serde_json::to_string(&Request::RouteAdd {
                route: route.clone()
            })
            .unwrap(),
            r#"{"t":"route_add","d":{"route":{"id":"pg-buildhost","kind":"forward","node":"aabbcc","local":15432,"remote":"db:5432","transport":"auto","enabled":true}}}"#
        );
        assert_eq!(
            serde_json::to_string(&Request::RouteSetEnabled {
                id: "pg-buildhost".into(),
                enabled: false,
            })
            .unwrap(),
            r#"{"t":"route_set_enabled","d":{"id":"pg-buildhost","enabled":false}}"#
        );
        assert_eq!(
            serde_json::to_string(&Response::RouteAdded {
                route: route.clone()
            })
            .unwrap(),
            r#"{"t":"route_added","d":{"route":{"id":"pg-buildhost","kind":"forward","node":"aabbcc","local":15432,"remote":"db:5432","transport":"auto","enabled":true}}}"#
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
                org: None,
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
    fn org_status_wire_shape_and_omissions() {
        // Multi-org P1 — lock the OrgStatus row shape + the NodeStatus.orgs
        // omission rules (single-org daemons stay byte-identical on the wire;
        // zero counters and absent errors are skipped).
        let row = OrgStatus {
            label: "acme".into(),
            server_url: "https://acme.invalid".into(),
            tenant_id: Some("aabbccddeeff001122334455".into()),
            agent_id: Some("554433221100ffeeddccbbaa".into()),
            primary: false,
            enabled: true,
            connected: false,
            terminal_error: None,
            updates_ignored: 0,
            overlay_mode: String::new(),
        };
        assert_eq!(
            serde_json::to_string(&row).unwrap(),
            r#"{"label":"acme","server_url":"https://acme.invalid","tenant_id":"aabbccddeeff001122334455","agent_id":"554433221100ffeeddccbbaa","primary":false,"enabled":true,"connected":false}"#
        );
        // Old-daemon JSON (no orgs field) parses to an empty list; a
        // populated one round-trips.
        let old = r#"{"node_id":"n","name":"h","version":"v","mode":"service","connected":true}"#;
        let st: NodeStatus = serde_json::from_str(old).unwrap();
        assert!(st.orgs.is_empty());
        let mut with = st.clone();
        with.orgs = vec![OrgStatus {
            terminal_error: Some("server goodbye".into()),
            updates_ignored: 2,
            ..row
        }];
        let s = serde_json::to_string(&with).unwrap();
        assert!(
            s.contains(r#""terminal_error":"server goodbye""#),
            "got {s}"
        );
        assert!(s.contains(r#""updates_ignored":2"#), "got {s}");
        assert_eq!(serde_json::from_str::<NodeStatus>(&s).unwrap(), with);
        // Empty orgs is omitted entirely (old-CLI-identical wire).
        assert!(!serde_json::to_string(&st).unwrap().contains("orgs"));
    }

    /// FR-49 — an OLDER daemon reports no `overlay_mode`, and that must parse
    /// as "unknown", never as `off`. The two are opposite claims: `off` says
    /// the operator's device is not on that org's mesh; absent says nobody
    /// asked. Collapsing them would rebuild the exact ambiguity this field was
    /// added to remove.
    #[test]
    fn an_absent_overlay_mode_is_not_off() {
        let old = r#"{"label":"acme","server_url":"https://acme.invalid","primary":false,"enabled":true,"connected":true}"#;
        let row: OrgStatus = serde_json::from_str(old).unwrap();
        assert_eq!(row.overlay_mode, "", "absent stays empty, not \"off\"");
        assert_ne!(row.overlay_mode, "off");

        // And a reporting daemon's `off` is carried verbatim, so the two are
        // distinguishable downstream.
        let reported = r#"{"label":"acme","server_url":"https://acme.invalid","primary":false,"enabled":true,"connected":true,"overlay_mode":"off"}"#;
        let row: OrgStatus = serde_json::from_str(reported).unwrap();
        assert_eq!(row.overlay_mode, "off");
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
        assert_eq!(status.name, "devbox");
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

    /// The client must look for a per-user daemon FIRST and the system
    /// daemon second.
    ///
    /// Order is the whole contract. Reversed, every unprivileged daemon on a
    /// host that also has a system one would be shadowed; and without the
    /// system candidate at all, a root daemon is invisible from a user
    /// session on macOS — `temp_dir()` is per-user there, so the CLI would
    /// search its own `/var/folders/…`, find nothing, and report "not
    /// running" for a daemon that is running.
    #[cfg(unix)]
    #[test]
    fn socket_candidates_prefer_the_per_user_daemon() {
        let cands = super::unix_socket_candidates();
        assert_eq!(cands[0], super::user_socket_path(), "per-user first");
        assert_eq!(cands[1], super::system_socket_path(), "system second");
        assert_ne!(cands[0], cands[1], "the two must be distinct paths");
        assert!(
            cands[1].starts_with("/var/run/roomler"),
            "the system path must be well-known, not per-user: {:?}",
            cands[1]
        );
        // Every candidate is the same socket FILE name — only the directory
        // (and therefore the privilege domain) differs.
        for c in &cands {
            assert_eq!(c.file_name().unwrap(), super::LOCALAPI_SOCKET_NAME);
        }
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
        assert_eq!(client.status().await.unwrap().name, "devbox");
        assert_eq!(client.peers().await.unwrap().len(), 2);

        // The control socket must be private to the owner.
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "control socket must be 0600");

        sd_tx.send(true).unwrap();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), srv).await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ─── srflx health ────────────────────────────────────────────────

    #[test]
    fn srflx_health_is_candidate_presence() {
        // The whole point of the field: "no candidates" is the failure, and it
        // must be readable without interpreting an error string.
        assert!(!SrflxStatus::default().is_healthy());
        assert!(
            !SrflxStatus {
                stun_server: Some("1.2.3.4:3478".into()),
                error: Some("timed out".into()),
                ..Default::default()
            }
            .is_healthy()
        );
        assert!(
            SrflxStatus {
                candidates: vec!["94.130.141.98:43649".into()],
                nat: Some("cone".into()),
                ..Default::default()
            }
            .is_healthy()
        );
    }

    #[test]
    fn srflx_absent_and_empty_are_different_on_the_wire() {
        // A daemon that predates the field omits `srflx` entirely; a daemon
        // that MEASURED zero candidates sends an empty list. Collapsing those
        // two would turn "we don't know" into "it's broken" for every older
        // node in the fleet.
        let mut s: NodeStatus = serde_json::from_str(
            r#"{"node_id":"n","name":"x","version":"v","mode":"service","connected":true}"#,
        )
        .unwrap();
        assert!(s.srflx.is_none(), "absent must decode to None");
        let json = serde_json::to_string(&s).unwrap();
        assert!(!json.contains("srflx"), "None must not serialise: {json}");

        s.srflx = Some(SrflxStatus::default());
        let json = serde_json::to_string(&s).unwrap();
        assert!(
            json.contains(r#""srflx":{"candidates":[]}"#),
            "a measured zero MUST serialise: {json}"
        );
        let back: NodeStatus = serde_json::from_str(&json).unwrap();
        assert!(!back.srflx.unwrap().is_healthy());
    }

    /// Multi-org — the consent modal's `org` line is ADDITIVE: a daemon that
    /// predates it (no `org` key) must still deserialize, and a single-org
    /// device must not ship an empty string that renders a blank "On behalf
    /// of" row.
    #[test]
    fn consent_request_org_is_additive_and_omitted_when_empty() {
        // An older daemon's payload — no `org` key at all.
        let legacy: ConsentRequest = serde_json::from_str(
            r#"{"session_id":"abc","controller_name":"Alice","permissions":"VIEW_SCREEN","timeout_secs":30}"#,
        )
        .expect("a pre-org payload must still parse");
        assert_eq!(legacy.org, "");

        // A single-org device omits the key entirely rather than sending "".
        let single = ConsentRequest {
            session_id: "abc".into(),
            controller_name: "Alice".into(),
            permissions: "VIEW_SCREEN".into(),
            timeout_secs: 30,
            kind: String::new(),
            detail: String::new(),
            expires_at_ms: 0,
            surface: String::new(),
            org: String::new(),
        };
        let json = serde_json::to_string(&single).unwrap();
        assert!(
            !json.contains("\"org\""),
            "empty org must not be serialized: {json}"
        );

        // A multi-org device names the asking organization.
        let multi = ConsentRequest {
            org: "Acme GmbH".into(),
            ..single
        };
        let round: ConsentRequest =
            serde_json::from_str(&serde_json::to_string(&multi).unwrap()).unwrap();
        assert_eq!(round.org, "Acme GmbH");
    }

    /// FR-27 — `kind` / `detail` / `expires_at_ms` are additive in exactly the
    /// same way, and for the same reason: the desktop app and the daemon do not
    /// step forward in the same instant.
    ///
    /// The empty defaults are load-bearing, not incidental. A pre-FR-27 daemon
    /// wrote only remote-control prompts, so an absent `kind` MUST read as
    /// `rc` at the UI — an empty string rendered literally would label a real
    /// screen-control request with a blank type.
    #[test]
    fn consent_request_kind_and_detail_are_additive() {
        let legacy: ConsentRequest = serde_json::from_str(
            r#"{"session_id":"abc","controller_name":"Alice","permissions":"VIEW_SCREEN","timeout_secs":30}"#,
        )
        .expect("a pre-FR-27 payload must still parse");
        assert_eq!(legacy.kind, "");
        assert_eq!(legacy.detail, "");
        assert_eq!(legacy.expires_at_ms, 0);

        // An rc prompt carries no detail and must not ship empty keys.
        let rc = ConsentRequest {
            session_id: "abc".into(),
            controller_name: "Alice".into(),
            permissions: "VIEW".into(),
            timeout_secs: 30,
            kind: "rc".into(),
            detail: String::new(),
            expires_at_ms: 0,
            surface: String::new(),
            org: String::new(),
        };
        let json = serde_json::to_string(&rc).unwrap();
        assert!(
            !json.contains("\"detail\""),
            "empty detail must be omitted: {json}"
        );
        assert!(
            !json.contains("\"expires_at_ms\""),
            "an unknown deadline must be omitted, not sent as 0: {json}"
        );

        // An exec prompt carries the whole point of the change: what is about
        // to run, and until when.
        let exec = ConsentRequest {
            kind: "exec".into(),
            detail: "systemctl restart roomlerd".into(),
            expires_at_ms: 1_700_000_000_000,
            surface: String::new(),
            ..rc
        };
        let round: ConsentRequest =
            serde_json::from_str(&serde_json::to_string(&exec).unwrap()).unwrap();
        assert_eq!(round.kind, "exec");
        assert_eq!(round.detail, "systemctl restart roomlerd");
        assert_eq!(round.expires_at_ms, 1_700_000_000_000);
    }

    /// FR-27 — `surface` decides whether a UI RENDERS a prompt or merely lists
    /// it. The daemon writes a marker even when its own native panel is up
    /// (this list is also what `roomlerd consent --list` reads), so a UI that
    /// ignored the field would put a SECOND Approve button in front of one
    /// decision.
    ///
    /// The absent case is the load-bearing one: a pre-FR-27 daemon had no
    /// native panel at all, so "" must mean "nobody is showing it" — i.e. the
    /// companion should — and never be mistaken for "native".
    #[test]
    fn consent_request_surface_defaults_to_the_companion() {
        let legacy: ConsentRequest = serde_json::from_str(
            r#"{"session_id":"abc","controller_name":"Alice","permissions":"VIEW","timeout_secs":30}"#,
        )
        .expect("a pre-FR-27 payload must still parse");
        assert_eq!(legacy.surface, "");
        assert_ne!(legacy.surface, "native", "absent must never read as native");

        let native = ConsentRequest {
            session_id: "abc".into(),
            controller_name: "Alice".into(),
            permissions: "VIEW".into(),
            timeout_secs: 30,
            kind: "rc".into(),
            detail: String::new(),
            expires_at_ms: 0,
            surface: "native".into(),
            org: String::new(),
        };
        let round: ConsentRequest =
            serde_json::from_str(&serde_json::to_string(&native).unwrap()).unwrap();
        assert_eq!(round.surface, "native");

        // A companion-served prompt omits the key rather than shipping the
        // default spelling, exactly as `org` does.
        let companion = ConsentRequest {
            surface: String::new(),
            ..native
        };
        let json = serde_json::to_string(&companion).unwrap();
        assert!(
            !json.contains("\"surface\""),
            "an empty surface must be omitted: {json}"
        );
    }
}
