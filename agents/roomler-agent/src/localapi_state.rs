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
}

/// Shared registry of [`OrgRuntime`] rows (primary first).
pub type OrgStatusRegistry = Arc<Mutex<Vec<OrgRuntime>>>;

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
            pinger,
            tunnel_hub,
            rtt_cache,
            routes: None,
            config_persist: None,
            orgs: None,
        }
    }

    /// Attach the multi-org status registry so `NodeStatus.orgs` is live.
    /// Separate from `new` to keep the constructor's existing call sites
    /// (incl. tests) unchanged.
    pub fn with_orgs(mut self, orgs: OrgStatusRegistry) -> Self {
        self.orgs = Some(orgs);
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
                                })
                                .collect()
                        })
                        .unwrap_or_default()
                })
                .unwrap_or_default(),
        }
    }

    fn peers(&self) -> Vec<PeerInfo> {
        // A peer list from a dropped connection is misleading — the carriers are
        // gone. Report none until reconnected + re-synced.
        if !self.connected.load(Ordering::Relaxed) {
            return Vec::new();
        }
        let mut peers = self.overlay.borrow().peers.clone();
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
                          ROOMLER_AGENT_OVERLAY_NETSTACK_SOCKS=<port>."
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
            crate::config_surface::entry_for(&cfg, &key)
                .ok_or_else(|| format!("unknown config key {key:?}"))
        })
        .await
        .unwrap_or_else(|e| Err(format!("config set task join: {e}")));
        match saved {
            Ok(entry) => {
                tracing::info!(key = %entry.key, value = ?entry.value,
                    "localapi: config key updated (takes effect on restart)");
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
                debug: None,
            }],
            exit_node: None,
            dns: None,
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
}
