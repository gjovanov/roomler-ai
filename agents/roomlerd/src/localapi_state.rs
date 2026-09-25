// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! The agent daemon's [`LocalApiState`] — the live data the LocalAPI serves.
//!
//! Thin adapter (unification P1): turns the agent's enrolled identity + a
//! "connected" flag + the overlay runtime's published [`OverlayView`] into the
//! read-only `status` / `peers` / `flows` that the CLI (`roomler`) and the
//! desktop app read over the local pipe/socket (`tunnel_core::localapi::serve`).
//!
//! Wired in `run_cmd`: the connected flag and the overlay `watch` channel are
//! created there (stable across WS reconnects), the signaling loop updates them,
//! and the listener reads this state.

use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::sync::watch;
use tunnel_core::localapi::{
    ConnectionType, ConsentRequest, DaemonMode, FlowInfo, LocalApiState, NodeStatus, OverlayView,
    PeerInfo, Response,
};

/// How often the RTT prober ICMP-pings each carrier-reachable peer (P3b-3).
pub const RTT_PROBE_INTERVAL: Duration = Duration::from_secs(15);
/// A cached RTT older than this is dropped from the peer view (the peer stopped
/// answering) so the column fades to "—" rather than showing a stale number.
/// ~3 missed probes.
pub const RTT_STALE: Duration = Duration::from_secs(45);
/// Per-peer ICMP probe timeout — short so one unreachable peer can't stretch the
/// sequential cycle past [`RTT_PROBE_INTERVAL`].
const RTT_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Shared RTT cache: overlay-ip → (last measured RTT ms, when measured). Written
/// by the prober task, read by [`DaemonState::peers`].
pub type RttCache = Arc<Mutex<HashMap<String, (u32, Instant)>>>;

/// The netstack ICMP backend behind the `ping` verb, abstracted so
/// [`DaemonState`] never names the feature-gated `NetstackHandle` type. The
/// concrete impl lives in `crate::overlay` (feature `overlay-netstack`); `None`
/// on any node not running the userspace stack.
#[async_trait]
pub trait NetstackPinger: Send + Sync {
    /// ICMP-ping `dst` (already resolved, either family) over the netstack;
    /// `Ok(rtt)` on reply.
    async fn ping(&self, dst: IpAddr, timeout: Duration) -> Result<Duration, String>;
}

/// P8-cosmetics — OS-level ICMP pinger for OS-TUN nodes, so the `peers` RTT
/// column is populated everywhere (it was netstack-only before — "— by-design
/// on OS-TUN"). Shells the platform `ping` once per probe and parses ping's
/// OWN reported round-trip (subprocess spawn cost never pollutes the number).
/// The overlay ICMP path is exactly what a user's own `ping 100.64.x.y`
/// exercises, so the column now shows the same truth they'd measure.
pub struct OsPinger;

#[async_trait]
impl NetstackPinger for OsPinger {
    async fn ping(&self, dst: IpAddr, timeout: Duration) -> Result<Duration, String> {
        let target = dst.to_string();
        let mut cmd = tokio::process::Command::new("ping");
        #[cfg(windows)]
        {
            let ms = timeout.as_millis().max(100).to_string();
            cmd.args(["-n", "1", "-w", &ms, &target]);
            // CREATE_NO_WINDOW — never flash a console under an interactive
            // session (the prober runs every 15 s).
            cmd.creation_flags(0x0800_0000);
        }
        #[cfg(target_os = "linux")]
        {
            let secs = timeout.as_secs().max(1).to_string();
            cmd.args(["-c", "1", "-W", &secs, &target]);
        }
        #[cfg(target_os = "macos")]
        {
            let ms = timeout.as_millis().max(1000).to_string();
            cmd.args(["-c", "1", "-W", &ms, &target]);
        }
        cmd.stdin(std::process::Stdio::null());
        let out = tokio::time::timeout(timeout + Duration::from_secs(2), cmd.output())
            .await
            .map_err(|_| "ping subprocess timed out".to_string())?
            .map_err(|e| e.to_string())?;
        if !out.status.success() {
            return Err("no reply".into());
        }
        parse_ping_ms(&String::from_utf8_lossy(&out.stdout))
            .map(Duration::from_millis)
            .ok_or_else(|| "no rtt in ping output".into())
    }
}

/// Locale-tolerant extraction of the round-trip from `ping` output: the
/// integer immediately before the first standalone-ish "ms" ("time=4ms",
/// "Zeit=4ms", "time<1ms", "time=0.523 ms" ⇒ 4 / 4 / 1 / 0). Pure.
pub(crate) fn parse_ping_ms(s: &str) -> Option<u64> {
    for (idx, _) in s.match_indices("ms") {
        let head = s[..idx].trim_end();
        let num: String = head
            .chars()
            .rev()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        if num.is_empty() {
            continue; // "ms" inside a word ("streams") — keep scanning
        }
        if let Ok(v) = num.split('.').next().unwrap_or("").parse::<u64>() {
            return Some(v);
        }
    }
    None
}

/// FR-85 — the answer when this daemon has no recorder attached.
#[cfg(feature = "recording")]
fn recording_unavailable() -> Response {
    Response::Error {
        message: "screen recording is not available on this daemon".into(),
    }
}

/// Live daemon state behind the LocalAPI. Built once in `run_cmd`, wrapped in an
/// `Arc<dyn LocalApiState>` for the listener; reads are cheap clones off a
/// `watch` borrow + an atomic load.
pub struct DaemonState {
    node_id: String,
    /// Mutable so a LocalAPI rename is reflected in `status()` immediately
    /// (the server-side name still updates on the next reconnect's hello).
    name: Mutex<String>,
    version: String,
    mode: DaemonMode,
    tenant_id: Option<String>,
    /// Set while a signaling WS connection is live (updated by the signaling
    /// loop's per-connection guard). `peers()` reports none when this is false,
    /// since the overlay carriers are torn down on WS drop and the last view is
    /// stale.
    connected: Arc<AtomicBool>,
    /// The overlay runtime's latest view. An empty `Default` when overlay is
    /// disabled or this build lacks `overlay-l3` (nothing publishes) — so
    /// `peers()` is simply empty there.
    overlay: watch::Receiver<OverlayView>,
    /// The SAME consent broker the signaling loop prompts on (injected from
    /// `run_cmd`) — so `consent_decide` gates on the LIVE pending set and reads
    /// the broker's own sentinel dir, rather than a throwaway broker over a
    /// re-resolved path.
    consent: crate::consent::ConsentBroker,
    /// FR-51 P4 — the primary enrollment is ephemeral (from the loaded
    /// config). Surfaces in `status()` so `roomler status` can say the device
    /// removes itself.
    ephemeral: bool,
    /// FR-27 — live remote-control sessions, written by every signalling
    /// loop through its `ViewerIndicator`. `None` in tests and in any daemon
    /// shape that never runs signalling.
    rc_sessions: Option<crate::rc_sessions::RcSessionRegistry>,
    /// The netstack ICMP backend for the `ping` verb. `None` on a node not
    /// running the userspace stack (OS-TUN or non-overlay build).
    pinger: Option<Arc<dyn NetstackPinger>>,
    /// The tunnel-client hub (P3b-2 PR-C) — the same instance the signaling loop
    /// publishes its egress into. Backs the `flows` / `create_forward` /
    /// `create_socks5` / `kill_flow` verbs; the daemon originates tunnels over
    /// its own agent WS.
    tunnel_hub: crate::tunnel::client_mgr::TunnelClientHub,
    /// Per-peer RTT cache filled by the ICMP prober task (P3b-3), read by
    /// `peers()` to populate `rtt_ms`. Empty on a node without the netstack
    /// pinger (no prober runs) → `rtt_ms` stays `None`.
    rtt_cache: RttCache,
    /// P6: the declared-route reconciler backing the `Route*` verbs.
    /// `None` in unit tests / states built without one — the verbs then
    /// report empty/unsupported via the trait defaults' semantics.
    routes: Option<crate::tunnel::route_reconciler::RouteReconciler>,
    /// The daemon's resolved config path + the daemon-wide write lock, backing
    /// the `SetDeviceName` verb. `None` in unit tests / states built without a
    /// persist target — the verb then reports unsupported.
    config_persist: Option<(std::path::PathBuf, crate::config::WriteLock)>,
    /// Multi-org P1: live per-enrollment rows surfaced as `NodeStatus.orgs`.
    /// `None` in unit tests / states built without one → the field is empty
    /// (old-daemon shape).
    orgs: Option<OrgStatusRegistry>,
    /// Multi-org — every SECONDARY org's live overlay view, labelled (see
    /// [`OrgViewRegistry`]). `None`/empty ⇒ single-org: `peers` returns the
    /// primary's flat list exactly as before.
    org_views: Option<OrgViewRegistry>,
    /// The daemon's live gate-4 flags, so a `ConfigSet` here takes effect at
    /// the same moment a pushed change would (`docs/remote-config.md`).
    /// `None` in unit tests → the save still lands, only the live re-seed is
    /// skipped.
    remote_config: Option<crate::remote_config::RemoteConfigServices>,
    /// FR-85 — the recorder's supervisor behind the recording verbs. `None`
    /// in unit tests and daemon shapes built without one → the verbs answer
    /// "not available". (The console-user gate is the LocalAPI listener's,
    /// applied before any of them reaches this state.)
    #[cfg(feature = "recording")]
    recorder: Option<Arc<crate::recording::manager::RecordingManager>>,
    /// FR-84 D5b — the org device list + mesh proxy behind the `Devices` /
    /// `Mesh` verbs (per-org server + agent token, a short cache). `None` in
    /// unit tests / states built without one → the trait default
    /// (`Upstream { code: "unsupported" }`).
    self_view: Option<Arc<crate::self_view::SelfView>>,
    /// FR-84 D3 — the `RestartDaemon` verb's handle into `run_cmd`. `None` in
    /// unit tests and daemon shapes built without one → the verb answers
    /// "not available", which is also what an older daemon's clients expect.
    restart: Option<RestartHandle>,
    /// FR-84 D3 — when this daemon process started (unix ms), reported with
    /// its pid so a client can tell the relaunched daemon from the one that
    /// left even when Windows hands the new process the old PID.
    started_at_ms: u64,
}

/// FR-84 D3 — what the `RestartDaemon` verb needs from `run_cmd`: who
/// supervises this process, the internal-shutdown sender the auto-updater
/// already uses, when the process started, and where the last accepted
/// restart is recorded. `run_cmd` keeps a clone and reads
/// [`Self::accepted_exit_code`] back once the graceful shutdown is done.
///
/// The verb ARMS a restart (records it, answers `DaemonRestarting`); only
/// [`Self::commit`] — called by the LocalAPI connection loop once that
/// answer has been written — starts the shutdown, so a caller never loses its
/// connection before it knows the restart was accepted.
#[derive(Clone)]
pub struct RestartHandle {
    inner: Arc<RestartInner>,
}

struct RestartInner {
    supervision: crate::supervision::Supervision,
    shutdown: watch::Sender<bool>,
    started: Instant,
    record_path: std::path::PathBuf,
    /// One decision at a time: two requests racing must not both pass the
    /// rate limit before either has recorded itself.
    decide: tokio::sync::Mutex<()>,
    /// The accepted restart's exit code — set once, when a restart is armed.
    accepted: std::sync::OnceLock<i32>,
    /// The shutdown has been started (idempotence for [`RestartHandle::commit`]).
    committed: AtomicBool,
}

/// If the connection loop never gets to [`RestartHandle::commit`] (its task
/// was torn down between the answer and the write), an accepted restart
/// still happens this long after it was armed. The answer is ~100 bytes into
/// a pipe buffer, so on the normal path the commit comes first by orders of
/// magnitude; this only makes "accepted ⇒ restarted" unconditional.
const RESTART_COMMIT_FALLBACK: Duration = Duration::from_secs(5);

impl RestartHandle {
    pub fn new(
        supervision: crate::supervision::Supervision,
        shutdown: watch::Sender<bool>,
        started: Instant,
        record_path: std::path::PathBuf,
    ) -> Self {
        Self {
            inner: Arc::new(RestartInner {
                supervision,
                shutdown,
                started,
                record_path,
                decide: tokio::sync::Mutex::new(()),
                accepted: std::sync::OnceLock::new(),
                committed: AtomicBool::new(false),
            }),
        }
    }

    /// The exit code of the restart this process accepted, if it accepted
    /// one. `run_cmd` leaves with it after the graceful shutdown.
    pub fn accepted_exit_code(&self) -> Option<i32> {
        self.inner.accepted.get().copied()
    }

    /// Start the graceful shutdown of an ACCEPTED restart. Idempotent, and a
    /// no-op when nothing was accepted.
    pub fn commit(&self) {
        if self.inner.accepted.get().is_none() || self.inner.committed.swap(true, Ordering::SeqCst)
        {
            return;
        }
        tracing::info!(
            supervisor = self.inner.supervision.wire(),
            "restart: the answer is out — starting the graceful shutdown"
        );
        let _ = self.inner.shutdown.send(true);
    }
}

/// Wall-clock ms since the epoch — the restart record is compared across
/// process lifetimes, so it cannot be a monotonic clock.
fn unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Multi-org P1 — one enrollment's live handles, seeded by `run_cmd` and
/// read by `status()`. The `connected` flag is the SAME per-connection
/// `AtomicBool` the org's signaling guard flips; `terminal_error` is written
/// by the org supervisor when its loop stops permanently.
pub struct OrgRuntime {
    pub label: String,
    pub server_url: String,
    pub tenant_id: String,
    pub agent_id: String,
    pub primary: bool,
    pub enabled: bool,
    pub connected: Arc<AtomicBool>,
    pub terminal_error: Arc<Mutex<Option<String>>>,
    pub updates_ignored: Arc<std::sync::atomic::AtomicU32>,
    /// FR-49 — this enrollment's overlay participation (`OrgOverlayMode::wire`).
    ///
    /// A config-level decision rather than a live handle, so it needs no `Arc`;
    /// it is re-seeded whenever a row is, which is also when the config that
    /// decides it was last read.
    pub overlay_mode: &'static str,
}

/// Shared registry of [`OrgRuntime`] rows (primary first).
pub type OrgStatusRegistry = Arc<Mutex<Vec<OrgRuntime>>>;

/// Multi-org — one SECONDARY org's live overlay view, labelled.
///
/// Each org runs its own [`OverlayRuntime`] publishing to its own `watch`
/// channel; before this registry the secondaries' receivers were dropped on
/// the floor (`main.rs`, `let (tx, _rx) = watch::channel(...)`), so `peers`
/// could only ever show the primary's mesh. Registered at boot for every
/// `[[orgs]]` entry and on a live `rc:agent.join_org`.
pub type OrgViewRegistry = Arc<Mutex<Vec<(String, watch::Receiver<OverlayView>)>>>;

impl DaemonState {
    /// Build from the enrolled config identity + the live handles. `mode` is the
    /// privilege the daemon runs at (today's agent is always the full "be
    /// accessed" service node → [`DaemonMode::Service`]; the unprivileged
    /// user-mode daemon arrives with the binary unification at P3).
    #[allow(clippy::too_many_arguments)] // a daemon-state constructor; grouping would obscure
    pub fn new(
        node_id: String,
        name: String,
        mode: DaemonMode,
        tenant_id: Option<String>,
        connected: Arc<AtomicBool>,
        overlay: watch::Receiver<OverlayView>,
        consent: crate::consent::ConsentBroker,
        pinger: Option<Arc<dyn NetstackPinger>>,
        tunnel_hub: crate::tunnel::client_mgr::TunnelClientHub,
        rtt_cache: RttCache,
    ) -> Self {
        Self {
            node_id,
            name: Mutex::new(name),
            version: env!("CARGO_PKG_VERSION").to_string(),
            mode,
            tenant_id,
            connected,
            overlay,
            consent,
            rc_sessions: None,
            pinger,
            tunnel_hub,
            rtt_cache,
            routes: None,
            config_persist: None,
            orgs: None,
            org_views: None,
            remote_config: None,
            self_view: None,
            ephemeral: false,
            #[cfg(feature = "recording")]
            recorder: None,
            restart: None,
            // Built once, in `run_cmd`, seconds into the process's life.
            started_at_ms: unix_now_ms(),
        }
    }

    /// FR-84 D3 — attach the restart verb's handle (see [`RestartHandle`]).
    /// The verb reads `local_restart_enabled` through [`Self::with_config_persist`]'s
    /// path, so both must be attached for it to accept anything.
    pub fn with_restart(mut self, restart: RestartHandle) -> Self {
        self.restart = Some(restart);
        self
    }

    /// FR-85 — attach the recorder's supervisor so the recording verbs are
    /// live.
    #[cfg(feature = "recording")]
    pub fn with_recorder(
        mut self,
        recorder: Arc<crate::recording::manager::RecordingManager>,
    ) -> Self {
        self.recorder = Some(recorder);
        self
    }

    /// FR-84 D5b — attach the per-org HTTP registry (server URL + agent
    /// token per enrollment, seeded next to the `OrgRuntime` rows) so the
    /// `Devices` / `Mesh` verbs can ask the org's server. Builder style like
    /// the rest, so `new()`'s call sites stay put.
    pub fn with_org_http(mut self, registry: crate::self_view::OrgHttpRegistry) -> Self {
        self.self_view = Some(Arc::new(crate::self_view::SelfView::new(registry)));
        self
    }

    /// FR-51 P4 — stamp the primary enrollment's ephemeral nature (builder
    /// style like `with_config_persist`, so `new()`'s arg list stays put).
    pub fn with_ephemeral(mut self, ephemeral: bool) -> Self {
        self.ephemeral = ephemeral;
        self
    }

    /// FR-27 — attach the live remote-control session registry, so a thin
    /// client can render "Being viewed by …" and offer a Disconnect. Absent
    /// (tests, and any daemon shape that never runs signalling) ⇒ an empty
    /// list and a `false` disconnect, which is the honest answer there.
    pub fn with_rc_sessions(mut self, sessions: crate::rc_sessions::RcSessionRegistry) -> Self {
        self.rc_sessions = Some(sessions);
        self
    }

    /// Attach the multi-org status registry so `NodeStatus.orgs` is live.
    /// Separate from `new` to keep the constructor's existing call sites
    /// (incl. tests) unchanged.
    pub fn with_orgs(mut self, orgs: OrgStatusRegistry) -> Self {
        self.orgs = Some(orgs);
        self
    }

    /// Multi-org — attach the per-secondary-org overlay views so `peers`
    /// reports EVERY org's mesh, not just the primary's. Absent (or empty) ⇒
    /// single-org behaviour, byte-identical.
    pub fn with_org_views(mut self, views: OrgViewRegistry) -> Self {
        self.org_views = Some(views);
        self
    }

    /// Attach the declared-route reconciler (P6) so the `Route*` verbs are
    /// live. Separate from `new` to keep the constructor's existing call
    /// sites (incl. tests) unchanged.
    pub fn with_routes(mut self, routes: crate::tunnel::route_reconciler::RouteReconciler) -> Self {
        self.routes = Some(routes);
        self
    }

    /// Attach the config write path + the daemon-wide write lock so the
    /// `SetDeviceName` verb can persist — the daemon writes ITS OWN config
    /// (profile-correct under SYSTEM, where an unelevated desktop app's direct
    /// file write is denied). Same load→mutate→save-under-lock discipline as
    /// the route reconciler (P6).
    pub fn with_config_persist(
        mut self,
        path: std::path::PathBuf,
        lock: crate::config::WriteLock,
    ) -> Self {
        self.config_persist = Some((path, lock));
        self
    }

    /// Attach the daemon's live gate-4 flags so a `ConfigSet` through this
    /// LocalAPI takes effect at the same moment a SERVER push would.
    ///
    /// Without it, `exec_enabled` had an inversion: a pushed change was live
    /// immediately while the owner's own edit sat in the file until the daemon
    /// restarted. Gate 4 is the refusal that belongs to whoever holds the
    /// machine; it cannot be the slower of the two.
    pub fn with_remote_config(
        mut self,
        remote_config: crate::remote_config::RemoteConfigServices,
    ) -> Self {
        self.remote_config = Some(remote_config);
        self
    }

    /// Resolve a `ping` target — a literal overlay IP (either family) or a peer
    /// **name** — against the live mesh view. Mirrors the netstack SOCKS
    /// front's resolver (bare label / first DNS label), but reads the view
    /// `DaemonState` holds. A name resolves to the peer's IPv4 by default, or
    /// its *derived* overlay IPv6 (published by the runtime) with `prefer_v6`;
    /// a literal is used as-is (an unroutable v6 fails cleanly at the send).
    fn resolve_overlay(&self, target: &str, prefer_v6: bool) -> Option<IpAddr> {
        if let Ok(ip) = target.parse::<IpAddr>() {
            return Some(ip);
        }
        let tl = target.to_ascii_lowercase();
        let bare = tl.split('.').next().unwrap_or(&tl).to_string();
        self.overlay.borrow().peers.iter().find_map(|p| {
            let n = p.name.to_ascii_lowercase();
            if !p.name.is_empty() && (n == tl || n == bare) {
                let v4 = p.overlay_ip.as_deref();
                let pick = if prefer_v6 {
                    // Fall back to v4 if no published v6 (shouldn't happen —
                    // the runtime derives one whenever the v4 exists).
                    p.overlay_ip6.as_deref().or(v4)
                } else {
                    v4
                };
                pick.and_then(|s| s.parse::<IpAddr>().ok())
            } else {
                None
            }
        })
    }
}

#[async_trait]
impl LocalApiState for DaemonState {
    fn status(&self) -> NodeStatus {
        NodeStatus {
            node_id: self.node_id.clone(),
            name: self.name.lock().map(|n| n.clone()).unwrap_or_default(),
            version: self.version.clone(),
            mode: self.mode,
            tenant_id: self.tenant_id.clone(),
            // The overlay IP the runtime last assigned us — a stable identity,
            // so it's kept even across a brief disconnect.
            overlay_ip: self.overlay.borrow().self_ip.clone(),
            overlay_ip6: self.overlay.borrow().self_ip6.clone(),
            connected: self.connected.load(Ordering::Relaxed),
            // FR-51 P4 — the primary enrollment's nature, stamped at startup.
            ephemeral: self.ephemeral,
            // P5/S4 — exit-node routing status the overlay runtime published.
            exit_node: self.overlay.borrow().exit_node.clone(),
            // S1b — the config file this daemon actually loaded, so the
            // desktop stops guessing which copy is live.
            config_path: self
                .config_persist
                .as_ref()
                .map(|(p, _)| p.display().to_string()),
            // S2 — MagicDNS status the overlay runtime published.
            dns: self.overlay.borrow().dns.clone(),
            // NAT-traversal — the srflx gather outcome. Empty candidates means
            // this node can't hole-punch and reads as UDP-blocked to every peer.
            srflx: self.overlay.borrow().srflx.clone(),
            // FR-33 — captured LAN prefixes, from the process-wide netstate
            // snapshot (a HOST property, like netcheck). `None` when the
            // monitor is not running — and, since 2026-09-06, also when the
            // operator switched the probe OFF: an empty list read as `clear`
            // in the field (CORPLAP-3, kill-switch cycle), the same word as a
            // genuinely clear host. The probe flag travels separately so a
            // new CLI can say "probe OFF" instead of going silent.
            #[cfg(feature = "overlay-l3")]
            lan_captures: if tunnel_core::env::flag("OVERLAY_LAN_CAPTURE_PROBE", true) {
                tunnel_core::overlay::netstate::handle().map(|h| {
                    h.snapshot()
                        .lan_captures
                        .iter()
                        .map(|c| tunnel_core::localapi::LanCaptureStatus {
                            prefix: c.prefix.clone(),
                            owner: c.owner.clone(),
                            via: c.via_ifref.clone(),
                            via_name: c.via_name.clone(),
                        })
                        .collect()
                })
            } else {
                None
            },
            #[cfg(feature = "overlay-l3")]
            lan_capture_probe: tunnel_core::overlay::netstate::handle()
                .map(|_| tunnel_core::env::flag("OVERLAY_LAN_CAPTURE_PROBE", true)),
            #[cfg(not(feature = "overlay-l3"))]
            lan_captures: None,
            #[cfg(not(feature = "overlay-l3"))]
            lan_capture_probe: None,
            // FR-47 — the last join the server refused, if any. Read from the
            // process-wide slot rather than the overlay view: a refusal means
            // the runtime never came up, so there is no view to carry it.
            //
            // Feature-gated like `lan_captures` above, because `crate::overlay`
            // itself is: a signalling-only build has no overlay module and can
            // never be refused a join it does not attempt, so `None` there is
            // the truth rather than a stub.
            #[cfg(any(feature = "overlay-l3", feature = "overlay-netstack"))]
            join_refusal: crate::overlay::last_join_refusal(),
            #[cfg(not(any(feature = "overlay-l3", feature = "overlay-netstack")))]
            join_refusal: None,
            // C4 stage 1 — the warm TURN/UDP allocation's state.
            warm_relay: self.overlay.borrow().warm_relay.clone(),
            // B4 — the measured capability vector, read straight from the
            // process-wide netcheck slot (a HOST property; no runtime
            // threading needed). The slot lives behind tunnel-core's
            // `overlay-l3` feature; a signalling-only build reports None
            // exactly like a pre-B4 daemon.
            #[cfg(feature = "overlay-l3")]
            netcheck: tunnel_core::overlay::netcheck::current().map(|(v, age)| {
                tunnel_core::localapi::NetcheckStatus {
                    stun_udp: v.stun_udp,
                    relay_band_udp: v.relay_band_udp,
                    derp_ws_ok: v.derp_ws_ok,
                    nat: v.nat,
                    age_s: age.as_secs(),
                }
            }),
            #[cfg(not(feature = "overlay-l3"))]
            netcheck: None,
            // Multi-org P1 — one row per enrollment; empty (and omitted on
            // the wire) for a single-org daemon or a state built without
            // the registry.
            orgs: self
                .orgs
                .as_ref()
                .map(|reg| {
                    reg.lock()
                        .map(|rows| {
                            rows.iter()
                                .map(|o| tunnel_core::localapi::OrgStatus {
                                    label: o.label.clone(),
                                    server_url: o.server_url.clone(),
                                    tenant_id: (!o.tenant_id.is_empty())
                                        .then(|| o.tenant_id.clone()),
                                    agent_id: (!o.agent_id.is_empty()).then(|| o.agent_id.clone()),
                                    primary: o.primary,
                                    enabled: o.enabled,
                                    connected: o.connected.load(Ordering::Relaxed),
                                    terminal_error: o
                                        .terminal_error
                                        .lock()
                                        .ok()
                                        .and_then(|t| t.clone()),
                                    updates_ignored: o.updates_ignored.load(Ordering::Relaxed),
                                    overlay_mode: o.overlay_mode.to_string(),
                                })
                                .collect()
                        })
                        .unwrap_or_default()
                })
                .unwrap_or_default(),
            // PR-B1 — per-socket receive liveness + band-walk tripwire.
            direct_socks: self.overlay.borrow().direct_socks.clone(),
            // #32 — the `/derp` inbound-drop counters, verbatim from the
            // overlay view. `None` when the overlay is not running.
            derp_inbound_drops: self.overlay.borrow().derp_inbound_drops,
            direct_bind_walks: Some(
                tunnel_core::evidence::DIRECT_BIND_WALKS.load(Ordering::Relaxed),
            ),
            roam_adoptions: Some(tunnel_core::evidence::ROAM_ADOPTIONS.load(Ordering::Relaxed)),
            // FR-68 — route-guard evidence, read together so two snapshots can
            // be diffed as one reading. Always Some: a zero here is a real
            // measurement (a quiet host, or a platform whose guard is a no-op),
            // not an absence.
            route_guard: Some((
                tunnel_core::evidence::ROUTE_EVICTIONS.load(Ordering::Relaxed),
                tunnel_core::evidence::ROUTE_SIBLING_SPARES.load(Ordering::Relaxed),
                tunnel_core::evidence::ROUTE_WAVES.load(Ordering::Relaxed),
                tunnel_core::evidence::FORCED_REVALIDATIONS.load(Ordering::Relaxed),
            )),
            // #1282 — read alongside route_guard so one snapshot is internally
            // consistent: tick + event should equal route_guard's wave total.
            route_wave_arms: Some((
                tunnel_core::evidence::ROUTE_WAVES_TICK.load(Ordering::Relaxed),
                tunnel_core::evidence::ROUTE_WAVES_EVENT.load(Ordering::Relaxed),
            )),
            // #1328 — same snapshot, because the number only means something
            // next to the eviction count it is supposed to be bounding.
            route_yields: Some(tunnel_core::evidence::ROUTE_YIELDS.load(Ordering::Relaxed)),
            disco_answered: Some(tunnel_core::evidence::DISCO_ANSWERED.load(Ordering::Relaxed)),
            // FR-19 — present ONLY when the responder actually bound, so a
            // failed bind reads as "no relay here" rather than a phantom one.
            //
            // Gated to match `relay_server` itself: a build without the overlay
            // features has no responder to report, and this file DOES compile
            // in those lanes (ffmpeg-encoder, vp9-444), which a local check run
            // only with `--features overlay-l3` will not reveal.
            #[cfg(any(feature = "overlay-l3", feature = "overlay-netstack"))]
            org_relay: crate::relay_server::status().map(|(listening, c)| {
                tunnel_core::localapi::OrgRelayStatus {
                    listening,
                    answered: c.answered,
                    refused_not_shaped: c.refused_not_shaped,
                    refused_not_probe: c.refused_not_probe,
                    refused_rate_limited: c.refused_rate_limited,
                }
            }),
            #[cfg(not(any(feature = "overlay-l3", feature = "overlay-netstack")))]
            org_relay: None,
            // FR-46 — what this daemon has actually READ through a retired
            // prefix. Always `Some`, never `None`, on a daemon new enough to
            // have the field: `None` is reserved for "this daemon predates the
            // field", and reporting it for an empty set would make an answered
            // question indistinguishable from an unanswerable one.
            legacy_env_uses: Some(tunnel_core::env::legacy_env_uses()),
            retired_env_present: Some(tunnel_core::env::retired_env_present()),
            // FR-84 D3 — which process answered, so a client waiting out a
            // restart knows the relaunched daemon from the one that is leaving.
            pid: Some(std::process::id()),
            started_at_ms: Some(self.started_at_ms),
        }
    }

    fn peers(&self) -> Vec<PeerInfo> {
        // A peer list from a dropped connection is misleading — the carriers are
        // gone. Report none until reconnected + re-synced.
        if !self.connected.load(Ordering::Relaxed) {
            return Vec::new();
        }
        let mut peers = self.overlay.borrow().peers.clone();
        // Multi-org — append every SECONDARY org's mesh, labelled. Each org
        // runs its own overlay engine with a DISJOINT peer set and address
        // space, so without the label a merged list is ambiguous (the same
        // host can appear once per shared org under different overlay IPs).
        // Only stamp when there IS more than one org, so a single-org daemon's
        // output stays byte-identical.
        if let Some(views) = &self.org_views
            && let Ok(views) = views.lock()
            && !views.is_empty()
        {
            let primary_label = self
                .orgs
                .as_ref()
                .and_then(|o| o.lock().ok())
                .and_then(|rows| rows.iter().find(|r| r.primary).map(|r| r.label.clone()))
                .unwrap_or_else(|| "primary".to_string());
            for p in &mut peers {
                p.org = primary_label.clone();
            }
            for (label, rx) in views.iter() {
                let mut org_peers = rx.borrow().peers.clone();
                for p in &mut org_peers {
                    p.org = label.clone();
                }
                peers.extend(org_peers);
            }
        }
        // P3b-3: overlay a peer as `Tunnel` when its WG carrier is down and the
        // daemon reaches its backing agent via a live tunnel flow.
        apply_tunnel_override(&mut peers, &self.tunnel_hub.active_flow_agent_ids());
        // P3b-3: fill `rtt_ms` from the ICMP prober cache (netstack nodes via
        // the userspace pinger; OS-TUN nodes via `OsPinger` since P8-cosmetics).
        // A stale entry (peer stopped answering) is dropped so the column fades
        // to "—" rather than lying. Empty cache (prober warming up) → `None`.
        if let Ok(cache) = self.rtt_cache.lock()
            && !cache.is_empty()
        {
            for p in &mut peers {
                if let Some(ip) = p.overlay_ip.as_deref()
                    && let Some((ms, at)) = cache.get(ip)
                    && at.elapsed() < RTT_STALE
                {
                    p.rtt_ms = Some(*ms);
                }
            }
        }
        peers
    }

    fn flows(&self) -> Vec<FlowInfo> {
        // P3b-2 PR-C: the tunnel data plane folded into the daemon — report the
        // supervised forwards / SOCKS5 listeners it originates over its agent WS.
        self.tunnel_hub.flows_snapshot()
    }

    fn consent_pending(&self) -> Vec<ConsentRequest> {
        // Read the broker's OWN sentinel dir — resolved in-process, so it's the
        // daemon's real profile even under SystemContext, where the interactive-
        // user tray reading the dir directly would look in the WRONG profile (the
        // P2b bug fix). Same parse the tray's cmd_get_pending_consents used to do.
        //
        // P2b review L2: the scan is bounded. Only the daemon writes here, so
        // dozens of pendings would already be a bug — but this fn runs on every
        // tray/CLI poll (~750 ms cadence), and an unbounded read-parse loop over
        // a corrupted / adversarially stuffed directory must not turn the
        // LocalAPI thread into an I/O grinder.
        const MAX_PENDING_SCAN: usize = 64;
        let Ok(entries) = std::fs::read_dir(self.consent.sentinel_dir()) else {
            return Vec::new(); // dir not created yet ⇒ nothing pending
        };
        let mut out = Vec::new();
        for entry in entries.flatten() {
            if out.len() >= MAX_PENDING_SCAN {
                tracing::warn!(
                    cap = MAX_PENDING_SCAN,
                    "localapi: consent_pending hit the scan cap — sentinel dir has \
                     implausibly many pending entries, truncating the listing"
                );
                break;
            }
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("pending") {
                continue;
            }
            if let Ok(body) = std::fs::read_to_string(&path)
                && let Ok(pc) = serde_json::from_str::<ConsentRequest>(&body)
            {
                out.push(pc);
            }
        }
        out
    }

    fn consent_decide(&self, session_id: &str, allow: bool) -> bool {
        // SECURITY: the session id becomes a sentinel FILE NAME, so reject
        // anything that isn't a 24-char hex ObjectId before it's used (path-
        // traversal / injection guard). The pipe SDDL already limits WHO can call
        // this (SYSTEM + Administrators + interactive user, ≥ medium integrity).
        if !is_hex_object_id(session_id) {
            tracing::warn!(
                session = %session_id,
                "localapi: rejecting consent decision — session id is not a 24-char hex ObjectId"
            );
            return false;
        }
        // Record via the LIVE broker: honored ONLY if the session is actively
        // being prompted (no pre-approval / confused-deputy — the decision is an
        // answer to a question the broker is currently asking).
        self.consent.record_decision(session_id, allow)
    }

    fn rc_sessions(&self) -> Vec<tunnel_core::localapi::RcSessionInfo> {
        self.rc_sessions
            .as_ref()
            .map(|r| r.list())
            .unwrap_or_default()
    }

    fn rc_disconnect(&self, session_id: &str) -> bool {
        // Same guard as `consent_decide`, and for the same reason: the id
        // reaches a lookup keyed by `ObjectId`, and refusing a malformed one up
        // front keeps the failure legible instead of an opaque "no such
        // session". The pipe SDDL already limits WHO may call this.
        if !is_hex_object_id(session_id) {
            tracing::warn!(
                session = %session_id,
                "localapi: rejecting rc disconnect — session id is not a 24-char hex ObjectId"
            );
            return false;
        }
        let Ok(oid) = bson::oid::ObjectId::parse_str(session_id) else {
            return false;
        };
        match &self.rc_sessions {
            Some(reg) => reg.disconnect(&oid),
            None => false,
        }
    }

    async fn ping(&self, target: &str, timeout_ms: u64, prefer_v6: bool) -> Response {
        let Some(pinger) = self.pinger.clone() else {
            // rc.204 — say what to do in BOTH non-netstack shapes: an OS-TUN
            // node pings peers with the system `ping` (the old message sent
            // operators chasing netstack when the overlay was simply off).
            return Response::Error {
                message: "roomler ping uses the userspace netstack, which this node isn't \
                          running. On an OS-TUN node, use the system `ping <overlay-ip>` \
                          instead (check `roomler status` / `roomler peers` for the IPs). If \
                          the overlay isn't up at all, set `overlay_enabled = true` in the \
                          node's config.toml and restart the daemon; for netstack mode, set \
                          ROOMLERD_OVERLAY_NETSTACK_SOCKS=<port>."
                    .into(),
            };
        };
        let Some(ip) = self.resolve_overlay(target, prefer_v6) else {
            return Response::Error {
                message: format!("no overlay peer named '{target}' — try an overlay IP or `peers`"),
            };
        };
        let timeout = Duration::from_millis(if timeout_ms == 0 { 3000 } else { timeout_ms });
        match pinger.ping(ip, timeout).await {
            Ok(rtt) => Response::Pong {
                target: target.to_string(),
                overlay_ip: ip.to_string(),
                rtt_micros: rtt.as_micros() as u64,
            },
            Err(message) => Response::Error { message },
        }
    }

    async fn create_forward(
        &self,
        node: &str,
        local: u16,
        remote: &str,
        transport: &str,
    ) -> Response {
        match self
            .tunnel_hub
            .create_forward(node, local, remote, transport)
            .await
        {
            Ok(id) => Response::FlowCreated { id },
            Err(message) => Response::Error { message },
        }
    }

    async fn create_socks5(&self, node: &str, local: u16, transport: &str) -> Response {
        match self.tunnel_hub.create_socks5(node, local, transport).await {
            Ok(id) => Response::FlowCreated { id },
            Err(message) => Response::Error { message },
        }
    }

    fn kill_flow(&self, id: &str) -> bool {
        self.tunnel_hub.kill_flow(id)
    }

    fn route_list(&self) -> Vec<tunnel_core::localapi::RouteInfo> {
        self.routes.as_ref().map(|r| r.list()).unwrap_or_default()
    }

    async fn route_add(&self, route: tunnel_core::localapi::RouteDescriptor) -> Response {
        let Some(routes) = self.routes.as_ref() else {
            return Response::Error {
                message: "declared routes are not available on this daemon".into(),
            };
        };
        match routes.add(route).await {
            Ok(route) => Response::RouteAdded { route },
            Err(message) => Response::Error { message },
        }
    }

    async fn set_device_name(&self, name: &str) -> Response {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return Response::Error {
                message: "device name is empty".into(),
            };
        }
        if trimmed.len() > 64 {
            return Response::Error {
                message: "device name is too long (max 64 characters)".into(),
            };
        }
        let Some((path, lock)) = self.config_persist.as_ref() else {
            return Response::Error {
                message: "renaming is not supported on this node".into(),
            };
        };
        // Same discipline as the route reconciler: hold the daemon-wide write
        // lock across load→mutate→save so a concurrent config writer (route
        // add, graceful shutdown) can't have its field dropped by our
        // full-struct save. Load FRESH — the boot-time snapshot may be stale.
        let _guard = lock.lock().await;
        let path = path.clone();
        let new_name = trimmed.to_string();
        let saved = tokio::task::spawn_blocking(move || {
            let mut cfg = crate::config::load(&path)
                .map_err(|e| format!("loading config for rename: {e:#}"))?;
            cfg.machine_name = new_name.clone();
            crate::config::save(&path, &cfg)
                .map_err(|e| format!("saving renamed config: {e:#}"))?;
            Ok::<String, String>(new_name)
        })
        .await
        .unwrap_or_else(|e| Err(format!("rename task join: {e}")));
        match saved {
            Ok(name) => {
                if let Ok(mut live) = self.name.lock() {
                    live.clone_from(&name);
                }
                tracing::info!(%name, "localapi: device renamed (announced on next reconnect)");
                Response::DeviceNameSet { name }
            }
            Err(message) => Response::Error { message },
        }
    }

    async fn route_remove(&self, id: &str) -> Response {
        let Some(routes) = self.routes.as_ref() else {
            return Response::Error {
                message: "declared routes are not available on this daemon".into(),
            };
        };
        match routes.remove(id).await {
            Ok(ok) => Response::RouteRemoved { ok },
            Err(message) => Response::Error { message },
        }
    }

    async fn route_set_enabled(&self, id: &str, enabled: bool) -> Response {
        let Some(routes) = self.routes.as_ref() else {
            return Response::Error {
                message: "declared routes are not available on this daemon".into(),
            };
        };
        match routes.set_enabled(id, enabled).await {
            Ok(ok) => Response::RouteUpdated { ok },
            Err(message) => Response::Error { message },
        }
    }

    /// FR-84 D1 — the atomic route edit; the reconciler's `replace` owns the
    /// validation and the one-write persistence.
    async fn route_update(&self, route: tunnel_core::localapi::RouteDescriptor) -> Response {
        let Some(routes) = self.routes.as_ref() else {
            return Response::Error {
                message: "declared routes are not available on this daemon".into(),
            };
        };
        match routes.replace(route).await {
            Ok(route) => Response::RouteReplaced { route },
            Err(message) => Response::Error { message },
        }
    }

    /// S2 — the editable config surface. Values come from a FRESH load of
    /// the daemon's own config file, so pending not-yet-restarted edits
    /// show (the boot-time snapshot would lie after a `ConfigSet`).
    async fn config_entries(&self) -> Response {
        let Some((path, lock)) = self.config_persist.as_ref() else {
            return Response::Error {
                message: "config editing is not available on this daemon".into(),
            };
        };
        let _guard = lock.lock().await;
        let path = path.clone();
        let loaded = tokio::task::spawn_blocking(move || {
            crate::config::load(&path).map_err(|e| format!("loading config: {e:#}"))
        })
        .await
        .unwrap_or_else(|e| Err(format!("config load task join: {e}")));
        match loaded {
            Ok(cfg) => Response::ConfigEntries(crate::config_surface::entries(&cfg)),
            Err(message) => Response::Error { message },
        }
    }

    /// S2 — set/clear one editable key. Same write discipline as
    /// [`set_device_name`](Self::set_device_name): daemon-wide write lock
    /// across load→validate→save so a concurrent config writer can't have
    /// its field dropped by our full-struct save. Validation is
    /// per-key in [`crate::config_surface::apply`]; a validation error
    /// leaves the file untouched.
    async fn config_set(&self, key: &str, value: Option<&str>) -> Response {
        let Some((path, lock)) = self.config_persist.as_ref() else {
            return Response::Error {
                message: "config editing is not available on this daemon".into(),
            };
        };
        let _guard = lock.lock().await;
        let path = path.clone();
        let key = key.to_string();
        let value = value.map(str::to_string);
        let saved = tokio::task::spawn_blocking(move || {
            let mut cfg =
                crate::config::load(&path).map_err(|e| format!("loading config: {e:#}"))?;
            crate::config_surface::apply(&mut cfg, &key, value.as_deref())?;
            crate::config::save(&path, &cfg).map_err(|e| format!("saving config: {e:#}"))?;
            let entry = crate::config_surface::entry_for(&cfg, &key)
                .ok_or_else(|| format!("unknown config key {key:?}"))?;
            Ok((entry, cfg))
        })
        .await
        .unwrap_or_else(|e| Err(format!("config set task join: {e}")));
        match saved {
            Ok((entry, cfg)) => {
                // Re-seed the LIVE gate-4 flags from what was just written,
                // still under the write lock. Everything else on this surface
                // genuinely is restart-required; `exec_enabled` and
                // `remote_config_enabled` are not, and leaving them stale here
                // would mean a SERVER push took effect faster than the owner's
                // own decision (docs/remote-config.md).
                if let Some(rc) = self.remote_config.as_ref() {
                    rc.adopt_local(&cfg);
                }
                // The surface says per key whether that re-seed made the
                // change live (FR-84 D2) — log the same truth the client
                // shows, never a blanket "on restart".
                tracing::info!(key = %entry.key, value = ?entry.value,
                    applies = if entry.restart_required { "restart" } else { "live" },
                    "localapi: config key updated");
                Response::ConfigUpdated { entry }
            }
            Err(message) => Response::Error { message },
        }
    }

    /// S2 — bounded log tail for the desktop's log viewer. The daemon
    /// resolves the source from ITS OWN perspective (role-correct dirs,
    /// SYSTEM-profile paths the desktop can't even read), caps the
    /// response, and the client polls `size` for follow.
    async fn tail_log(&self, source: &str, max_bytes: Option<u64>) -> Response {
        const CAP_MAX: u64 = 64 * 1024;
        const CAP_DEFAULT: u64 = 32 * 1024;
        let cap = max_bytes.unwrap_or(CAP_DEFAULT).clamp(512, CAP_MAX);
        let source = source.to_string();
        tokio::task::spawn_blocking(move || {
            let Some(path) = crate::logging::tail_source_path(&source) else {
                return Response::Error {
                    message: format!(
                        "no log file for source {source:?} (expected daemon | service | panic)"
                    ),
                };
            };
            match crate::logging::read_tail(&path, cap) {
                Ok((size, content)) => Response::LogTail {
                    path: path.display().to_string(),
                    size,
                    content,
                },
                Err(e) => Response::Error {
                    message: format!("reading {}: {e}", path.display()),
                },
            }
        })
        .await
        .unwrap_or_else(|e| Response::Error {
            message: format!("log tail task join: {e}"),
        })
    }

    #[cfg(feature = "recording")]
    async fn record_start(&self, opts: tunnel_core::localapi::RecordStartOpts) -> Response {
        match &self.recorder {
            Some(r) => r.start(opts).await,
            None => recording_unavailable(),
        }
    }

    #[cfg(feature = "recording")]
    async fn record_stop(&self) -> Response {
        match &self.recorder {
            Some(r) => r.stop().await,
            None => recording_unavailable(),
        }
    }

    #[cfg(feature = "recording")]
    async fn record_status(&self) -> Response {
        match &self.recorder {
            Some(r) => r.status(),
            None => Response::Recording(tunnel_core::localapi::RecordingState::unavailable(
                tunnel_core::localapi::NO_RECORDER,
            )),
        }
    }

    #[cfg(feature = "recording")]
    async fn recordings_list(&self) -> Response {
        match &self.recorder {
            Some(r) => r.list().await,
            None => recording_unavailable(),
        }
    }

    #[cfg(feature = "recording")]
    async fn recording_delete(&self, name: &str) -> Response {
        match &self.recorder {
            Some(r) => r.delete(name).await,
            None => recording_unavailable(),
        }
    }

    /// FR-84 D3 — "Apply now": restart this daemon through its supervisor.
    ///
    /// LocalAPI-only by construction: no server message reaches this, and
    /// remote configuration never restarts a daemon (docs/remote-config.md
    /// §7b). Refused unless a supervisor was PROVEN at startup, the device
    /// allows it (`local_restart_enabled`, read from the file for this
    /// request), nothing would be lost (a recording), and the last accepted
    /// restart — recorded on disk, so it binds the relaunched process too —
    /// is at least `RESTART_MIN_INTERVAL` old. Under systemd the unit's
    /// effective policy is read back before anything is promised.
    ///
    /// Accepting records the request durably FIRST (a restart that could not
    /// be recorded must not happen — the record is the loop bound), then arms
    /// the exit code and answers. The shutdown itself starts in
    /// [`Self::restart_commit`], after the answer is written.
    async fn restart_daemon(&self, reason: &str) -> Response {
        use crate::supervision as sup;
        let refuse = |message: String| Response::Error { message };
        let Some(restart) = self.restart.as_ref() else {
            return refuse("restarting is not available on this daemon".into());
        };
        let Some((config_path, lock)) = self.config_persist.as_ref() else {
            return refuse("restarting is not available on this daemon (no config)".into());
        };
        let reason = sup::sanitize_reason(reason);
        let inner = &restart.inner;
        let _one_at_a_time = inner.decide.lock().await;

        // The kill switch, from the FILE — the key is live, so an owner's
        // edit is in force for the very next request.
        let enabled = {
            let _guard = lock.lock().await;
            let path = config_path.clone();
            match tokio::task::spawn_blocking(move || crate::config::load(&path)).await {
                Ok(Ok(cfg)) => cfg.local_restart_enabled,
                Ok(Err(e)) => {
                    return refuse(format!(
                        "could not read local_restart_enabled from {}: {e:#}",
                        config_path.display()
                    ));
                }
                Err(e) => return refuse(format!("config read task: {e}")),
            }
        };
        let record_path = inner.record_path.clone();
        let last =
            match tokio::task::spawn_blocking(move || sup::read_last_request(&record_path)).await {
                Ok(Ok(last)) => last,
                Ok(Err(e)) => {
                    return refuse(format!(
                        "could not read the last restart's record ({e}) — refusing, because that \
                     record is what keeps a restart loop impossible"
                    ));
                }
                Err(e) => return refuse(format!("restart record task: {e}")),
            };
        #[cfg(feature = "recording")]
        let recording = crate::recording::manager::is_recording();
        #[cfg(not(feature = "recording"))]
        let recording = false;
        let now_ms = unix_now_ms();
        let inputs = sup::RestartInputs {
            supervision: inner.supervision,
            enabled,
            last_request_ms: last,
            now_ms,
            uptime: inner.started.elapsed(),
            recording,
            already_accepted: inner.accepted.get().is_some(),
        };
        let supervisor = inner.supervision.wire();
        let refused = |why: String| {
            tracing::info!(%reason, supervisor, refusal = %why, "restart requested through the LocalAPI: refused");
            refuse(why)
        };
        let plan = match sup::decide_restart(&inputs) {
            Ok(plan) => plan,
            Err(why) => return refused(why),
        };
        if let sup::Supervision::Systemd(unit) = inner.supervision
            && let Err(why) = sup::confirm_systemd(unit, plan.exit_code).await
        {
            return refused(why);
        }
        let record = sup::RestartRecord {
            at_ms: now_ms,
            pid: std::process::id(),
            supervisor: plan.supervisor.to_string(),
            reason: reason.clone(),
        };
        let record_path = inner.record_path.clone();
        match tokio::task::spawn_blocking(move || sup::write_record(&record_path, &record)).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                return refused(format!(
                    "could not record this restart ({e}) — refusing, because that record is \
                     what keeps a restart loop impossible"
                ));
            }
            Err(e) => return refused(format!("restart record task: {e}")),
        }
        // Armed. From here the process WILL restart: `restart_commit` starts
        // the shutdown once this answer is written, and the fallback below
        // covers a connection that never gets that far.
        let _ = inner.accepted.set(plan.exit_code);
        tracing::info!(
            %reason,
            supervisor,
            restart_by = plan.restart_by,
            exit_code = plan.exit_code,
            "restart requested through the LocalAPI: accepted — leaving once the answer is written"
        );
        let fallback = restart.clone();
        tokio::spawn(async move {
            tokio::time::sleep(RESTART_COMMIT_FALLBACK).await;
            fallback.commit();
        });
        Response::DaemonRestarting {
            supervisor: plan.supervisor.to_string(),
            restart_by: plan.restart_by.to_string(),
            exit_code: plan.exit_code,
            pid: std::process::id(),
            started_at_ms: self.started_at_ms,
        }
    }

    /// FR-84 D3 — the `DaemonRestarting` answer is written: start the
    /// graceful shutdown (no-op unless a restart was accepted).
    fn restart_commit(&self) {
        if let Some(restart) = self.restart.as_ref() {
            restart.commit();
        }
    }

    /// Fleet RPC — relay an exec to another device over this daemon's own
    /// agent WS and wait for the answer.
    ///
    /// The daemon is the right actor for three reasons: it already holds an
    /// authenticated server connection (so the CLI needs no credentials of its
    /// own), the server can resolve THIS device's owner as the acting
    /// principal, and the answer arrives asynchronously on the WS receive path
    /// where only the daemon can catch it.
    ///
    /// Note the trust boundary here is deliberately NOT just the pipe ACL,
    /// unlike every other mutating verb: being on this box is not by itself
    /// authority to run commands on a different one. The server applies all
    /// four gates and additionally requires this device to be blessed with
    /// `ExecPolicy::can_originate`.
    async fn exec_remote(
        &self,
        node: &str,
        shell: &str,
        command: &str,
        timeout_ms: u64,
    ) -> Response {
        let Some(sink) = self.tunnel_hub.sink_now() else {
            return Response::Error {
                message: "not connected to the server — remote commands need the control \
                          connection (check `roomler status`)"
                    .into(),
            };
        };

        let request_id = bson::oid::ObjectId::new().to_hex();
        // Register the waiter BEFORE sending, or a fast answer could arrive
        // with nowhere to go.
        let (_guard, rx) = crate::exec::expect_response(&request_id);

        if sink
            .send(roomler_ai_remote_control::ClientMsg::RpcExecRequest {
                request_id: request_id.clone(),
                target: node.to_string(),
                shell: shell.to_string(),
                command: command.to_string(),
                timeout_ms,
            })
            .await
            .is_err()
        {
            return Response::Error {
                message: "the server connection dropped while sending the command".into(),
            };
        }

        // Our own patience: the command's budget, the server's grace, plus
        // slack for the two extra WS hops this leg adds over the HTTP path.
        let budget = roomler_ai_remote_control::models::exec_limits::clamp_timeout_ms(timeout_ms);
        let deadline = Duration::from_millis(budget) + Duration::from_secs(30);
        let outcome = match tokio::time::timeout(deadline, rx).await {
            Ok(Ok(o)) => o,
            // The waiter was dropped without a value — only happens if the
            // signalling loop tore down mid-flight.
            Ok(Err(_)) => crate::exec::ExecOutcome {
                error: Some("the server connection dropped while awaiting the result".into()),
                ..Default::default()
            },
            Err(_) => crate::exec::ExecOutcome {
                error: Some(format!(
                    "no answer from {node} within {}s",
                    deadline.as_secs()
                )),
                ..Default::default()
            },
        };

        Response::ExecResult {
            request_id,
            node: node.to_string(),
            exit_code: outcome.exit_code,
            stdout: outcome.stdout,
            stderr: outcome.stderr,
            truncated: outcome.truncated,
            duration_ms: outcome.duration_ms,
            error: outcome.error,
        }
    }

    /// Roomler SSH (P6b) — relay a grant request over this device's control
    /// WS and hand the answer back to the local caller.
    ///
    /// The public key is the CALLER's; the daemon never sees the private half,
    /// so a grant observed anywhere along this path is useless on its own.
    async fn ssh_session(&self, node: &str, public_key: &str, session_secs: u64) -> Response {
        let Some(sink) = self.tunnel_hub.sink_now() else {
            return Response::Error {
                message: "not connected to the server — SSH grants need the control \
                          connection (check `roomler status`)"
                    .into(),
            };
        };

        let request_id = bson::oid::ObjectId::new().to_hex();
        // Register the waiter BEFORE sending, or a fast answer could arrive
        // with nowhere to go.
        let (_guard, rx) = crate::ssh_origin::expect_response(&request_id);

        if sink
            .send(roomler_ai_remote_control::ClientMsg::SshRequest {
                request_id: request_id.clone(),
                target: node.to_string(),
                public_key: public_key.to_string(),
                session_secs,
            })
            .await
            .is_err()
        {
            return Response::Error {
                message: "the server connection dropped while requesting the session".into(),
            };
        }

        // Short on purpose: this is a gate decision plus one push to the
        // target, not a command that runs. A grant also expires in ~60 s, so
        // waiting minutes for one would hand back something already dead.
        let deadline = std::time::Duration::from_secs(30);
        let answer = match tokio::time::timeout(deadline, rx).await {
            Ok(Ok(a)) => a,
            Ok(Err(_)) => crate::ssh_origin::SshGrantAnswer {
                error: Some("the server connection dropped while awaiting the grant".into()),
                ..Default::default()
            },
            Err(_) => crate::ssh_origin::SshGrantAnswer {
                error: Some(format!(
                    "no answer about {node} within {}s",
                    deadline.as_secs()
                )),
                ..Default::default()
            },
        };

        Response::SshSession {
            request_id,
            node: node.to_string(),
            address: answer.address,
            port: answer.port,
            host_pubkey: answer.host_pubkey,
            grant_id: answer.grant_id,
            expires_at_ms: answer.expires_at_ms,
            error: answer.error,
        }
    }

    /// FR-84 D5b — the devices this device may see, asked of the org's
    /// server with the org's agent token (`self_view.rs`). Read-only; the
    /// token never crosses the pipe.
    async fn devices(&self, org: &str, query: &tunnel_core::localapi::DevicesQuery) -> Response {
        match &self.self_view {
            Some(view) => view.devices(org, query).await,
            None => Response::Upstream {
                code: "unsupported".into(),
                message: "this daemon was started without a server identity to ask".into(),
                status: None,
            },
        }
    }

    /// FR-84 D5b — the mesh graph for the same set.
    async fn mesh(&self, org: &str) -> Response {
        match &self.self_view {
            Some(view) => view.mesh(org).await,
            None => Response::Upstream {
                code: "unsupported".into(),
                message: "this daemon was started without a server identity to ask".into(),
                status: None,
            },
        }
    }

    /// S1b — archive the STALE config copy on a split-config host (the
    /// desktop's "Two configurations found" banner finally gets a button).
    /// The daemon is the only safe actor: it knows which copy it LOADED and
    /// has the rights to touch `%PROGRAMDATA%`. Guards:
    ///   * a second copy must actually exist,
    ///   * the daemon must be connected (proves the live copy's token works),
    ///   * both copies must parse and carry the SAME `agent_id` (a different
    ///     id = a second enrollment — refused, an operator must decide),
    ///   * the stale copy is renamed aside (`config.toml.stale-<ts>`), never
    ///     deleted.
    async fn config_cleanup_stale(&self) -> Response {
        let Some((live_path, lock)) = self.config_persist.as_ref() else {
            return Response::Error {
                message: "config cleanup is not available on this daemon".into(),
            };
        };
        // Candidate copies: the per-user default + (Windows) machine-global.
        let mut candidates: Vec<std::path::PathBuf> = Vec::new();
        if let Ok(p) = crate::config::default_config_path() {
            candidates.push(p);
        }
        #[cfg(target_os = "windows")]
        candidates.push(crate::config::machine_global_config_path());
        let stale: Vec<_> = candidates
            .into_iter()
            .filter(|p| p != live_path && p.exists())
            .collect();
        let Some(stale_path) = stale.first().cloned() else {
            return Response::ConfigCleaned {
                ok: false,
                detail: "no stale config copy present".into(),
            };
        };
        if !self.connected.load(Ordering::Relaxed) {
            return Response::Error {
                message: "refusing cleanup while disconnected — the live config's \
                          token isn't proven working"
                    .into(),
            };
        }
        let _guard = lock.lock().await;
        let live = match crate::config::load(live_path) {
            Ok(c) => c,
            Err(e) => {
                return Response::Error {
                    message: format!("live config unreadable: {e:#}"),
                };
            }
        };
        let stale_cfg = match crate::config::load(&stale_path) {
            Ok(c) => c,
            Err(e) => {
                return Response::Error {
                    message: format!("stale config unreadable ({}): {e:#}", stale_path.display()),
                };
            }
        };
        if live.agent_id.is_empty() || live.agent_id != stale_cfg.agent_id {
            return Response::Error {
                message: format!(
                    "refusing cleanup: the copies carry different identities \
                     (live agent_id {} vs {} in {}) — that's a second enrollment, \
                     not a stale duplicate",
                    live.agent_id,
                    stale_cfg.agent_id,
                    stale_path.display()
                ),
            };
        }
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let archived = stale_path.with_extension(format!("toml.stale-{ts}"));
        match std::fs::rename(&stale_path, &archived) {
            Ok(()) => {
                tracing::info!(
                    stale = %stale_path.display(),
                    archived = %archived.display(),
                    "localapi: archived stale config copy"
                );
                Response::ConfigCleaned {
                    ok: true,
                    detail: format!(
                        "archived {} -> {}",
                        stale_path.display(),
                        archived.display()
                    ),
                }
            }
            Err(e) => Response::Error {
                message: format!("archiving {} failed: {e}", stale_path.display()),
            },
        }
    }
}

/// Overlay [`ConnectionType::Tunnel`] onto every peer whose WG carrier is down
/// (`Blocked`/`Offline`) but whose backing `agent_id` is in `tunneled` — the set
/// of agent ids reached by a live daemon tunnel flow (P3b-3). Pure so the
/// precedence (Direct > Relay > **Tunnel** > Blocked > Offline — Tunnel never
/// masks a live WG carrier) is unit-tested without a hub. No-op on empty
/// `tunneled`.
fn apply_tunnel_override(peers: &mut [PeerInfo], tunneled: &HashSet<String>) {
    if tunneled.is_empty() {
        return;
    }
    for p in peers.iter_mut() {
        let gap = matches!(
            p.connection,
            ConnectionType::Blocked | ConnectionType::Offline
        );
        if gap && p.agent_id.as_deref().is_some_and(|a| tunneled.contains(a)) {
            p.connection = ConnectionType::Tunnel;
        }
    }
}

/// Spawn the RTT prober (P3b-3): every [`RTT_PROBE_INTERVAL`], ICMP-ping each
/// carrier-reachable peer over the userspace netstack and cache the round-trip
/// so `peers()` can surface `rtt_ms`. Only meaningful on a netstack node (the
/// caller spawns it only when a `pinger` exists); the cache stays empty
/// otherwise and every peer's `rtt_ms` is `None`.
///
/// Probes **only** `Direct`/`Relay` peers — a `Blocked`/`Offline` peer has no
/// working carrier, so a ping just burns the full timeout and would stretch the
/// sequential cycle. Pings sequentially so a burst of ICMP never hits the wire
/// at once; the worst-case cycle is (live-peer-count × [`RTT_PROBE_TIMEOUT`]),
/// comfortably under the interval for realistic meshes. Exits on `shutdown`.
pub fn spawn_rtt_prober(
    pinger: Arc<dyn NetstackPinger>,
    overlay: watch::Receiver<OverlayView>,
    cache: RttCache,
    mut shutdown: watch::Receiver<bool>,
    on_sample: Option<RttSampleHook>,
) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(RTT_PROBE_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                _ = shutdown.changed() => {
                    if *shutdown.borrow() { return; }
                }
                _ = tick.tick() => {}
            }
            // Snapshot the peers to probe, then release the watch borrow before
            // any await (the borrow is not held across the ping).
            let targets: Vec<(String, String, IpAddr)> = overlay
                .borrow()
                .peers
                .iter()
                .filter(|p| matches!(p.connection, ConnectionType::Direct | ConnectionType::Relay))
                .filter_map(|p| {
                    let ip = p.overlay_ip.as_deref()?;
                    ip.parse::<IpAddr>()
                        .ok()
                        .map(|addr| (p.node_id.clone(), ip.to_string(), addr))
                })
                .collect();
            for (node_id, key, ip) in targets {
                if let Ok(rtt) = pinger.ping(ip, RTT_PROBE_TIMEOUT).await {
                    let ms = rtt.as_millis().min(u32::MAX as u128) as u32;
                    if let Ok(mut c) = cache.lock() {
                        c.insert(key, (ms, Instant::now()));
                    }
                    // B1 — hand the sample to the overlay runtime's quality
                    // plane (a timeout deliberately hands over NOTHING —
                    // loss is the death path's business).
                    if let Some(hook) = &on_sample {
                        hook(&node_id, ms);
                    }
                }
            }
        }
    });
}

/// B1 — optional per-sample prober callback: `(node_id_hex, rtt_ms)` for
/// every SUCCESSFUL probe. Feature-agnostic — the overlay-enabled caller
/// (main.rs) bridges it into the runtime's `OverlayEvent` channel via a
/// weak sender, so this module never grows an overlay-feature dependency.
pub type RttSampleHook = Arc<dyn Fn(&str, u32) + Send + Sync>;

/// A 24-char hex ObjectId — the only shape a session id may take before it's
/// used as a sentinel filename. Guards [`DaemonState::consent_decide`] against a
/// caller smuggling path separators / traversal into the filename.
fn is_hex_object_id(s: &str) -> bool {
    s.len() == 24 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;
    use tunnel_core::localapi::ConnectionType;

    /// P8-cosmetics — the locale-tolerant ping-output parse: English, German,
    /// sub-millisecond, and Linux fractional forms all yield the right integer;
    /// output without a real measurement yields None.
    #[test]
    fn parse_ping_ms_is_locale_tolerant() {
        assert_eq!(
            parse_ping_ms("Reply from 100.64.0.1: bytes=32 time=4ms TTL=128"),
            Some(4)
        );
        assert_eq!(
            parse_ping_ms("Antwort von 100.64.0.1: Bytes=32 Zeit=101ms TTL=128"),
            Some(101)
        );
        assert_eq!(
            parse_ping_ms("Reply from 100.64.0.1: bytes=32 time<1ms TTL=128"),
            Some(1)
        );
        assert_eq!(
            parse_ping_ms("64 bytes from 100.64.0.14: icmp_seq=1 ttl=64 time=0.523 ms"),
            Some(0)
        );
        assert_eq!(parse_ping_ms("Request timed out."), None);
        assert_eq!(parse_ping_ms("streams and other ms-free words"), None);
    }

    fn view() -> OverlayView {
        OverlayView {
            self_ip: Some("100.64.0.2".into()),
            self_ip6: Some("fd72:6f6f:6d6c::6440:2".into()),
            peers: vec![PeerInfo {
                node_id: "n2".into(),
                name: "peer".into(),
                org: String::new(),
                overlay_ip: Some("100.64.0.1".into()),
                overlay_ip6: Some("fd72:6f6f:6d6c::6440:1".into()),
                online: true,
                connection: ConnectionType::Relay,
                upgrading: false,
                stalled: false,
                rtt_ms: None,
                last_seen_ms: None,
                agent_id: None,
                relay_local: Some("94.130.141.74:10850".into()),
                relay_dst: Some("5.9.157.226:12728".into()),
                relay_kind: None,
                relay_transport: None,
                relay_server: None,
                why: None,
                probes: Vec::new(),
                debug: None,
            }],
            exit_node: None,
            dns: None,
            derp_inbound_drops: None,
            srflx: None,
            warm_relay: None,
            direct_socks: Vec::new(),
        }
    }

    fn consent_broker(tag: &str) -> crate::consent::ConsentBroker {
        crate::consent::ConsentBroker::new(
            crate::consent::Mode::AutoGrant,
            std::env::temp_dir().join(format!("roomler-las-consent-{tag}-{}", std::process::id())),
        )
        .unwrap()
    }

    fn empty_rtt_cache() -> RttCache {
        Arc::new(Mutex::new(HashMap::new()))
    }

    #[tokio::test]
    async fn resolve_targets_and_ping_without_pinger_errors() {
        let (_tx, rx) = watch::channel(view());
        let st = DaemonState::new(
            "aid".into(),
            "host".into(),
            DaemonMode::Service,
            None,
            Arc::new(AtomicBool::new(true)),
            rx,
            consent_broker("ping"),
            None, // no netstack pinger
            crate::tunnel::client_mgr::TunnelClientHub::new("test".into()),
            empty_rtt_cache(),
        );
        // Resolve by peer name (from `view`), by first label, and by literal IP.
        assert_eq!(
            st.resolve_overlay("peer", false),
            Some(IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1)))
        );
        assert_eq!(
            st.resolve_overlay("PEER.myorg.roomler.net", false),
            Some(IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1)))
        );
        assert_eq!(
            st.resolve_overlay("100.64.0.9", false),
            Some(IpAddr::V4(Ipv4Addr::new(100, 64, 0, 9)))
        );
        // prefer_v6 picks the runtime-published derived v6 for a NAME target…
        assert_eq!(
            st.resolve_overlay("peer", true),
            Some("fd72:6f6f:6d6c::6440:1".parse().unwrap())
        );
        // …and a literal v6 target is accepted as-is.
        assert_eq!(
            st.resolve_overlay("fd72:6f6f:6d6c::6440:9", false),
            Some("fd72:6f6f:6d6c::6440:9".parse().unwrap())
        );
        assert_eq!(st.resolve_overlay("ghost", false), None);
        // With no pinger (not a netstack node) `ping` is a clean Error, not a Pong.
        assert!(matches!(
            st.ping("peer", 0, false).await,
            Response::Error { .. }
        ));
    }

    #[test]
    fn consent_pending_scan_is_bounded() {
        let (_tx, rx) = watch::channel(view());
        let broker = consent_broker("cap");
        let dir = broker.sentinel_dir().to_path_buf();
        // Stuff the dir well past the cap with well-formed pending sentinels
        // (only the daemon writes here in production — this simulates a
        // corrupted / adversarially stuffed directory).
        for i in 0..80u32 {
            let req = tunnel_core::localapi::ConsentRequest {
                session_id: format!("{i:024x}"),
                controller_name: "x".into(),
                permissions: "VIEW_SCREEN".into(),
                timeout_secs: 30,
                kind: "rc".into(),
                detail: String::new(),
                expires_at_ms: 0,
                surface: String::new(),
                org: String::new(),
            };
            std::fs::write(
                dir.join(format!("{i:024x}.pending")),
                serde_json::to_string(&req).unwrap(),
            )
            .unwrap();
        }
        let st = DaemonState::new(
            "aid".into(),
            "host".into(),
            DaemonMode::Service,
            None,
            Arc::new(AtomicBool::new(true)),
            rx,
            broker,
            None,
            crate::tunnel::client_mgr::TunnelClientHub::new("test".into()),
            empty_rtt_cache(),
        );
        let pending = st.consent_pending();
        assert!(
            pending.len() <= 64,
            "consent_pending scan must be capped at 64, got {}",
            pending.len()
        );
        assert!(
            pending.len() >= 60,
            "the cap should still return a full page, got {}",
            pending.len()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn set_device_name_persists_and_updates_status() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut cfg = crate::config::test_fixture();
        cfg.machine_name = "old-name".into();
        crate::config::save(&path, &cfg).unwrap();

        let (_tx, rx) = watch::channel(view());
        let st = DaemonState::new(
            "aid".into(),
            "old-name".into(),
            DaemonMode::Service,
            None,
            Arc::new(AtomicBool::new(true)),
            rx,
            consent_broker("rename"),
            None,
            crate::tunnel::client_mgr::TunnelClientHub::new("test".into()),
            empty_rtt_cache(),
        )
        .with_config_persist(path.clone(), Arc::new(tokio::sync::Mutex::new(())));

        // Rejections: empty + oversized names never touch the config.
        assert!(matches!(
            st.set_device_name("   ").await,
            Response::Error { .. }
        ));
        assert!(matches!(
            st.set_device_name(&"x".repeat(80)).await,
            Response::Error { .. }
        ));
        assert_eq!(crate::config::load(&path).unwrap().machine_name, "old-name");

        // Happy path: trimmed, persisted, and live in status() immediately.
        match st.set_device_name("  new-name  ").await {
            Response::DeviceNameSet { name } => assert_eq!(name, "new-name"),
            other => panic!("expected DeviceNameSet, got {other:?}"),
        }
        assert_eq!(st.status().name, "new-name");
        assert_eq!(crate::config::load(&path).unwrap().machine_name, "new-name");

        // Without a persist target the verb is a clean unsupported error.
        let (_tx2, rx2) = watch::channel(view());
        let bare = DaemonState::new(
            "aid".into(),
            "n".into(),
            DaemonMode::Service,
            None,
            Arc::new(AtomicBool::new(true)),
            rx2,
            consent_broker("rename2"),
            None,
            crate::tunnel::client_mgr::TunnelClientHub::new("test".into()),
            empty_rtt_cache(),
        );
        assert!(matches!(
            bare.set_device_name("x").await,
            Response::Error { .. }
        ));
    }

    /// FR-84 D5b — the verbs reach the self-view only when a registry was
    /// attached; without one the answer is the named `unsupported`, never a
    /// bare error a client could mistake for "no devices".
    #[tokio::test]
    async fn devices_and_mesh_route_through_the_attached_registry() {
        fn state(tag: &str) -> DaemonState {
            let (_tx, rx) = watch::channel(view());
            DaemonState::new(
                "aid".into(),
                "host".into(),
                DaemonMode::Service,
                None,
                Arc::new(AtomicBool::new(true)),
                rx,
                consent_broker(tag),
                None,
                crate::tunnel::client_mgr::TunnelClientHub::new("test".into()),
                empty_rtt_cache(),
            )
        }
        let q = tunnel_core::localapi::DevicesQuery::default();

        let bare = state("dv-bare");
        assert!(matches!(
            bare.devices("", &q).await,
            Response::Upstream { ref code, .. } if code == "unsupported"
        ));
        assert!(matches!(
            bare.mesh("").await,
            Response::Upstream { ref code, .. } if code == "unsupported"
        ));

        // With a registry, the org is resolved BEFORE any request: an unknown
        // label is named as such (no server is contacted — none exists here).
        let registry: crate::self_view::OrgHttpRegistry =
            Arc::new(Mutex::new(vec![crate::self_view::OrgHttp {
                label: "primary".into(),
                server_url: "https://example.invalid".into(),
                agent_token: "tok".into(),
                primary: true,
                enabled: true,
            }]));
        let wired = state("dv-wired").with_org_http(registry);
        assert!(matches!(
            wired.devices("ghost", &q).await,
            Response::Upstream { ref code, .. } if code == "unknown_org"
        ));
    }

    #[test]
    fn consent_decide_hex_guard_rejects_unsafe_session_ids() {
        // The guard fires BEFORE any filesystem write, so a bad id is a pure
        // no-op — traversal / separators / wrong-length are all rejected.
        assert!(is_hex_object_id("0123456789abcdef01234567"));
        assert!(is_hex_object_id("6A11682E804368D30EDF57C6")); // upper-case hex ok
        assert!(!is_hex_object_id("6a11682e804368d30edf57c")); // 23 chars
        assert!(!is_hex_object_id("6a11682e804368d30edf57c6z")); // 25 / non-hex
        assert!(!is_hex_object_id("../../etc/passwd"));
        assert!(!is_hex_object_id("6a11682e804368d30edf57c6.approve"));
        assert!(!is_hex_object_id(""));
    }

    #[test]
    fn status_and_peers_track_connected_flag() {
        let connected = Arc::new(AtomicBool::new(false));
        let (_tx, rx) = watch::channel(view());
        let consent = crate::consent::ConsentBroker::new(
            crate::consent::Mode::AutoGrant,
            std::env::temp_dir().join(format!("roomler-las-consent-{}", std::process::id())),
        )
        .unwrap();
        let st = DaemonState::new(
            "aid".into(),
            "host".into(),
            DaemonMode::Service,
            Some("tid".into()),
            connected.clone(),
            rx,
            consent,
            None,
            crate::tunnel::client_mgr::TunnelClientHub::new("test".into()),
            empty_rtt_cache(),
        );

        // Identity + overlay IP are always reported; connected reflects the flag.
        let s = st.status();
        assert_eq!(s.node_id, "aid");
        assert_eq!(s.name, "host");
        assert_eq!(s.tenant_id.as_deref(), Some("tid"));
        assert_eq!(s.overlay_ip.as_deref(), Some("100.64.0.2"));
        assert!(!s.connected);

        // Peers hidden while disconnected…
        assert!(st.peers().is_empty(), "no peers reported while WS is down");

        // …shown once connected.
        connected.store(true, Ordering::Relaxed);
        assert!(st.status().connected);
        let peers = st.peers();
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].connection, ConnectionType::Relay);

        // Flows are empty on the agent side in P1.
        assert!(st.flows().is_empty());
    }

    #[test]
    fn peers_fill_rtt_from_fresh_cache_and_drop_stale() {
        // The `view()` peer is a Relay with overlay_ip 100.64.0.1.
        let (_tx, rx) = watch::channel(view());
        let cache = empty_rtt_cache();
        let st = DaemonState::new(
            "aid".into(),
            "host".into(),
            DaemonMode::Service,
            None,
            Arc::new(AtomicBool::new(true)),
            rx,
            consent_broker("rtt"),
            None,
            crate::tunnel::client_mgr::TunnelClientHub::new("test".into()),
            cache.clone(),
        );

        // Fresh cache entry → surfaced as rtt_ms.
        cache
            .lock()
            .unwrap()
            .insert("100.64.0.1".into(), (52, Instant::now()));
        assert_eq!(st.peers()[0].rtt_ms, Some(52));

        // Stale entry (older than RTT_STALE) → dropped to None (fade to "—").
        let stale_at = Instant::now()
            .checked_sub(RTT_STALE + Duration::from_secs(1))
            .unwrap();
        cache
            .lock()
            .unwrap()
            .insert("100.64.0.1".into(), (52, stale_at));
        assert_eq!(st.peers()[0].rtt_ms, None);

        // Empty cache → None.
        cache.lock().unwrap().clear();
        assert_eq!(st.peers()[0].rtt_ms, None);
    }

    #[test]
    fn tunnel_override_only_fills_carrier_gaps_and_respects_precedence() {
        fn peer(agent: Option<&str>, conn: ConnectionType) -> PeerInfo {
            PeerInfo {
                node_id: "n".into(),
                name: "p".into(),
                org: String::new(),
                overlay_ip: None,
                overlay_ip6: None,
                online: true,
                connection: conn,
                upgrading: false,
                stalled: false,
                rtt_ms: None,
                last_seen_ms: None,
                agent_id: agent.map(|s| s.into()),
                relay_local: None,
                relay_dst: None,
                relay_kind: None,
                relay_transport: None,
                relay_server: None,
                why: None,
                probes: Vec::new(),
                debug: None,
            }
        }
        let tunneled: HashSet<String> = ["aid-1".to_string()].into_iter().collect();
        let mut peers = vec![
            peer(Some("aid-1"), ConnectionType::Blocked), // → Tunnel (gap + live flow)
            peer(Some("aid-1"), ConnectionType::Offline), // → Tunnel (gap + live flow)
            peer(Some("aid-1"), ConnectionType::Direct),  // stays Direct (carrier wins)
            peer(Some("aid-1"), ConnectionType::Relay),   // stays Relay (carrier wins)
            peer(Some("aid-2"), ConnectionType::Blocked), // stays Blocked (no flow)
            peer(None, ConnectionType::Blocked),          // stays Blocked (no agent_id)
        ];
        apply_tunnel_override(&mut peers, &tunneled);
        assert_eq!(peers[0].connection, ConnectionType::Tunnel);
        assert_eq!(peers[1].connection, ConnectionType::Tunnel);
        assert_eq!(peers[2].connection, ConnectionType::Direct);
        assert_eq!(peers[3].connection, ConnectionType::Relay);
        assert_eq!(peers[4].connection, ConnectionType::Blocked);
        assert_eq!(peers[5].connection, ConnectionType::Blocked);

        // Empty tunneled set → untouched.
        let mut p2 = vec![peer(Some("aid-1"), ConnectionType::Blocked)];
        apply_tunnel_override(&mut p2, &HashSet::new());
        assert_eq!(p2[0].connection, ConnectionType::Blocked);
    }

    // ── FR-84 D3: the restart verb, end to end on the daemon side ──────────

    use crate::supervision::{RESTART_RECORD_FILE, Supervision, SystemdUnit};

    /// A fresh config directory holding a real `config.toml` (the verb reads
    /// `local_restart_enabled` from the FILE), unique per test.
    fn restart_dir(tag: &str, enabled: bool) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("roomler-las-restart-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut cfg = crate::config::test_fixture();
        cfg.local_restart_enabled = enabled;
        crate::config::save(&dir.join("config.toml"), &cfg).unwrap();
        dir
    }

    /// A `DaemonState` wired the way `run_cmd` wires it for the verb — as a
    /// freshly started process would be (its own handle, its own shutdown
    /// channel), over the config in `dir`.
    fn restart_state(
        dir: &std::path::Path,
        supervision: Supervision,
    ) -> (DaemonState, RestartHandle, watch::Receiver<bool>) {
        let (_ov_tx, ov_rx) = watch::channel(view());
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let config = dir.join("config.toml");
        let restart = RestartHandle::new(
            supervision,
            shutdown_tx,
            // Just started — which only FR-43's worker minds, and it has its
            // own test (`a_young_macos_worker_waits_out_the_fr43_threshold`).
            // Not `now - 1h`: that panics on a runner booted under an hour ago.
            Instant::now(),
            crate::supervision::restart_record_path(&config),
        );
        let st = DaemonState::new(
            "aid".into(),
            "host".into(),
            DaemonMode::Service,
            None,
            Arc::new(AtomicBool::new(true)),
            ov_rx,
            consent_broker("restart"),
            None,
            crate::tunnel::client_mgr::TunnelClientHub::new("test".into()),
            empty_rtt_cache(),
        )
        .with_config_persist(config, Arc::new(tokio::sync::Mutex::new(())))
        .with_restart(restart.clone());
        (st, restart, shutdown_rx)
    }

    fn refusal(resp: Response) -> String {
        match resp {
            Response::Error { message } => message,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    /// The orphan `roomlerd run`: refused, nothing recorded, and even an
    /// explicit commit starts no shutdown — nothing was accepted.
    #[tokio::test]
    async fn restart_refused_when_unsupervised() {
        let dir = restart_dir("orphan", true);
        let (st, restart, shutdown) = restart_state(&dir, Supervision::None);
        let why = refusal(st.restart_daemon("apply").await);
        assert!(why.contains("not running under a service manager"), "{why}");
        assert!(
            !dir.join(RESTART_RECORD_FILE).exists(),
            "a refusal records nothing"
        );
        st.restart_commit();
        assert!(
            !*shutdown.borrow(),
            "a refused restart never shuts the daemon down"
        );
        assert_eq!(restart.accepted_exit_code(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `local_restart_enabled = false` in the file refuses — and the key is
    /// LIVE: turning it back on in the file is in force for the next request,
    /// no restart needed (which would be circular).
    #[tokio::test]
    async fn restart_refused_when_disabled_and_the_switch_is_live() {
        let dir = restart_dir("disabled", false);
        let (st, _restart, shutdown) = restart_state(&dir, Supervision::WindowsScm);
        let why = refusal(st.restart_daemon("apply").await);
        assert!(why.contains("local_restart_enabled = false"), "{why}");
        assert!(!*shutdown.borrow());
        // Same process, the file flipped back on through the LocalAPI.
        assert!(matches!(
            st.config_set("local_restart_enabled", Some("true")).await,
            Response::ConfigUpdated { .. }
        ));
        assert!(matches!(
            st.restart_daemon("apply").await,
            Response::DaemonRestarting { .. }
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Accepted: the answer names the supervisor's own exit code, the request
    /// is on disk BEFORE the answer, and the shutdown starts only at the
    /// commit — the LocalAPI loop's cue that the answer is out. A second ask
    /// in the same process is refused.
    #[tokio::test]
    async fn restart_accepted_records_first_and_shuts_down_only_on_commit() {
        let dir = restart_dir("accept", true);
        let (st, restart, shutdown) = restart_state(&dir, Supervision::WindowsScm);
        match st.restart_daemon("settings: overlay_enabled").await {
            Response::DaemonRestarting {
                supervisor,
                restart_by,
                exit_code,
                pid,
                started_at_ms,
            } => {
                assert_eq!(supervisor, "scm");
                assert_eq!(restart_by, "supervisor");
                assert_eq!(exit_code, 0, "decide_exit_reaction(0) is Respawn");
                // The same process identity `status` reports, so a waiting
                // caller can recognise the one that is leaving.
                let status = st.status();
                assert_eq!(Some(pid), status.pid);
                assert_eq!(Some(started_at_ms), status.started_at_ms);
            }
            other => panic!("expected DaemonRestarting, got {other:?}"),
        }
        let record = std::fs::read_to_string(dir.join(RESTART_RECORD_FILE)).unwrap();
        assert!(record.contains("settings: overlay_enabled"), "{record}");
        assert!(!*shutdown.borrow(), "armed, not yet committed");
        assert_eq!(restart.accepted_exit_code(), Some(0));
        st.restart_commit();
        assert!(
            *shutdown.borrow(),
            "the commit starts the internal shutdown"
        );
        let why = refusal(st.restart_daemon("again").await);
        assert!(why.contains("already under way"), "{why}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The loop bound survives the restart it bounds: the relaunched process
    /// (a new handle, nothing in memory) reads the record and refuses a
    /// second restart inside the window.
    #[tokio::test]
    async fn restart_rate_limit_binds_the_relaunched_process() {
        let dir = restart_dir("ratelimit", true);
        let (st, _restart, _shutdown) = restart_state(&dir, Supervision::WindowsTask);
        match st.restart_daemon("first").await {
            Response::DaemonRestarting {
                restart_by,
                exit_code,
                ..
            } => {
                assert_eq!(restart_by, "caller", "the task's restart is the caller's");
                assert_eq!(exit_code, crate::watchdog::RESTART_REQUESTED_EXIT_CODE);
            }
            other => panic!("expected DaemonRestarting, got {other:?}"),
        }
        drop(st);
        let (next, next_restart, next_shutdown) = restart_state(&dir, Supervision::WindowsTask);
        let why = refusal(next.restart_daemon("second").await);
        assert!(
            why.contains("s ago") && why.contains("try again in"),
            "{why}"
        );
        assert!(!*next_shutdown.borrow());
        assert_eq!(next_restart.accepted_exit_code(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Under systemd nothing is promised until systemd itself confirms it
    /// will restart THIS process. A test process is never a unit's main
    /// process (and on a host without systemd there is nobody to ask), so
    /// the answer here is always a refusal — the fail-closed direction.
    #[tokio::test]
    async fn systemd_restart_is_refused_unless_systemd_confirms() {
        let dir = restart_dir("systemd", true);
        let unit = SystemdUnit {
            name: "roomlerd.service",
            user: false,
        };
        let (st, restart, shutdown) = restart_state(&dir, Supervision::Systemd(unit));
        let why = refusal(st.restart_daemon("apply").await);
        assert!(why.contains("roomlerd.service"), "{why}");
        assert!(!dir.join(RESTART_RECORD_FILE).exists());
        assert!(!*shutdown.borrow());
        assert_eq!(restart.accepted_exit_code(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A daemon built without the handle (older shapes, other tests) says it
    /// cannot restart, and `status` names the answering process.
    #[tokio::test]
    async fn restart_without_a_handle_is_unavailable_and_status_names_the_pid() {
        let (_tx, rx) = watch::channel(view());
        let st = DaemonState::new(
            "aid".into(),
            "host".into(),
            DaemonMode::Service,
            None,
            Arc::new(AtomicBool::new(true)),
            rx,
            consent_broker("nohandle"),
            None,
            crate::tunnel::client_mgr::TunnelClientHub::new("test".into()),
            empty_rtt_cache(),
        );
        assert!(refusal(st.restart_daemon("x").await).contains("not available"));
        st.restart_commit(); // no handle: a no-op, not a panic
        let status = st.status();
        assert_eq!(status.pid, Some(std::process::id()));
        assert!(status.started_at_ms.is_some_and(|t| t > 0));
    }
}
