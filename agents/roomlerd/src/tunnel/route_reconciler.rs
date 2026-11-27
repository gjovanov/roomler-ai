// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
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
//! #1035 — ONE writer owns the map from declared routes to hub flows: the
//! reconcile task. The verbs (`add`, `replace`, `remove`, `set_enabled`)
//! write only the DECLARED side, the list and each route's generation (which
//! every change to the route bumps); they save it, kick the task, and answer
//! after the pass that applies their change. Every flow a pass creates names
//! its route in the hub's record (`client_mgr::RouteTag`: the descriptor the
//! flow was built from, and that generation), and each pass adopts or stops a
//! tagged flow the map does not hold. Before this the verbs wrote the map
//! after awaiting the config save, and the pass after awaiting the hub's
//! create, each without looking: a pass inside `route enable`'s save left a
//! live flow that no entry named, and the route read `backoff … 10048` until
//! the daemon restarted.
//!
//! Persistence: the reconciler is the daemon-side writer of the
//! `tunnel_routes` config field. Every load-modify-save runs under the
//! daemon-wide config-write lock shared with app.rs's other runtime
//! writers (clean-run promotion, graceful shutdown) — see
//! `config::WriteLock`. Cross-PROCESS writers (tray enroll/device-name,
//! CLI, wizard) remain last-writer-wins on the whole file; `config::save`
//! is atomic (temp+rename) so a torn file is impossible either way.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::{Duration, Instant};

use tokio::sync::{Notify, watch};
use tracing::{info, warn};
use tunnel_core::localapi::{FlowKind, RouteDescriptor, RouteInfo, RouteState};

use super::client_mgr::{FlowReport, RouteTag, TunnelClientHub};
use super::port_holder::{Holder, PortFlow};

/// Steady-state reconcile cadence. Every pass is cheap (in-memory diff +
/// a few hub map reads); creates only happen when something is out of
/// shape, so a short tick keeps `route add` → live latency low without
/// meaningful idle cost. A `kick` (route CRUD) reconciles immediately.
const RECONCILE_TICK: Duration = Duration::from_secs(5);

/// Create-retry backoff bounds (mirrors the hub's session backoff feel:
/// quick first retries, capped).
const CREATE_BACKOFF_MIN: Duration = Duration::from_secs(1);
const CREATE_BACKOFF_MAX: Duration = Duration::from_secs(30);

/// #1035 — how long a verb waits for the pass that applies its change. A
/// pass is in-memory work and one loopback bind per create, so it takes
/// milliseconds; the bound covers a task that is gone or stuck. Past it the
/// verb answers with its change saved, and the next pass applies it.
const APPLY_WAIT: Duration = Duration::from_secs(5);

/// Runtime state the reconciler tracks per declared route id. Routes with
/// no entry are simply not-yet-considered (next pass picks them up).
///
/// #1035 — written by the reconcile task alone. Each entry carries the
/// generation of its route that it was written for, and the next pass resets
/// an entry an older generation wrote (keeping a live flow that still serves
/// the route).
enum RouteRuntime {
    /// Awaiting (re)creation; carries the create-retry bookkeeping.
    Pending {
        consecutive_failures: u32,
        next_retry: Option<Instant>,
        last_error: Option<String>,
        generation: u64,
    },
    /// The route's flow exists in the hub (hub supervises the session).
    Live { flow_id: String, generation: u64 },
    /// Terminal — supervisor hit a permanent failure. Operator re-enable
    /// (or remove) clears it.
    Failed { reason: String, generation: u64 },
}

impl RouteRuntime {
    fn fresh(generation: u64) -> Self {
        RouteRuntime::Pending {
            consecutive_failures: 0,
            next_retry: None,
            last_error: None,
            generation,
        }
    }

    fn generation(&self) -> u64 {
        match self {
            RouteRuntime::Pending { generation, .. }
            | RouteRuntime::Live { generation, .. }
            | RouteRuntime::Failed { generation, .. } => *generation,
        }
    }
}

/// #1035 — the declared side of the map, the only state the verbs write.
struct Declared {
    /// In-memory mirror of `config.tunnel_routes`.
    routes: Vec<RouteDescriptor>,
    /// Each declared route's generation. Every change to a route (add,
    /// replace, enable, disable) bumps it, so a change that leaves the
    /// descriptor as it was (an enable that clears a terminal `Failed` or a
    /// create backoff) still reaches the pass.
    generations: HashMap<String, u64>,
    /// The last number handed out, by any change: one counter for every
    /// route, so a removed and re-added id never reuses a generation. It is
    /// also the revision a verb waits for a pass to apply.
    revision: u64,
}

impl Declared {
    fn new(routes: Vec<RouteDescriptor>) -> Self {
        let mut declared = Declared {
            routes: Vec::new(),
            generations: HashMap::new(),
            revision: 0,
        };
        for route in &routes {
            declared.bump(&route.id);
        }
        declared.routes = routes;
        declared
    }

    /// A change to route `id`: its new generation, which is also the revision
    /// the change is applied at.
    fn bump(&mut self, id: &str) -> u64 {
        self.revision += 1;
        self.generations.insert(id.to_string(), self.revision);
        self.revision
    }

    /// Route `id` is no longer declared: the revision of that change.
    fn forget(&mut self, id: &str) -> u64 {
        self.generations.remove(id);
        self.revision += 1;
        self.revision
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
    /// Daemon-wide config write lock (shared with app.rs's writers).
    cfg_lock: crate::config::WriteLock,
    /// #1035 — the declared routes and their generations: what the verbs
    /// write.
    declared: StdMutex<Declared>,
    /// Per-route runtime state, keyed by route id. #1035: written by the
    /// reconcile task alone (`reconcile_pass`, and `clear_pending_backoffs`
    /// between passes), never by a verb.
    runtime: StdMutex<HashMap<String, RouteRuntime>>,
    /// Wakes the reconcile task immediately after a CRUD change.
    kick: Notify,
    /// #1035 — the declared revision the reconcile task's last pass applied;
    /// the verbs wait on it. Set once, by [`RouteReconciler::spawn`]. With no
    /// task (a test that runs its passes by hand) a verb does not wait.
    applied: OnceLock<watch::Receiver<u64>>,
    /// #1035 — a pass is running. Two at once would be two writers of the
    /// map; debug builds assert it never happens.
    in_pass: AtomicBool,
    /// Monotonic source for generated route ids (`route-N`).
    seq: std::sync::atomic::AtomicU64,
    /// Test seam (#1035): parks the next create between the hub's answer and
    /// the pass's commit.
    #[cfg(test)]
    gate: StdMutex<Option<CreateGate>>,
    /// Test seam (#1035): the creates the passes have asked the hub for.
    #[cfg(test)]
    creates: std::sync::atomic::AtomicU32,
}

/// Test seam (#1035): a test lands a verb inside a create's await with it.
/// `reached` gets the hub's answer, and the pass commits once `release`
/// fires (or its sender is dropped).
#[cfg(test)]
struct CreateGate {
    reached: tokio::sync::oneshot::Sender<Result<String, String>>,
    release: tokio::sync::oneshot::Receiver<()>,
}

/// #1035 — held by a pass for its whole life (`Inner::in_pass`).
struct OnePass<'a>(&'a AtomicBool);

impl<'a> OnePass<'a> {
    fn enter(running: &'a AtomicBool) -> Self {
        let already = running.swap(true, Ordering::AcqRel);
        debug_assert!(
            !already,
            "two reconcile passes at once: the route map has one writer (#1035)"
        );
        OnePass(running)
    }
}

impl Drop for OnePass<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
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
                declared: StdMutex::new(Declared::new(declared)),
                runtime: StdMutex::new(HashMap::new()),
                kick: Notify::new(),
                applied: OnceLock::new(),
                in_pass: AtomicBool::new(false),
                seq: std::sync::atomic::AtomicU64::new(1),
                #[cfg(test)]
                gate: StdMutex::new(None),
                #[cfg(test)]
                creates: std::sync::atomic::AtomicU32::new(0),
            }),
        }
    }

    /// Spawn the reconcile task: a pass on every tick / kick, until
    /// `shutdown`. Route flows die with the daemon (hub tasks are aborted
    /// on shutdown like every ephemeral flow) and come back on the next
    /// start from the persisted descriptors.
    ///
    /// #1035 — this task is the route map's ONE writer, so a second call
    /// starts nothing. After each pass it publishes the declared revision
    /// the pass applied, which the verbs wait for.
    pub fn spawn(&self, mut shutdown: watch::Receiver<bool>) {
        let (applied_tx, applied_rx) = watch::channel(0u64);
        if self.inner.applied.set(applied_rx).is_err() {
            warn!(
                "route reconciler already running; not starting a second writer of the route map"
            );
            return;
        }
        let this = self.clone();
        tokio::spawn(async move {
            info!(
                declared = this.inner.declared.lock().unwrap().routes.len(),
                "route reconciler started"
            );
            // R1: a netstate Major clears every Pending route's backoff and
            // reconciles immediately — the failures that earned the backoff
            // described a network that no longer exists. The clear is what
            // makes the wake-up effective: `reconcile_pass` gates creation
            // on `now >= next_retry`, so a bare kick would still see the
            // route as not-due. Damped upstream (≤1 material Major/120 s).
            let mut net_rx = super::netwatch::subscribe();
            loop {
                let revision = this.reconcile_pass().await;
                // The verbs waiting on a change at or below `revision`
                // answer now.
                applied_tx.send_replace(revision);
                tokio::select! {
                    biased;
                    _ = shutdown.changed() => {
                        if *shutdown.borrow() {
                            return;
                        }
                    }
                    _ = this.inner.kick.notified() => {}
                    summary = super::netwatch::next_major(&mut net_rx) => {
                        let cleared = this.clear_pending_backoffs();
                        info!(%summary, cleared, "network changed — reconciling routes now");
                    }
                    _ = tokio::time::sleep(RECONCILE_TICK) => {}
                }
            }
        });
    }

    /// Clear the `next_retry` gate on every `Pending` route so the next
    /// `reconcile_pass` re-creates them immediately. Returns how many were
    /// cleared. Deliberately does NOT touch `consecutive_failures` (the
    /// ladder resumes where it was if the new network fails too) and never
    /// touches terminal `Failed` (operator-owned).
    fn clear_pending_backoffs(&self) -> usize {
        let mut runtime = self.inner.runtime.lock().unwrap();
        let mut cleared = 0;
        for rt in runtime.values_mut() {
            if let RouteRuntime::Pending {
                next_retry: next_retry @ Some(_),
                ..
            } = rt
            {
                *next_retry = None;
                cleared += 1;
            }
        }
        cleared
    }

    /// One reconcile pass: converge hub flows toward the declared set.
    /// Returns the declared revision it applied.
    ///
    /// #1035 — the pass is the route map's one writer, so it can read an
    /// entry, await the hub and write the entry back with no other writer in
    /// between. What can change under it is the declared side, which the
    /// verbs write at any time: a create commits only for the descriptor its
    /// route still declares, and stops its own flow otherwise.
    async fn reconcile_pass(&self) -> u64 {
        let _one = OnePass::enter(&self.inner.in_pass);
        let (declared, generations, revision) = {
            let d = self.inner.declared.lock().unwrap();
            (d.routes.clone(), d.generations.clone(), d.revision)
        };
        let generation_of = |id: &str| generations.get(id).copied().unwrap_or(0);

        // 1) Entries. A route that is gone or disabled loses its entry and its
        //    flow. An entry an older generation of its route wrote starts
        //    over, unless it is a live flow that still serves the route as
        //    declared now: an enable keeps such a flow, and a replace never
        //    keeps the old descriptor's.
        let to_kill: Vec<String> = {
            let mut runtime = self.inner.runtime.lock().unwrap();
            let mut kill = Vec::new();
            runtime.retain(|id, rt| {
                let Some(route) = declared.iter().find(|r| &r.id == id && r.enabled) else {
                    if let RouteRuntime::Live { flow_id, .. } = rt {
                        kill.push(flow_id.clone());
                    }
                    return false;
                };
                let generation = generation_of(id);
                if rt.generation() != generation {
                    match rt {
                        RouteRuntime::Live {
                            flow_id,
                            generation: written_for,
                        } if self.serves(flow_id, route) => *written_for = generation,
                        _ => {
                            if let RouteRuntime::Live { flow_id, .. } = rt {
                                kill.push(flow_id.clone());
                            }
                            *rt = RouteRuntime::fresh(generation);
                        }
                    }
                }
                true
            });
            kill
        };
        for flow_id in to_kill {
            self.inner.hub.kill_flow(&flow_id);
        }

        // 2) Flows the map lost: adopted or stopped (#1035).
        self.adopt_or_stop_lost_flows(&declared, &generations);

        // 3) Converge each enabled route. Creates await the hub, so state
        //    reads/writes are scoped to keep the mutexes un-held across
        //    awaits.
        for route in declared.iter().filter(|r| r.enabled) {
            let generation = generation_of(&route.id);
            // Multi-org P1: only primary-org routes are supervised. A
            // secondary label is legal in the schema (clients may write it
            // ahead of the P2/P3 supervision slice) but parks as terminal
            // Failed so a mislabeled route can't hammer the WRONG org's WS
            // with doomed opens.
            if let Some(org) = route.org.as_deref()
                && org != crate::config::PRIMARY_ORG_LABEL
            {
                let mut runtime = self.inner.runtime.lock().unwrap();
                if !matches!(runtime.get(&route.id), Some(RouteRuntime::Failed { .. })) {
                    warn!(
                        route = %route.id,
                        %org,
                        "declared route names a secondary org — parked (org route \
                         supervision lands with multi-org P2/P3)"
                    );
                    runtime.insert(
                        route.id.clone(),
                        RouteRuntime::Failed {
                            reason: format!(
                                "org {org:?} routes are not supervised yet (P1: primary only)"
                            ),
                            generation,
                        },
                    );
                }
                continue;
            }
            enum Action {
                Create,
                MarkFailed(String, String), // (flow_id to kill, reason)
                Nothing,
            }
            let action = {
                let mut runtime = self.inner.runtime.lock().unwrap();
                match runtime.get(&route.id) {
                    None => {
                        runtime.insert(route.id.clone(), RouteRuntime::fresh(generation));
                        Action::Create
                    }
                    Some(RouteRuntime::Failed { .. }) => Action::Nothing,
                    Some(RouteRuntime::Live { flow_id, .. }) => {
                        if let Some(reason) = self.inner.hub.flow_fatal(flow_id) {
                            Action::MarkFailed(flow_id.clone(), reason)
                        } else if !self.inner.hub.has_flow(flow_id) {
                            // Externally killed (operator `kill <flow>` on a
                            // route-owned flow). Declared intent wins —
                            // recreate; `route rm/disable` is the way to stop
                            // a declared route.
                            runtime.insert(route.id.clone(), RouteRuntime::fresh(generation));
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
                    self.inner.runtime.lock().unwrap().insert(
                        route.id.clone(),
                        RouteRuntime::Failed { reason, generation },
                    );
                }
                Action::Create => {
                    #[cfg(test)]
                    self.inner
                        .creates
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let result = self
                        .inner
                        .hub
                        .create_route_flow(RouteTag {
                            descriptor: route.clone(),
                            generation,
                        })
                        .await;
                    #[cfg(test)]
                    self.park_at_test_gate(&result).await;
                    // #1035 — a verb may have changed the route while the hub
                    // created its flow. Commit only for the descriptor the
                    // route still declares; otherwise the flow serves no
                    // route, so stop it now, and the next pass builds what the
                    // route wants. Recording it would leave a disabled route
                    // with a flow, or a replaced one on its old port.
                    let still_declared = self
                        .inner
                        .declared
                        .lock()
                        .unwrap()
                        .routes
                        .iter()
                        .any(|r| r == route);
                    if let (Ok(flow_id), false) = (&result, still_declared) {
                        info!(
                            route = %route.id, flow = %flow_id,
                            "the route changed while its flow was created; stopping that flow"
                        );
                        self.inner.hub.kill_flow(flow_id);
                        continue;
                    }
                    let mut runtime = self.inner.runtime.lock().unwrap();
                    match result {
                        Ok(flow_id) => {
                            info!(route = %route.id, flow = %flow_id, "route reconciled into a live flow");
                            super::port_holder::note_ok(&route.id);
                            runtime.insert(
                                route.id.clone(),
                                RouteRuntime::Live {
                                    flow_id,
                                    generation,
                                },
                            );
                        }
                        Err(message) => {
                            // FR-84 D6 / #1035 — refused its port for two
                            // minutes: say WHO holds it (off the reconcile
                            // pass; the lookup reads tables and /proc).
                            if super::port_holder::note_failure(&route.id, &message) {
                                let (this, route_id, port) =
                                    (self.clone(), route.id.clone(), route.local);
                                tokio::spawn(async move {
                                    if let Ok(holder) = tokio::task::spawn_blocking(move || {
                                        super::port_holder::lookup(port)
                                    })
                                    .await
                                    {
                                        warn!(
                                            route = %route_id,
                                            port,
                                            holder = %this.holder_line(port, &holder),
                                            "route still cannot bind its local port"
                                        );
                                    }
                                });
                            }
                            let failures = match runtime.get(&route.id) {
                                Some(RouteRuntime::Pending {
                                    consecutive_failures,
                                    ..
                                }) => consecutive_failures + 1,
                                _ => 1,
                            };
                            let backoff = create_backoff(failures);
                            warn!(
                                route = %route.id, reason = %message, failures,
                                backoff_s = backoff.as_secs(),
                                "route flow creation failed; retrying"
                            );
                            runtime.insert(
                                route.id.clone(),
                                RouteRuntime::Pending {
                                    consecutive_failures: failures,
                                    next_retry: Some(Instant::now() + backoff),
                                    last_error: Some(message),
                                    generation,
                                },
                            );
                        }
                    }
                }
            }
        }
        revision
    }

    /// #1035 — a hub flow tagged for a route whose entry does not hold it is
    /// a flow the map lost. Adopt it when it serves its route as declared
    /// now and the route has no flow of its own; stop it in every other case,
    /// through `kill_flow`, which also tells the server its session (#1754).
    /// A lost handle costs one pass, never a daemon restart. A flow with no
    /// tag (an ad-hoc forward or SOCKS5 listener) is not the reconciler's,
    /// and is never touched.
    fn adopt_or_stop_lost_flows(
        &self,
        declared: &[RouteDescriptor],
        generations: &HashMap<String, u64>,
    ) {
        let mut stop = Vec::new();
        {
            let mut runtime = self.inner.runtime.lock().unwrap();
            // Oldest first: of two lost flows for one route, the older one is
            // the likelier to hold the port, and it is the one adopted.
            for flow in self.inner.hub.flow_routes() {
                let Some(tag) = flow.route else {
                    continue;
                };
                let id = tag.descriptor.id.clone();
                let route_has_no_flow = match runtime.get(&id) {
                    Some(RouteRuntime::Live { flow_id, .. }) if *flow_id == flow.flow_id => {
                        continue; // held: not lost
                    }
                    None | Some(RouteRuntime::Pending { .. }) => true,
                    Some(RouteRuntime::Live { flow_id, .. }) => !self.inner.hub.has_flow(flow_id),
                    Some(RouteRuntime::Failed { .. }) => false,
                };
                let serves_as_declared =
                    !flow.fatal && declared.iter().any(|r| r.enabled && *r == tag.descriptor);
                if serves_as_declared && route_has_no_flow {
                    info!(route = %id, flow = %flow.flow_id, "adopting a flow the route had lost (#1035)");
                    let generation = generations.get(&id).copied().unwrap_or(0);
                    runtime.insert(
                        id,
                        RouteRuntime::Live {
                            flow_id: flow.flow_id,
                            generation,
                        },
                    );
                } else {
                    warn!(
                        route = %id, flow = %flow.flow_id, built_for_generation = tag.generation,
                        "stopping a flow the route had lost (#1035)"
                    );
                    stop.push(flow.flow_id);
                }
            }
        }
        for flow_id in stop {
            self.inner.hub.kill_flow(&flow_id);
        }
    }

    /// #1035 — does hub flow `flow_id` serve `route` as it is declared now:
    /// built from exactly this descriptor, and not stopped on a permanent
    /// failure? A flow built from an older descriptor (another port, node,
    /// target or transport) never does.
    fn serves(&self, flow_id: &str, route: &RouteDescriptor) -> bool {
        self.inner.hub.flow_route(flow_id).is_some_and(|flow| {
            !flow.fatal && flow.route.is_some_and(|tag| tag.descriptor == *route)
        })
    }

    // ---- LocalAPI verb backs ---------------------------------------------

    /// The `RouteList` rows: declared descriptors joined with runtime state.
    ///
    /// #1685 — a `Live` runtime entry means the hub HAS a flow for the route,
    /// not that the route serves; the state a reader gets comes from the
    /// flow's own report ([`route_state_for_flow`]). Before this, `Live` was
    /// mapped straight to `Active`, and a route to an offline node read
    /// `active` with nothing listening on its port.
    ///
    /// #1035 — an entry an older generation of its route wrote reads the way
    /// the next pass leaves it, so a row is right before that pass runs.
    pub fn list(&self) -> Vec<RouteInfo> {
        let (routes, generations) = {
            let d = self.inner.declared.lock().unwrap();
            (d.routes.clone(), d.generations.clone())
        };
        let runtime = self.inner.runtime.lock().unwrap();
        let mut out: Vec<RouteInfo> = routes
            .into_iter()
            .map(|route| {
                let state = if !route.enabled {
                    RouteState::Disabled
                } else {
                    let generation = generations.get(&route.id).copied().unwrap_or(0);
                    match runtime.get(&route.id) {
                        // The next pass starts this entry over: a flow built
                        // from an older descriptor is not the route's flow,
                        // and an enable clears a failure or a backoff.
                        Some(rt)
                            if rt.generation() != generation
                                && !matches!(rt, RouteRuntime::Live { flow_id, .. }
                                    if self.serves(flow_id, &route)) =>
                        {
                            RouteState::Pending { flow_id: None }
                        }
                        Some(RouteRuntime::Live { flow_id, .. }) => {
                            match self.inner.hub.flow_report(flow_id) {
                                Some(report) => route_state_for_flow(flow_id, &report),
                                // Killed by hand; the next pass recreates it.
                                None => RouteState::Pending { flow_id: None },
                            }
                        }
                        Some(RouteRuntime::Failed { reason, .. }) => RouteState::Failed {
                            reason: reason.clone(),
                        },
                        Some(RouteRuntime::Pending {
                            consecutive_failures,
                            next_retry: Some(t),
                            last_error: Some(e),
                            ..
                        }) if *t > Instant::now() => RouteState::Backoff {
                            next_retry_secs: t.saturating_duration_since(Instant::now()).as_secs(),
                            last_error: e.clone(),
                            // No flow: creating one is what keeps failing.
                            flow_id: None,
                            attempts: *consecutive_failures,
                        },
                        _ => RouteState::Pending { flow_id: None },
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
    /// message for the LocalAPI. #1035: answers after the pass that applies
    /// it, so the route's flow exists (or its create failure is recorded).
    pub async fn add(&self, mut route: RouteDescriptor) -> Result<RouteDescriptor, String> {
        validate_shape(&route)?;

        let (effective, revision) = {
            let mut declared = self.inner.declared.lock().unwrap();
            if route.id.is_empty() {
                route.id = loop {
                    let candidate = format!(
                        "route-{}",
                        self.inner
                            .seq
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                    );
                    if !declared.routes.iter().any(|r| r.id == candidate) {
                        break candidate;
                    }
                };
            } else if declared.routes.iter().any(|r| r.id == route.id) {
                return Err(format!("a route with id '{}' already exists", route.id));
            }
            if route.enabled
                && declared
                    .routes
                    .iter()
                    .any(|r| r.enabled && r.local == route.local)
            {
                return Err(format!(
                    "local port {} is already used by another enabled route",
                    route.local
                ));
            }
            declared.routes.push(route.clone());
            let revision = declared.bump(&route.id);
            (declared.routes.clone(), revision)
        };

        if let Err(e) = self.persist(&effective).await {
            // Roll the in-memory add back so memory matches disk. A pass may
            // have built the route's flow meanwhile; the next one stops it.
            {
                let mut declared = self.inner.declared.lock().unwrap();
                declared.routes.retain(|r| r.id != route.id);
                declared.forget(&route.id);
            }
            self.inner.kick.notify_one();
            return Err(e);
        }
        self.inner.kick.notify_one();
        self.await_applied(revision).await;
        Ok(route)
    }

    /// FR-84 D1 — replace a declared route IN ONE STEP, keyed by `route.id`
    /// (the `RouteUpdate` verb).
    ///
    /// Validated exactly like [`Self::add`], with the route being replaced
    /// excepted from the local-port clash check. An invalid descriptor is
    /// refused before anything changes, so the OLD route keeps running and
    /// stays persisted. A valid one is written to config in ONE save; the
    /// pass this verb then answers after (#1035) stops the old flow and
    /// builds the new descriptor's. The obvious alternative — `remove` then
    /// `add` — loses the route entirely when the add fails: the removal is
    /// already on disk by then.
    ///
    /// `Ok` carries the effective descriptor. A replacement identical to what
    /// is declared is a no-op (nothing is restarted).
    pub async fn replace(&self, route: RouteDescriptor) -> Result<RouteDescriptor, String> {
        if route.id.trim().is_empty() {
            return Err("a route update needs the id of the route to replace".to_string());
        }
        validate_shape(&route)?;

        // Swap the descriptor in memory (remembering the old one), then
        // persist; on a failed save put the old one back so memory matches
        // disk — the same rollback discipline as `add`.
        let (previous, snapshot, revision) = {
            let mut declared = self.inner.declared.lock().unwrap();
            let Some(idx) = declared.routes.iter().position(|r| r.id == route.id) else {
                return Err(format!("no declared route with id '{}'", route.id));
            };
            if declared.routes[idx] == route {
                return Ok(route);
            }
            if route.enabled
                && declared
                    .routes
                    .iter()
                    .enumerate()
                    .any(|(i, r)| i != idx && r.enabled && r.local == route.local)
            {
                return Err(format!(
                    "local port {} is already used by another enabled route",
                    route.local
                ));
            }
            let previous = std::mem::replace(&mut declared.routes[idx], route.clone());
            let revision = declared.bump(&route.id);
            (previous, declared.routes.clone(), revision)
        };
        if let Err(e) = self.persist(&snapshot).await {
            {
                let mut declared = self.inner.declared.lock().unwrap();
                if let Some(slot) = declared.routes.iter_mut().find(|r| r.id == route.id) {
                    *slot = previous;
                }
                declared.bump(&route.id);
            }
            self.inner.kick.notify_one();
            return Err(e);
        }

        // Persisted. The pass stops the old flow (a flow built from the old
        // descriptor never serves the new one) and builds the new one; a
        // disabled replacement has none.
        self.inner.kick.notify_one();
        self.await_applied(revision).await;
        info!(route = %route.id, local = route.local, enabled = route.enabled, "route replaced");
        Ok(route)
    }

    /// Remove a declared route and persist the removal; the pass this verb
    /// answers after (#1035) stops the route's flow, and any flow the map had
    /// lost for it. `Ok(false)` when the id was unknown.
    pub async fn remove(&self, id: &str) -> Result<bool, String> {
        let (found, snapshot, revision) = {
            let mut declared = self.inner.declared.lock().unwrap();
            let before = declared.routes.len();
            declared.routes.retain(|r| r.id != id);
            let found = declared.routes.len() != before;
            let revision = if found {
                declared.forget(id)
            } else {
                declared.revision
            };
            (found, declared.routes.clone(), revision)
        };
        if !found {
            return Ok(false);
        }
        self.persist(&snapshot).await?;
        self.inner.kick.notify_one();
        self.await_applied(revision).await;
        info!(route = %id, "route removed");
        Ok(true)
    }

    /// Enable/disable a declared route. `Ok(false)` when the id was unknown.
    ///
    /// #1035 — the change bumps the route's generation. So enabling clears a
    /// terminal `Failed` and a create backoff (the pass starts over an entry
    /// an older generation wrote), and an enable of a route that is already
    /// live keeps its flow. The verb answers after the pass that applies it.
    pub async fn set_enabled(&self, id: &str, enabled: bool) -> Result<bool, String> {
        let (snapshot, revision) = {
            let mut declared = self.inner.declared.lock().unwrap();
            let Some(r) = declared.routes.iter_mut().find(|r| r.id == id) else {
                return Ok(false);
            };
            r.enabled = enabled;
            // NB a re-enable that now clashes on `local` with another
            // enabled route isn't rejected here — the reconcile pass
            // surfaces it as Backoff{"port … in use"} against whichever
            // route loses the bind, which is honest about the runtime
            // reality (the add-time check prevents the common case).
            let revision = declared.bump(id);
            (declared.routes.clone(), revision)
        };
        self.persist(&snapshot).await?;
        self.inner.kick.notify_one();
        self.await_applied(revision).await;
        info!(route = %id, enabled, "route enabled-state changed");
        Ok(true)
    }

    /// #1035 — wait for the pass that applies `revision`, so a verb's answer
    /// reflects the world as it did when the verbs wrote the map themselves:
    /// after `route rm` the route's flow is gone, after `route add` it exists
    /// (or its create failure is recorded). Bounded by [`APPLY_WAIT`]: past
    /// it the change is saved and the next pass applies it. No reconcile
    /// task, or one that has stopped, means no wait.
    async fn await_applied(&self, revision: u64) {
        let Some(applied) = self.inner.applied.get() else {
            return;
        };
        let mut applied = applied.clone();
        let waited = tokio::time::timeout(APPLY_WAIT, async move {
            // `Err` = the task stopped: nothing left to wait for.
            let _ = applied.wait_for(|done| *done >= revision).await;
        })
        .await;
        if waited.is_err() {
            warn!(
                revision,
                wait_s = APPLY_WAIT.as_secs(),
                "a route change is saved; the reconcile pass has not applied it yet"
            );
        }
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

    /// FR-84 D6 / #1035 — the log line's worth of who holds `port`, a port a
    /// declared route has been refused for [`super::port_holder::STUCK_AFTER`].
    /// When the holder is this daemon, it names the tunnel flow on the port
    /// and what that flow is to the routes: a flow its route lost (#1035's
    /// shape), another route's serving flow, or an ad-hoc one.
    fn holder_line(&self, port: u16, holder: &Holder) -> String {
        let flow = match holder {
            Holder::ThisDaemon { .. } => self.flow_on_port(port),
            _ => None,
        };
        super::port_holder::describe_flow(holder, flow.as_ref())
    }

    /// The hub's flow on `port`, as the map sees it; a lost one first, since
    /// it is the shape the holder line exists for.
    fn flow_on_port(&self, port: u16) -> Option<PortFlow> {
        let flows = self.inner.hub.flow_routes();
        let runtime = self.inner.runtime.lock().unwrap();
        let mut found: Vec<PortFlow> = flows
            .into_iter()
            .filter(|f| f.local == port)
            .map(|f| match f.route {
                None => PortFlow::AdHoc { flow: f.flow_id },
                Some(tag) => {
                    let route = tag.descriptor.id;
                    let held = matches!(runtime.get(&route),
                        Some(RouteRuntime::Live { flow_id, .. }) if *flow_id == f.flow_id);
                    if held {
                        PortFlow::Serving {
                            flow: f.flow_id,
                            route,
                        }
                    } else {
                        PortFlow::Lost {
                            flow: f.flow_id,
                            route,
                        }
                    }
                }
            })
            .collect();
        found.sort_by_key(|f| match f {
            PortFlow::Lost { .. } => 0,
            PortFlow::Serving { .. } => 1,
            PortFlow::AdHoc { .. } => 2,
        });
        found.into_iter().next()
    }

    /// Test seam (#1035): hand a gated test the hub's answer, then wait until
    /// it lets the pass commit. The gate's lock is a statement temporary,
    /// never held across the await.
    #[cfg(test)]
    async fn park_at_test_gate(&self, result: &Result<String, String>) {
        let gate = self.inner.gate.lock().unwrap().take();
        if let Some(gate) = gate {
            let _ = gate.reached.send(result.clone());
            let _ = gate.release.await;
        }
    }
}

/// #1685 — the state a declared route reports for the flow the hub holds for
/// it. `active` is the bound listener and nothing less. A flow whose session
/// failed reads `backoff` WITH its flow id (the tunnel session, not flow
/// creation, is what is being retried), carrying the last error, the
/// consecutive failures and the countdown to the next attempt; a flow still
/// on its first attempt — or between a healthy session's end and its
/// re-open — reads `pending` with its flow id ("connecting").
fn route_state_for_flow(flow_id: &str, report: &FlowReport) -> RouteState {
    if let Some(reason) = &report.fatal {
        return RouteState::Failed {
            reason: reason.clone(),
        };
    }
    if report.listening {
        return RouteState::Active {
            flow_id: flow_id.to_string(),
        };
    }
    if report.failures > 0 || report.last_error.is_some() {
        return RouteState::Backoff {
            next_retry_secs: report.next_retry_in.map(|d| d.as_secs()).unwrap_or(0),
            last_error: report.last_error.clone().unwrap_or_default(),
            flow_id: Some(flow_id.to_string()),
            attempts: report.failures,
        };
    }
    RouteState::Pending {
        flow_id: Some(flow_id.to_string()),
    }
}

/// The shape checks shared by `add` and `replace`. They mirror what the hub
/// enforces at create time, so a bad route fails HERE (once, with a clear
/// message) instead of silently backing off forever.
fn validate_shape(route: &RouteDescriptor) -> Result<(), String> {
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
    Ok(())
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
            org: None,
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
    async fn secondary_org_route_parks_as_failed() {
        let mut acme = desc("acme-pg", 41051, true);
        acme.org = Some("acme".into());
        let mut prim = desc("prim", 41052, true);
        prim.org = Some("primary".into()); // explicit primary is supervised normally
        let (r, _dir) = reconciler(vec![acme, prim]);
        r.reconcile_pass().await;
        let rows = r.list();
        let acme_row = rows.iter().find(|i| i.route.id == "acme-pg").unwrap();
        assert!(
            matches!(&acme_row.state, RouteState::Failed { reason }
                if reason.contains("acme") && reason.contains("primary only")),
            "got {:?}",
            acme_row.state
        );
        let prim_row = rows.iter().find(|i| i.route.id == "prim").unwrap();
        assert!(
            !matches!(&prim_row.state, RouteState::Failed { .. }),
            "explicit-primary route must not park: {:?}",
            prim_row.state
        );
    }

    #[tokio::test]
    async fn list_maps_disabled_and_pending() {
        let (r, _dir) = reconciler(vec![desc("a", 1001, true), desc("b", 1002, false)]);
        let rows = r.list();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].route.id, "a");
        assert_eq!(rows[0].state, RouteState::Pending { flow_id: None });
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
        assert_eq!(r.list()[0].state, RouteState::Pending { flow_id: None });

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

    /// `N` distinct free loopback ports, for routes that must really bind.
    /// The listeners are held together and dropped together, so no two
    /// answers can be the same port.
    fn free_ports<const N: usize>() -> [u16; N] {
        let held: Vec<std::net::TcpListener> = (0..N)
            .map(|_| std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap())
            .collect();
        let ports: Vec<u16> = held
            .iter()
            .map(|l| l.local_addr().unwrap().port())
            .collect();
        ports.try_into().unwrap()
    }

    /// The flow id behind a route that HAS one, or a panic naming its state.
    /// These tests run without an agent WS, so a route's flow never comes
    /// up: it reads `pending` WITH its flow id ("connecting", #1685), never
    /// `active` — that word is reserved for a bound listener.
    fn route_flow(r: &RouteReconciler, id: &str) -> String {
        let rows = r.list();
        let row = rows.iter().find(|i| i.route.id == id).unwrap();
        assert!(
            !matches!(row.state, RouteState::Active { .. }),
            "no listener can be bound without a WS: {:?}",
            row.state
        );
        match row.state.flow_id() {
            Some(flow_id) => flow_id.to_string(),
            None => panic!("route {id} has no flow: {:?}", row.state),
        }
    }

    /// FR-84 D1 — editing a live route's port: the replacement is ONE config
    /// write, and the pass that applies it retires the old flow and brings
    /// the route back on a flow toward the new port, with exactly one flow
    /// in the hub. #1035: the verb itself writes only the declared side;
    /// with the reconcile task running it answers after that pass
    /// (`a_verb_answers_after_the_pass_that_applies_it`), and this test,
    /// which runs its passes by hand, runs that pass itself.
    #[tokio::test]
    async fn replace_while_live_moves_the_flow_to_the_new_port() {
        let [p1, p2] = free_ports();
        let (r, _dir) = reconciler(vec![desc("a", p1, true)]);
        r.reconcile_pass().await;
        let f1 = route_flow(&r, "a");
        assert!(r.inner.hub.has_flow(&f1));
        assert_eq!(
            r.inner.hub.flows_snapshot()[0].local_addr,
            format!("127.0.0.1:{p1}")
        );

        let eff = r.replace(desc("a", p2, true)).await.unwrap();
        assert_eq!(eff.local, p2);
        let on_disk = crate::config::load(&r.inner.config_path)
            .unwrap()
            .tunnel_routes;
        assert_eq!(
            on_disk.len(),
            1,
            "a replace is not a remove+add: {on_disk:?}"
        );
        assert_eq!(
            on_disk[0].local, p2,
            "the replacement is what got persisted"
        );
        // Before the pass the row already reads as the pass will leave it:
        // the old descriptor's flow is not the route's flow.
        assert_eq!(r.list()[0].state, RouteState::Pending { flow_id: None });

        r.reconcile_pass().await;
        assert!(
            !r.inner.hub.has_flow(&f1),
            "the pass that applies the replacement retires the old flow"
        );
        let f2 = route_flow(&r, "a");
        assert_ne!(f1, f2, "a new flow, not the old one revived");
        let flows = r.inner.hub.flows_snapshot();
        assert_eq!(
            flows.len(),
            1,
            "exactly one flow — the old listener did not survive: {flows:?}"
        );
        assert_eq!(flows[0].local_addr, format!("127.0.0.1:{p2}"));
    }

    /// FR-84 D1 — every way a replacement can be invalid leaves the OLD
    /// route running on its old flow and persisted on its old port. This is
    /// the property that rules out implementing the edit as remove+add.
    #[tokio::test]
    async fn replace_with_invalid_keeps_the_old_route_running_and_persisted() {
        let [p1, p2, p3] = free_ports();
        let (r, _dir) = reconciler(vec![desc("a", p1, true), desc("b", p2, true)]);
        r.reconcile_pass().await;
        let f1 = route_flow(&r, "a");

        let mut bad_node = desc("a", p3, true);
        bad_node.node = "nope".into();
        let mut no_remote = desc("a", p3, true);
        no_remote.remote = None;
        let mut socks_with_remote = desc("a", p3, true);
        socks_with_remote.kind = FlowKind::Socks5;
        let clash = desc("a", p2, true); // b's port
        let zero = desc("a", 0, true);
        let cases = [
            ("bad node", bad_node),
            ("forward without remote", no_remote),
            ("socks5 with remote", socks_with_remote),
            ("port clash with another enabled route", clash),
            ("port 0", zero),
        ];
        for (what, bad) in cases {
            let err = r.replace(bad).await.expect_err(what);
            assert!(!err.is_empty(), "{what}: the refusal names a reason");
            assert_eq!(
                route_flow(&r, "a"),
                f1,
                "{what}: the old flow must keep running"
            );
            assert!(
                r.inner.hub.has_flow(&f1),
                "{what}: old flow still in the hub"
            );
            let on_disk = crate::config::load(&r.inner.config_path)
                .unwrap()
                .tunnel_routes;
            assert_eq!(on_disk.len(), 2, "{what}: nothing removed from disk");
            assert_eq!(
                on_disk.iter().find(|x| x.id == "a").unwrap().local,
                p1,
                "{what}: still persisted on the old port"
            );
        }

        // A route's OWN port is not a clash with itself: changing only the
        // remote is a valid edit.
        let mut same_port = desc("a", p1, true);
        same_port.remote = Some("db:6543".into());
        let eff = r.replace(same_port).await.unwrap();
        assert_eq!(eff.remote.as_deref(), Some("db:6543"));
        assert_eq!(
            crate::config::load(&r.inner.config_path)
                .unwrap()
                .tunnel_routes
                .iter()
                .find(|x| x.id == "a")
                .unwrap()
                .remote
                .as_deref(),
            Some("db:6543")
        );
    }

    /// FR-84 D1 — an unknown (or empty) id is an error, and the declared set
    /// is untouched on disk and in memory.
    #[tokio::test]
    async fn replace_unknown_id_errors_and_changes_nothing() {
        let (r, _dir) = reconciler(vec![desc("a", 1001, true)]);
        let err = r.replace(desc("ghost", 1002, true)).await.unwrap_err();
        assert!(err.contains("ghost"), "the refusal names the id: {err}");
        let err = r.replace(desc("", 1002, true)).await.unwrap_err();
        assert!(
            err.contains("id"),
            "an empty id cannot address a route: {err}"
        );
        let on_disk = crate::config::load(&r.inner.config_path)
            .unwrap()
            .tunnel_routes;
        assert_eq!(on_disk.len(), 1);
        assert_eq!(on_disk[0].id, "a");
        assert_eq!(on_disk[0].local, 1001);
        let rows = r.list();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].route.local, 1001);
    }

    /// #1685 — a route whose flow is REGISTERED but whose tunnel session is
    /// not up must not read `active`: nothing listens on its port. In this
    /// test there is no agent WS, so the flow's supervisor parks waiting for
    /// one — the same "flow exists, no listener" shape as a route to an
    /// offline node whose session keeps failing. Before the fix `Live` was
    /// mapped straight to `Active` and the port refused every connection.
    #[tokio::test]
    async fn a_route_whose_session_is_not_up_does_not_read_active() {
        let [p] = free_ports();
        let (r, _dir) = reconciler(vec![desc("a", p, true)]);
        r.reconcile_pass().await;
        let flow_id = {
            let flows = r.inner.hub.flows_snapshot();
            assert_eq!(flows.len(), 1, "the flow was created: {flows:?}");
            flows[0].id.clone()
        };
        assert!(
            std::net::TcpStream::connect(("127.0.0.1", p)).is_err(),
            "nothing listens on the route's port in this test"
        );
        let state = r.list()[0].state.clone();
        assert!(
            !matches!(state, RouteState::Active { .. }),
            "a route with no listener must not read active (flow {flow_id}): {state:?}"
        );
        // The reader still sees WHICH flow is dialing: "connecting (fl-N)".
        assert_eq!(
            state,
            RouteState::Pending {
                flow_id: Some(flow_id)
            }
        );
    }

    /// #1685 — the field shape: a route to an OFFLINE node. Its flow exists,
    /// every session attempt fails (`agent_unavailable`) and the supervisor
    /// backs off; the route reads `backoff` WITH the flow, the attempt count
    /// and the last error — never `active`. When the node comes back the
    /// driver binds the listener, and only then does the route read `active`.
    #[tokio::test]
    async fn a_retrying_session_reads_backoff_with_its_error_then_active_once_bound() {
        let [p] = free_ports();
        let mut route = desc("corp-socks", p, true);
        route.kind = FlowKind::Socks5;
        route.remote = None;
        let (r, _dir) = reconciler(vec![route]);
        r.reconcile_pass().await;
        let flow_id = route_flow(&r, "corp-socks");
        let live = r.inner.hub.flow_live_for_test(&flow_id).unwrap();

        // Two doomed cycles; the supervisor is now sleeping 8 s.
        let offline = "server error during tunnel.open: agent_unavailable: agent is offline";
        live.note_failure("waiting for DC pool to open: deadline has elapsed".into());
        live.note_failure(offline.into());
        live.note_backoff(Duration::from_secs(8));
        let state = r.list()[0].state.clone();
        match &state {
            RouteState::Backoff {
                next_retry_secs,
                last_error,
                flow_id: Some(f),
                attempts,
            } => {
                assert_eq!(f, &flow_id, "the reader can join the Flows table");
                assert_eq!(*attempts, 2);
                assert_eq!(last_error, offline, "the LAST error, not the first");
                assert!(
                    (6..=8).contains(next_retry_secs),
                    "countdown: {next_retry_secs}"
                );
            }
            other => panic!("expected backoff with the flow: {other:?}"),
        }
        // A pre-#1685 reader sees `backoff` too — a true statement — because
        // the tag set did not change; only fields were added.
        let wire = serde_json::to_value(&state).unwrap();
        assert_eq!(wire["state"], "backoff");
        assert_eq!(wire["flow_id"], flow_id);
        assert_eq!(wire["attempts"], 2);

        // The node came online: the driver bound the listener.
        live.mark_listening();
        assert_eq!(r.list()[0].state, RouteState::Active { flow_id });
    }

    /// #1685 — the pure mapping, every branch: fatal wins, then listening,
    /// then a recorded failure, else connecting.
    #[test]
    fn route_state_for_flow_orders_fatal_listening_failure_connecting() {
        let base = FlowReport {
            listening: false,
            fatal: None,
            failures: 0,
            last_error: None,
            next_retry_in: None,
        };
        assert_eq!(
            route_state_for_flow("fl-1", &base),
            RouteState::Pending {
                flow_id: Some("fl-1".into())
            }
        );
        assert_eq!(
            route_state_for_flow(
                "fl-1",
                &FlowReport {
                    listening: true,
                    ..base.clone()
                }
            ),
            RouteState::Active {
                flow_id: "fl-1".into()
            }
        );
        // In flight after a failure: no countdown ⇒ 0, still not active.
        assert_eq!(
            route_state_for_flow(
                "fl-1",
                &FlowReport {
                    failures: 3,
                    last_error: Some("x".into()),
                    ..base.clone()
                }
            ),
            RouteState::Backoff {
                next_retry_secs: 0,
                last_error: "x".into(),
                flow_id: Some("fl-1".into()),
                attempts: 3,
            }
        );
        // Terminal beats everything, even a stale `listening`.
        assert_eq!(
            route_state_for_flow(
                "fl-1",
                &FlowReport {
                    listening: true,
                    fatal: Some("revoked".into()),
                    ..base
                }
            ),
            RouteState::Failed {
                reason: "revoked".into()
            }
        );
    }

    #[tokio::test]
    async fn reenable_clears_terminal_failed() {
        let [p] = free_ports();
        let (r, _dir) = reconciler(vec![desc("a", p, true)]);
        // A pass marked the route Failed at its current generation.
        let generation = r.inner.declared.lock().unwrap().generations["a"];
        r.inner.runtime.lock().unwrap().insert(
            "a".into(),
            RouteRuntime::Failed {
                reason: "revoked".into(),
                generation,
            },
        );
        assert!(matches!(r.list()[0].state, RouteState::Failed { .. }));
        // A pass with no change to the route leaves it Failed (terminal).
        r.reconcile_pass().await;
        assert!(matches!(r.list()[0].state, RouteState::Failed { .. }));
        assert!(hub_flows(&r).is_empty());
        // Disable → Disabled wins over Failed…
        assert!(r.set_enabled("a", false).await.unwrap());
        assert_eq!(r.list()[0].state, RouteState::Disabled);
        // …re-enable → fresh Pending (Failed cleared)…
        assert!(r.set_enabled("a", true).await.unwrap());
        assert_eq!(r.list()[0].state, RouteState::Pending { flow_id: None });
        // …and the pass that applies the enable (#1035: a new generation)
        // builds the route a flow again.
        r.reconcile_pass().await;
        route_flow(&r, "a");
        assert_eq!(creates(&r), 1);
    }

    // ---- #1035: one writer owns the route→flow map --------------------------

    use crate::tunnel::port_holder::Holder;

    /// Poll `cond` every 5 ms until it holds, for at most 5 s. The test
    /// runtime has one thread, so a spawned verb or a flow's supervisor runs
    /// only while the test waits here.
    async fn eventually(what: &str, mut cond: impl FnMut() -> bool) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !cond() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for: {what}"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// Something listens on `port`: a bind of it fails.
    fn port_is_held(port: u16) -> bool {
        std::net::TcpListener::bind(("127.0.0.1", port)).is_err()
    }

    /// The ids of the hub's flows, in order.
    fn hub_flows(r: &RouteReconciler) -> Vec<String> {
        r.inner
            .hub
            .flows_snapshot()
            .into_iter()
            .map(|f| f.id)
            .collect()
    }

    /// The creates the passes have asked the hub for.
    fn creates(r: &RouteReconciler) -> u32 {
        r.inner.creates.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Drop the map's handle to a route's flow, the way #1035's races did:
    /// the hub keeps the flow and its port, and nothing maps it back.
    fn lose_the_handle(r: &RouteReconciler, id: &str) {
        let lost = r.inner.runtime.lock().unwrap().remove(id);
        assert!(
            matches!(lost, Some(RouteRuntime::Live { .. })),
            "route {id} held no flow to lose"
        );
    }

    /// Park the next pass's create after the hub's answer. Returns where that
    /// answer arrives, and the sender that lets the pass commit.
    fn gate_next_create(
        r: &RouteReconciler,
    ) -> (
        tokio::sync::oneshot::Receiver<Result<String, String>>,
        tokio::sync::oneshot::Sender<()>,
    ) {
        let (reached, reached_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release) = tokio::sync::oneshot::channel();
        *r.inner.gate.lock().unwrap() = Some(CreateGate { reached, release });
        (reached_rx, release_tx)
    }

    /// One pass on a task of its own, so it can park at a gate the test holds.
    fn spawn_pass(r: &RouteReconciler) -> tokio::task::JoinHandle<()> {
        let r = r.clone();
        tokio::spawn(async move {
            r.reconcile_pass().await;
        })
    }

    /// #1035, the field case (agent 0.4.189). `route enable` writes the flag,
    /// then awaits the config save under the daemon-wide config lock (0.67 to
    /// 1.47 s on the field host). A pass inside that window creates the
    /// route's flow, and the route must end on THAT flow with no second
    /// create. Before the fix the verb then inserted a fresh entry over the
    /// pass's `Live`: the hub kept the flow and its port, and every later
    /// create read `backoff … 10048` until the daemon restarted.
    #[tokio::test]
    async fn a_reenable_that_a_pass_overtakes_keeps_the_passs_flow() {
        let [p] = free_ports();
        let (r, _dir) = reconciler(vec![desc("a", p, false)]);
        let held = r.inner.cfg_lock.lock().await;
        let enable = tokio::spawn({
            let r = r.clone();
            async move { r.set_enabled("a", true).await }
        });
        eventually("the enable is declared", || r.list()[0].route.enabled).await;
        r.reconcile_pass().await;
        let f1 = route_flow(&r, "a");
        eventually("the pass's flow holds its port", || port_is_held(p)).await;
        drop(held);
        assert_eq!(enable.await.unwrap(), Ok(true));
        r.reconcile_pass().await;

        let state = r.list()[0].state.clone();
        assert_eq!(
            state.flow_id(),
            Some(f1.as_str()),
            "the route lost the pass's flow: {state:?}"
        );
        assert_eq!(hub_flows(&r), vec![f1], "one flow, and the route holds it");
        assert_eq!(creates(&r), 1, "a second create ran");
    }

    /// #1035 — enabling a route that is already enabled and live keeps its
    /// flow, with no race at all. Before the fix the verb put a fresh entry
    /// over `Live`, and the next create found the port held by the flow the
    /// route had just lost.
    #[tokio::test]
    async fn an_enable_of_a_live_route_keeps_its_flow() {
        let [p] = free_ports();
        let (r, _dir) = reconciler(vec![desc("a", p, true)]);
        r.reconcile_pass().await;
        let f1 = route_flow(&r, "a");
        eventually("the flow holds its port", || port_is_held(p)).await;
        assert!(r.set_enabled("a", true).await.unwrap());
        r.reconcile_pass().await;

        let state = r.list()[0].state.clone();
        assert_eq!(
            state.flow_id(),
            Some(f1.as_str()),
            "the enable cost the route its flow: {state:?}"
        );
        assert_eq!(hub_flows(&r), vec![f1]);
        assert_eq!(creates(&r), 1, "a second create ran");
    }

    /// #1035 — a disable landing inside a create's await. Once the pass is
    /// done the map and the hub agree: a disabled route has no flow. Before
    /// the fix the verb removed a `Pending` entry and stopped nothing, and the
    /// pass then recorded `Live` for a disabled route.
    #[tokio::test]
    async fn a_disable_inside_a_create_leaves_no_flow() {
        let [p] = free_ports();
        let (r, _dir) = reconciler(vec![desc("a", p, true)]);
        let (reached, release) = gate_next_create(&r);
        let pass = spawn_pass(&r);
        let f1 = reached.await.unwrap().expect("the hub created the flow");
        assert!(r.set_enabled("a", false).await.unwrap());
        release.send(()).unwrap();
        pass.await.unwrap();

        assert_eq!(r.list()[0].state, RouteState::Disabled);
        assert!(
            !r.inner.hub.has_flow(&f1),
            "the pass kept a flow for a disabled route"
        );
        assert_eq!(hub_flows(&r), Vec::<String>::new());
        assert!(
            !matches!(
                r.inner.runtime.lock().unwrap().get("a"),
                Some(RouteRuntime::Live { .. })
            ),
            "the map holds a flow for a disabled route"
        );

        // Enabled again, the route comes back on exactly one flow: its own.
        assert!(r.set_enabled("a", true).await.unwrap());
        eventually("the stopped flow let go of the port", || !port_is_held(p)).await;
        r.reconcile_pass().await;
        let f2 = route_flow(&r, "a");
        assert_eq!(hub_flows(&r), vec![f2]);
    }

    /// #1035 — a replace landing inside a create's await: the route ends on a
    /// flow for the new descriptor only. Before the fix the pass recorded
    /// `Live` for the flow it had built from the OLD descriptor, and every
    /// later pass saw a live flow and left the route serving its old shape.
    #[tokio::test]
    async fn a_replace_inside_a_create_keeps_only_the_new_descriptors_flow() {
        let [p1, p2] = free_ports();
        let (r, _dir) = reconciler(vec![desc("a", p1, true)]);
        let (reached, release) = gate_next_create(&r);
        let pass = spawn_pass(&r);
        let f1 = reached.await.unwrap().expect("the hub created the flow");
        r.replace(desc("a", p2, true)).await.unwrap();
        release.send(()).unwrap();
        pass.await.unwrap();
        r.reconcile_pass().await;

        let f2 = route_flow(&r, "a");
        assert_ne!(f1, f2, "the route kept the flow of its old descriptor");
        let flows = r.inner.hub.flows_snapshot();
        assert_eq!(
            flows.len(),
            1,
            "a flow for the new descriptor only: {flows:?}"
        );
        assert_eq!(flows[0].id, f2);
        assert_eq!(flows[0].local_addr, format!("127.0.0.1:{p2}"));
    }

    /// #1035 — a flow the map lost, of a route since removed: the next pass
    /// stops it. Before the fix `route rm` stopped only a flow its entry held,
    /// so a lost flow outlived its route and kept the port until a restart.
    #[tokio::test]
    async fn a_lost_flow_of_a_removed_route_is_stopped() {
        let [p] = free_ports();
        let (r, _dir) = reconciler(vec![desc("a", p, true)]);
        r.reconcile_pass().await;
        let f1 = route_flow(&r, "a");
        lose_the_handle(&r, "a");
        assert!(r.remove("a").await.unwrap());
        r.reconcile_pass().await;

        assert!(
            !r.inner.hub.has_flow(&f1),
            "the lost flow outlived its route"
        );
        assert_eq!(hub_flows(&r), Vec::<String>::new());
    }

    /// #1035 — the same for a route since disabled: `route disable` could not
    /// stop a lost flow either.
    #[tokio::test]
    async fn a_lost_flow_of_a_disabled_route_is_stopped() {
        let [p] = free_ports();
        let (r, _dir) = reconciler(vec![desc("a", p, true)]);
        r.reconcile_pass().await;
        let f1 = route_flow(&r, "a");
        lose_the_handle(&r, "a");
        assert!(r.set_enabled("a", false).await.unwrap());
        r.reconcile_pass().await;

        assert_eq!(r.list()[0].state, RouteState::Disabled);
        assert!(
            !r.inner.hub.has_flow(&f1),
            "the lost flow outlived its route's disable"
        );
        assert_eq!(hub_flows(&r), Vec::<String>::new());
    }

    /// #1035 — a lost flow built from exactly the declared descriptor is
    /// adopted, not rebuilt: one pass, no second create, no 10048. Before the
    /// fix the pass found no entry, created again, and the create found the
    /// port held by the flow the route had lost.
    #[tokio::test]
    async fn a_lost_flow_matching_its_route_is_adopted() {
        let [p] = free_ports();
        let (r, _dir) = reconciler(vec![desc("a", p, true)]);
        r.reconcile_pass().await;
        let f1 = route_flow(&r, "a");
        eventually("the flow holds its port", || port_is_held(p)).await;
        lose_the_handle(&r, "a");
        r.reconcile_pass().await;

        let state = r.list()[0].state.clone();
        assert_eq!(
            state.flow_id(),
            Some(f1.as_str()),
            "the lost flow was not adopted: {state:?}"
        );
        assert_eq!(hub_flows(&r), vec![f1]);
        assert_eq!(creates(&r), 1, "a second create ran");
    }

    /// #1035 — a lost flow built from an older descriptor of its route is
    /// stopped, never adopted, and the route ends on one flow for its new
    /// shape.
    #[tokio::test]
    async fn a_lost_flow_of_an_older_descriptor_is_stopped_not_adopted() {
        let [p1, p2] = free_ports();
        let (r, _dir) = reconciler(vec![desc("a", p1, true)]);
        r.reconcile_pass().await;
        let f1 = route_flow(&r, "a");
        lose_the_handle(&r, "a");
        r.replace(desc("a", p2, true)).await.unwrap();
        r.reconcile_pass().await;

        let f2 = route_flow(&r, "a");
        assert_ne!(f1, f2);
        let flows = r.inner.hub.flows_snapshot();
        assert_eq!(
            flows.len(),
            1,
            "the lost flow of the old descriptor survived: {flows:?}"
        );
        assert_eq!(flows[0].id, f2);
        assert_eq!(flows[0].local_addr, format!("127.0.0.1:{p2}"));
    }

    /// #1035 — the port-holder line names the lost flow and its route. Before
    /// the fix it read "a listener no flow owns": a flow did own the listener,
    /// and the route had lost the flow.
    #[tokio::test]
    async fn the_port_holder_line_names_the_lost_flow_and_its_route() {
        let [p] = free_ports();
        let mut route = desc("corp-socks", p, true);
        route.kind = FlowKind::Socks5;
        route.remote = None;
        let (r, _dir) = reconciler(vec![route]);
        r.reconcile_pass().await;
        let f1 = route_flow(&r, "corp-socks");
        lose_the_handle(&r, "corp-socks");

        let line = r.holder_line(
            p,
            &Holder::ThisDaemon {
                pid: std::process::id(),
            },
        );
        assert!(
            line.contains(&f1) && line.contains("corp-socks") && line.contains("lost"),
            "{line}"
        );
        assert!(!line.contains("no flow owns"), "{line}");
    }

    /// #1035 — the line tells this daemon's other flows apart: another
    /// route's flow serving on the port (two enabled routes declare it), and
    /// an ad-hoc flow no route declares. Any other holder reads as before.
    #[tokio::test]
    async fn the_port_holder_line_names_a_serving_or_an_ad_hoc_flow() {
        let [p, q] = free_ports();
        let (r, _dir) = reconciler(vec![desc("a", p, true)]);
        r.reconcile_pass().await;
        let fa = route_flow(&r, "a");
        let me = Holder::ThisDaemon {
            pid: std::process::id(),
        };

        let serving = r.holder_line(p, &me);
        assert!(
            serving.contains(&fa) && serving.contains("serves route a"),
            "{serving}"
        );
        assert!(!serving.contains("lost"), "{serving}");

        let ad_hoc = r
            .inner
            .hub
            .create_forward("aabbccddeeff001122334455", q, "db:5432", "", "")
            .await
            .unwrap();
        let line = r.holder_line(q, &me);
        assert!(
            line.contains(&ad_hoc) && line.contains("no route"),
            "{line}"
        );

        let foreign = Holder::Alive {
            pid: 11,
            image: Some("proxy.exe".into()),
        };
        assert_eq!(
            r.holder_line(p, &foreign),
            crate::tunnel::port_holder::describe(&foreign)
        );
    }

    /// #1035 — with one writer the verbs no longer touch the map, and their
    /// answers still reflect the world: each verb answers after the pass that
    /// applies its change. `route rm` promises to stop the route's flow, and
    /// the fleet's watchdogs read `flows` right after a verb.
    #[tokio::test]
    async fn a_verb_answers_after_the_pass_that_applies_it() {
        let [p1, p2] = free_ports();
        let (r, _dir) = reconciler(vec![]);
        // Kept alive for the whole test: the task stops when it reads `true`.
        let (_stop, stop_rx) = watch::channel(false);
        r.spawn(stop_rx);

        r.add(desc("a", p1, true)).await.unwrap();
        let f1 = route_flow(&r, "a");

        r.replace(desc("a", p2, true)).await.unwrap();
        assert!(
            !r.inner.hub.has_flow(&f1),
            "replace answered before the old flow was stopped"
        );
        let f2 = route_flow(&r, "a");
        let flows = r.inner.hub.flows_snapshot();
        assert_eq!(flows.len(), 1, "{flows:?}");
        assert_eq!(flows[0].local_addr, format!("127.0.0.1:{p2}"));

        assert!(r.set_enabled("a", false).await.unwrap());
        assert!(
            !r.inner.hub.has_flow(&f2),
            "disable answered before the flow was stopped"
        );
        eventually("the stopped flow let go of the port", || !port_is_held(p2)).await;
        assert!(r.set_enabled("a", true).await.unwrap());
        let f3 = route_flow(&r, "a");
        assert_eq!(hub_flows(&r), vec![f3]);

        assert!(r.remove("a").await.unwrap());
        assert_eq!(
            hub_flows(&r),
            Vec::<String>::new(),
            "rm answered before the route's flow was stopped"
        );
    }

    /// #1035 — a replace that keeps the local port (FR-84 D1's edit of a
    /// route's target) moves the route onto the new descriptor's flow in the
    /// pass that applies it. That pass stops the old flow and creates the new
    /// one on the same port, and a stopped flow lets go of its port only once
    /// the runtime drops its task. A create that does not wait for that finds
    /// the route's own old listener and reads `backoff … 10048` until a later
    /// pass: the field signature of the bug this fixes.
    #[tokio::test]
    async fn a_replace_that_keeps_the_port_moves_the_route_in_one_pass() {
        let [p] = free_ports();
        let (r, _dir) = reconciler(vec![desc("a", p, true)]);
        r.reconcile_pass().await;
        let f1 = route_flow(&r, "a");
        eventually("the flow holds its port", || port_is_held(p)).await;

        let mut edited = desc("a", p, true);
        edited.remote = Some("db2:5432".into());
        r.replace(edited).await.unwrap();
        r.reconcile_pass().await;

        let state = r.list()[0].state.clone();
        let f2 = state
            .flow_id()
            .unwrap_or_else(|| panic!("the replace left the route without a flow: {state:?}"))
            .to_string();
        assert_ne!(f1, f2, "the route kept the flow of its old target");
        let flows = r.inner.hub.flows_snapshot();
        assert_eq!(flows.len(), 1, "one flow, the new target's: {flows:?}");
        assert_eq!(flows[0].id, f2);
        assert_eq!(flows[0].target.as_deref(), Some("db2:5432"));
        assert_eq!(creates(&r), 2, "the new target's flow took one create");
    }
}
