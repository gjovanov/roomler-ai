//! P6: the declared-route RECONCILER.
//!
//! A route (`tunnel_core::localapi::RouteDescriptor`, persisted as
//! `[[tunnel_routes]]` in the daemon config) is INTENT: "keep a
//! forward/SOCKS5 listener up toward this node". This module maps that
//! intent onto the hub's ephemeral flows and nothing more:
//!
//! - it does NOT supervise sessions — every hub flow already runs under
//!   `client_mgr::run_flow_supervisor` (1 s→30 s backoff, WS-reconnect
//!   aware). Building a second retry loop here would double-supervise.
//! - the ONLY net-new retry is around **flow creation** (`create_forward`
//!   fails fast on a taken port / bad input) — a transiently-taken port at
//!   boot must not permanently skip a declared route.
//! - a flow whose supervisor stopped on a PERMANENT failure
//!   (`FlowLive.fatal`: enrollment revoked, cross-tenant) becomes the
//!   terminal [`RouteState::Failed`] — cleared only by an operator
//!   `route enable` (or remove). Without the terminal state, a revoked
//!   route would hammer the server with a doomed TunnelOpen every 30 s,
//!   across reboots, forever.
//!
//! Persistence: the reconciler is the daemon-side writer of the
//! `tunnel_routes` config field. Every load-modify-save runs under the
//! daemon-wide config-write lock shared with main.rs's other runtime
//! writers (clean-run promotion, graceful shutdown) — see
//! `config::WriteLock`. Cross-PROCESS writers (tray enroll/device-name,
//! CLI, wizard) remain last-writer-wins on the whole file; `config::save`
//! is atomic (temp+rename) so a torn file is impossible either way.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use tokio::sync::{Notify, watch};
use tracing::{info, warn};
use tunnel_core::localapi::{FlowKind, RouteDescriptor, RouteInfo, RouteState};

use super::client_mgr::TunnelClientHub;

/// Steady-state reconcile cadence. Every pass is cheap (in-memory diff +
/// a few hub map reads); creates only happen when something is out of
/// shape, so a short tick keeps `route add` → live latency low without
/// meaningful idle cost. A `kick` (route CRUD) reconciles immediately.
const RECONCILE_TICK: Duration = Duration::from_secs(5);

/// Create-retry backoff bounds (mirrors the hub's session backoff feel:
/// quick first retries, capped).
const CREATE_BACKOFF_MIN: Duration = Duration::from_secs(1);
const CREATE_BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Runtime state the reconciler tracks per declared route id. Routes with
/// no entry are simply not-yet-considered (next pass picks them up).
enum RouteRuntime {
    /// Awaiting (re)creation; carries the create-retry bookkeeping.
    Pending {
        consecutive_failures: u32,
        next_retry: Option<Instant>,
        last_error: Option<String>,
    },
    /// The route's flow exists in the hub (hub supervises the session).
    Live { flow_id: String },
    /// Terminal — supervisor hit a permanent failure. Operator re-enable
    /// (or remove) clears it.
    Failed { reason: String },
}

impl RouteRuntime {
    fn fresh() -> Self {
        RouteRuntime::Pending {
            consecutive_failures: 0,
            next_retry: None,
            last_error: None,
        }
    }
}

/// The reconciler handle: cheap to clone; the reconcile task and the
/// LocalAPI verbs share one inner.
#[derive(Clone)]
pub struct RouteReconciler {
    inner: Arc<Inner>,
}

struct Inner {
    hub: TunnelClientHub,
    /// The SAME config file the daemon loaded (resolution ladder already
    /// applied by `run_cmd`) — never re-derived here.
    config_path: PathBuf,
    /// Daemon-wide config write lock (shared with main.rs's writers).
    cfg_lock: crate::config::WriteLock,
    /// Declared routes — in-memory mirror of `config.tunnel_routes`.
    routes: StdMutex<Vec<RouteDescriptor>>,
    /// Per-route runtime state, keyed by route id.
    runtime: StdMutex<HashMap<String, RouteRuntime>>,
    /// Wakes the reconcile task immediately after a CRUD change.
    kick: Notify,
    /// Monotonic source for generated route ids (`route-N`).
    seq: std::sync::atomic::AtomicU64,
}

impl RouteReconciler {
    /// Build from the routes the daemon just loaded. Call
    /// [`RouteReconciler::spawn`] to start reconciling.
    pub fn new(
        hub: TunnelClientHub,
        config_path: PathBuf,
        cfg_lock: crate::config::WriteLock,
        declared: Vec<RouteDescriptor>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                hub,
                config_path,
                cfg_lock,
                routes: StdMutex::new(declared),
                runtime: StdMutex::new(HashMap::new()),
                kick: Notify::new(),
                seq: std::sync::atomic::AtomicU64::new(1),
            }),
        }
    }

    /// Spawn the reconcile task: a pass on every tick / kick, until
    /// `shutdown`. Route flows die with the daemon (hub tasks are aborted
    /// on shutdown like every ephemeral flow) and come back on the next
    /// start from the persisted descriptors.
    pub fn spawn(&self, mut shutdown: watch::Receiver<bool>) {
        let this = self.clone();
        tokio::spawn(async move {
            info!(
                declared = this.inner.routes.lock().unwrap().len(),
                "route reconciler started"
            );
            loop {
                this.reconcile_pass().await;
                tokio::select! {
                    biased;
                    _ = shutdown.changed() => {
                        if *shutdown.borrow() {
                            return;
                        }
                    }
                    _ = this.inner.kick.notified() => {}
                    _ = tokio::time::sleep(RECONCILE_TICK) => {}
                }
            }
        });
    }

    /// One reconcile pass: converge hub flows toward the declared set.
    async fn reconcile_pass(&self) {
        let declared = self.inner.routes.lock().unwrap().clone();

        // 1) Tear down runtime for routes that are gone or disabled.
        let (to_kill, to_drop): (Vec<String>, Vec<String>) = {
            let runtime = self.inner.runtime.lock().unwrap();
            let mut kill = Vec::new();
            let mut drop_keys = Vec::new();
            for (id, rt) in runtime.iter() {
                let still_wanted = declared.iter().any(|r| &r.id == id && r.enabled);
                if !still_wanted {
                    if let RouteRuntime::Live { flow_id } = rt {
                        kill.push(flow_id.clone());
                    }
                    drop_keys.push(id.clone());
                }
            }
            (kill, drop_keys)
        };
        for flow_id in to_kill {
            self.inner.hub.kill_flow(&flow_id);
        }
        {
            let mut runtime = self.inner.runtime.lock().unwrap();
            for id in to_drop {
                runtime.remove(&id);
            }
        }

        // 2) Converge each enabled route. Creates await the hub, so state
        //    reads/writes are scoped to keep the mutexes un-held across
        //    awaits.
        for route in declared.iter().filter(|r| r.enabled) {
            enum Action {
                Create,
                MarkFailed(String, String), // (flow_id to kill, reason)
                Nothing,
            }
            let action = {
                let mut runtime = self.inner.runtime.lock().unwrap();
                match runtime.get(&route.id) {
                    None => {
                        runtime.insert(route.id.clone(), RouteRuntime::fresh());
                        Action::Create
                    }
                    Some(RouteRuntime::Failed { .. }) => Action::Nothing,
                    Some(RouteRuntime::Live { flow_id }) => {
                        if let Some(reason) = self.inner.hub.flow_fatal(flow_id) {
                            Action::MarkFailed(flow_id.clone(), reason)
                        } else if !self.inner.hub.has_flow(flow_id) {
                            // Externally killed (operator `kill <flow>` on a
                            // route-owned flow). Declared intent wins —
                            // recreate; `route rm/disable` is the way to stop
                            // a declared route.
                            runtime.insert(route.id.clone(), RouteRuntime::fresh());
                            Action::Create
                        } else {
                            Action::Nothing
                        }
                    }
                    Some(RouteRuntime::Pending { next_retry, .. }) => {
                        let due = next_retry.map(|t| Instant::now() >= t).unwrap_or(true);
                        if due { Action::Create } else { Action::Nothing }
                    }
                }
            };

            match action {
                Action::Nothing => {}
                Action::MarkFailed(flow_id, reason) => {
                    warn!(route = %route.id, %reason, "route flow failed permanently; marking route Failed");
                    self.inner.hub.kill_flow(&flow_id);
                    self.inner
                        .runtime
                        .lock()
                        .unwrap()
                        .insert(route.id.clone(), RouteRuntime::Failed { reason });
                }
                Action::Create => {
                    let result = match route.kind {
                        FlowKind::Forward => {
                            let remote = route.remote.clone().unwrap_or_default();
                            self.inner
                                .hub
                                .create_forward(&route.node, route.local, &remote, &route.transport)
                                .await
                        }
                        FlowKind::Socks5 => {
                            self.inner
                                .hub
                                .create_socks5(&route.node, route.local, &route.transport)
                                .await
                        }
                    };
                    let mut runtime = self.inner.runtime.lock().unwrap();
                    match result {
                        Ok(flow_id) => {
                            info!(route = %route.id, flow = %flow_id, "route reconciled into a live flow");
                            runtime.insert(route.id.clone(), RouteRuntime::Live { flow_id });
                        }
                        Err(message) => {
                            let failures = match runtime.get(&route.id) {
                                Some(RouteRuntime::Pending {
                                    consecutive_failures,
                                    ..
                                }) => consecutive_failures + 1,
                                _ => 1,
                            };
                            let backoff = create_backoff(failures);
                            warn!(
                                route = %route.id, %message, failures,
                                backoff_s = backoff.as_secs(),
                                "route flow creation failed; retrying"
                            );
                            runtime.insert(
                                route.id.clone(),
                                RouteRuntime::Pending {
                                    consecutive_failures: failures,
                                    next_retry: Some(Instant::now() + backoff),
                                    last_error: Some(message),
                                },
                            );
                        }
                    }
                }
            }
        }
    }

    // ---- LocalAPI verb backs ---------------------------------------------

    /// The `RouteList` rows: declared descriptors joined with runtime state.
    pub fn list(&self) -> Vec<RouteInfo> {
        let routes = self.inner.routes.lock().unwrap().clone();
        let runtime = self.inner.runtime.lock().unwrap();
        let mut out: Vec<RouteInfo> = routes
            .into_iter()
            .map(|route| {
                let state = if !route.enabled {
                    RouteState::Disabled
                } else {
                    match runtime.get(&route.id) {
                        Some(RouteRuntime::Live { flow_id }) => RouteState::Active {
                            flow_id: flow_id.clone(),
                        },
                        Some(RouteRuntime::Failed { reason }) => RouteState::Failed {
                            reason: reason.clone(),
                        },
                        Some(RouteRuntime::Pending {
                            next_retry: Some(t),
                            last_error: Some(e),
                            ..
                        }) if *t > Instant::now() => RouteState::Backoff {
                            next_retry_secs: t.saturating_duration_since(Instant::now()).as_secs(),
                            last_error: e.clone(),
                        },
                        _ => RouteState::Pending,
                    }
                };
                RouteInfo { route, state }
            })
            .collect();
        out.sort_by(|a, b| a.route.id.cmp(&b.route.id));
        out
    }

    /// Validate + persist + reconcile a new route. Returns the effective
    /// descriptor (id generated when empty). `Err` is a user-facing
    /// message for the LocalAPI.
    pub async fn add(&self, mut route: RouteDescriptor) -> Result<RouteDescriptor, String> {
        // Validation mirrors what the hub will enforce at create time, so
        // a bad route fails HERE (once, with a clear message) instead of
        // silently backing off forever.
        super::client_mgr::parse_node(&route.node)?;
        match route.kind {
            FlowKind::Forward => {
                let remote = route
                    .remote
                    .as_deref()
                    .ok_or_else(|| "a forward route requires `remote` (host:port)".to_string())?;
                super::client_mgr::parse_host_port(remote).map_err(|e| e.to_string())?;
            }
            FlowKind::Socks5 => {
                if route.remote.is_some() {
                    return Err("a socks5 route must not set `remote`".to_string());
                }
            }
        }
        if route.local == 0 {
            return Err("`local` must be a non-zero port".to_string());
        }

        let effective = {
            let mut routes = self.inner.routes.lock().unwrap();
            if route.id.is_empty() {
                route.id = loop {
                    let candidate = format!(
                        "route-{}",
                        self.inner
                            .seq
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                    );
                    if !routes.iter().any(|r| r.id == candidate) {
                        break candidate;
                    }
                };
            } else if routes.iter().any(|r| r.id == route.id) {
                return Err(format!("a route with id '{}' already exists", route.id));
            }
            if route.enabled && routes.iter().any(|r| r.enabled && r.local == route.local) {
                return Err(format!(
                    "local port {} is already used by another enabled route",
                    route.local
                ));
            }
            routes.push(route.clone());
            routes.clone()
        };

        if let Err(e) = self.persist(&effective).await {
            // Roll the in-memory add back so memory matches disk.
            self.inner
                .routes
                .lock()
                .unwrap()
                .retain(|r| r.id != route.id);
            return Err(e);
        }
        self.inner.kick.notify_one();
        Ok(route)
    }

    /// Remove a declared route: kill its live flow, persist the removal.
    /// `Ok(false)` when the id was unknown.
    pub async fn remove(&self, id: &str) -> Result<bool, String> {
        let (found, snapshot) = {
            let mut routes = self.inner.routes.lock().unwrap();
            let before = routes.len();
            routes.retain(|r| r.id != id);
            (routes.len() != before, routes.clone())
        };
        if !found {
            return Ok(false);
        }
        self.persist(&snapshot).await?;
        // Tear down runtime + flow now (the next pass would too, but the
        // response should reflect the world).
        let live_flow = {
            let mut runtime = self.inner.runtime.lock().unwrap();
            match runtime.remove(id) {
                Some(RouteRuntime::Live { flow_id }) => Some(flow_id),
                _ => None,
            }
        };
        if let Some(flow_id) = live_flow {
            self.inner.hub.kill_flow(&flow_id);
        }
        info!(route = %id, "route removed");
        Ok(true)
    }

    /// Enable/disable a declared route. Enabling clears a terminal
    /// `Failed`. `Ok(false)` when the id was unknown.
    pub async fn set_enabled(&self, id: &str, enabled: bool) -> Result<bool, String> {
        let snapshot = {
            let mut routes = self.inner.routes.lock().unwrap();
            let Some(r) = routes.iter_mut().find(|r| r.id == id) else {
                return Ok(false);
            };
            r.enabled = enabled;
            routes.clone()
            // NB a re-enable that now clashes on `local` with another
            // enabled route isn't rejected here — the reconcile pass
            // surfaces it as Backoff{"port … in use"} against whichever
            // route loses the bind, which is honest about the runtime
            // reality (the add-time check prevents the common case).
        };
        self.persist(&snapshot).await?;
        {
            let mut runtime = self.inner.runtime.lock().unwrap();
            if enabled {
                // A re-enable resets Failed/backoff to a fresh Pending.
                runtime.insert(id.to_string(), RouteRuntime::fresh());
            } else if let Some(RouteRuntime::Live { flow_id }) = runtime.remove(id) {
                self.inner.hub.kill_flow(&flow_id);
            }
        }
        self.inner.kick.notify_one();
        info!(route = %id, enabled, "route enabled-state changed");
        Ok(true)
    }

    /// Persist `routes` as the config's `tunnel_routes` under the
    /// daemon-wide write lock (reload-modify-save so concurrent writers'
    /// OTHER fields are preserved).
    async fn persist(&self, routes: &[RouteDescriptor]) -> Result<(), String> {
        let _guard = self.inner.cfg_lock.lock().await;
        let mut cfg = crate::config::load(&self.inner.config_path)
            .map_err(|e| format!("could not reload config to persist routes: {e:#}"))?;
        cfg.tunnel_routes = routes.to_vec();
        crate::config::save(&self.inner.config_path, &cfg)
            .map_err(|e| format!("could not persist routes: {e:#}"))?;
        Ok(())
    }
}

/// Exponential create-retry backoff: 1 s, 2 s, 4 s, … capped at 30 s.
fn create_backoff(consecutive_failures: u32) -> Duration {
    let exp = consecutive_failures.saturating_sub(1).min(5);
    (CREATE_BACKOFF_MIN * 2u32.pow(exp)).min(CREATE_BACKOFF_MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn desc(id: &str, local: u16, enabled: bool) -> RouteDescriptor {
        RouteDescriptor {
            id: id.into(),
            kind: FlowKind::Forward,
            node: "aabbccddeeff001122334455".into(),
            local,
            remote: Some("db:5432".into()),
            transport: String::new(),
            enabled,
        }
    }

    fn reconciler(declared: Vec<RouteDescriptor>) -> (RouteReconciler, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        // Persist targets need a loadable config on disk.
        let mut cfg = crate::config::test_fixture();
        cfg.tunnel_routes = declared.clone();
        crate::config::save(&path, &cfg).unwrap();
        let r = RouteReconciler::new(
            TunnelClientHub::new("test".into()),
            path,
            Arc::new(tokio::sync::Mutex::new(())),
            declared,
        );
        (r, dir)
    }

    #[test]
    fn create_backoff_grows_and_caps() {
        assert_eq!(create_backoff(1), Duration::from_secs(1));
        assert_eq!(create_backoff(2), Duration::from_secs(2));
        assert_eq!(create_backoff(3), Duration::from_secs(4));
        assert_eq!(create_backoff(6), Duration::from_secs(30)); // 2^5=32 → cap
        assert_eq!(create_backoff(60), Duration::from_secs(30));
    }

    #[tokio::test]
    async fn list_maps_disabled_and_pending() {
        let (r, _dir) = reconciler(vec![desc("a", 1001, true), desc("b", 1002, false)]);
        let rows = r.list();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].route.id, "a");
        assert_eq!(rows[0].state, RouteState::Pending);
        assert_eq!(rows[1].state, RouteState::Disabled);
    }

    #[tokio::test]
    async fn add_validates_and_persists_and_generates_ids() {
        let (r, _dir) = reconciler(vec![]);

        // Bad node.
        let mut bad = desc("x", 1010, true);
        bad.node = "nope".into();
        assert!(r.add(bad).await.is_err());

        // Forward without remote.
        let mut bad = desc("x", 1010, true);
        bad.remote = None;
        assert!(r.add(bad).await.is_err());

        // Socks5 with a remote.
        let mut bad = desc("x", 1010, true);
        bad.kind = FlowKind::Socks5;
        assert!(r.add(bad).await.is_err());

        // Good — empty id gets generated; persisted to the config file.
        let mut ok = desc("", 1010, true);
        ok.id = String::new();
        let eff = r.add(ok).await.unwrap();
        assert!(eff.id.starts_with("route-"), "generated id, got {}", eff.id);
        let on_disk = crate::config::load(&r.inner.config_path).unwrap();
        assert_eq!(on_disk.tunnel_routes.len(), 1);
        assert_eq!(on_disk.tunnel_routes[0].id, eff.id);

        // Duplicate id rejected.
        let dup = desc(&eff.id, 1011, true);
        assert!(r.add(dup).await.is_err());

        // Duplicate enabled local port rejected.
        let clash = desc("other", 1010, true);
        let err = r.add(clash).await.unwrap_err();
        assert!(err.contains("already used"), "got {err}");
    }

    #[tokio::test]
    async fn remove_and_set_enabled_round_trip_config() {
        let (r, _dir) = reconciler(vec![desc("a", 1001, true)]);

        assert!(r.set_enabled("a", false).await.unwrap());
        assert!(
            !crate::config::load(&r.inner.config_path)
                .unwrap()
                .tunnel_routes[0]
                .enabled
        );
        assert_eq!(r.list()[0].state, RouteState::Disabled);

        assert!(r.set_enabled("a", true).await.unwrap());
        assert_eq!(r.list()[0].state, RouteState::Pending);

        assert!(!r.set_enabled("ghost", true).await.unwrap());

        assert!(r.remove("a").await.unwrap());
        assert!(
            crate::config::load(&r.inner.config_path)
                .unwrap()
                .tunnel_routes
                .is_empty()
        );
        assert!(!r.remove("a").await.unwrap());
    }

    #[tokio::test]
    async fn reenable_clears_terminal_failed() {
        let (r, _dir) = reconciler(vec![desc("a", 1001, true)]);
        r.inner.runtime.lock().unwrap().insert(
            "a".into(),
            RouteRuntime::Failed {
                reason: "revoked".into(),
            },
        );
        assert!(matches!(r.list()[0].state, RouteState::Failed { .. }));
        // Disable → Disabled wins over Failed…
        assert!(r.set_enabled("a", false).await.unwrap());
        assert_eq!(r.list()[0].state, RouteState::Disabled);
        // …re-enable → fresh Pending (Failed cleared).
        assert!(r.set_enabled("a", true).await.unwrap());
        assert_eq!(r.list()[0].state, RouteState::Pending);
    }
}
