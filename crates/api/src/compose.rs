// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-69 — the host's composition: which module crates this build links,
//! which of them the operator switched on, and how they are mounted.
//!
//! Static on purpose (spec D4): every module is a concrete type behind a
//! Cargo feature and a `#[cfg]` block here, so a module that forgets its
//! routes, indexes or hooks does not compile, and the set of modules a
//! binary carries is readable in one place. The runtime switch
//! (`[modules] <id> = false`) is the per-module kill switch during the roll
//! that introduced it: the module still links, but nothing of it is mounted.
//!
//! Until every pillar has been extracted, the host's own routes and state
//! carry the rest; `EXTRACTED` says which switches actually do something, and
//! `init` says so at boot for the ones that do not yet.
//!
//! P4 added the three surfaces a stateful module needs beyond routes:
//! [`Modules::ws_closed`] (a socket closed — every handler for that role is
//! told), [`Modules::run_startup_jobs`] (the host's startup lease drives the
//! modules' leader-gated jobs) and [`Modules::shutdown`] (reverse composition
//! order, ahead of the host's own cleanup).

use std::sync::Arc;

use axum::Router;
use roomler_ai_config::Settings;
use roomler_core::{
    Core, IndexSet, Job, Module, Role, WsCtx, WsHandler, WsHandlerSpec, graph, job::Cadence,
};
use tracing::{error, info, warn};

/// The module ids whose crates exist — the switches that are effective.
pub const EXTRACTED: &[&str] = &[
    #[cfg(feature = "saas")]
    "saas",
    #[cfg(feature = "chat")]
    "chat",
    #[cfg(feature = "conference")]
    "conference",
];

/// The modules this build links, initialised — `None` where the operator
/// switched one off.
#[derive(Clone, Default)]
pub struct Modules {
    #[cfg(feature = "saas")]
    pub saas: Option<roomler_ai_mod_saas::SaasState>,
    #[cfg(feature = "chat")]
    pub chat: Option<roomler_ai_mod_chat::ChatState>,
    #[cfg(feature = "conference")]
    pub conference: Option<roomler_ai_mod_conference::ConferenceState>,
    /// Every mounted module's WebSocket namespace handlers, collected at
    /// init so the socket dispatch can look one up per message.
    ws: Vec<WsHandlerSpec>,
}

impl Modules {
    /// Initialise every linked module the settings do not switch off, in
    /// composition order (`graph::MODULES`). Logs each switch that is off
    /// for a module that is not extracted yet — that switch unmounts nothing.
    pub async fn init(core: Core, settings: &Settings) -> anyhow::Result<Self> {
        for id in settings.modules.switched_off() {
            if !EXTRACTED.contains(&id) {
                warn!(
                    module = id,
                    "[modules] switch is OFF in config but that module is not yet extracted — \
                     nothing is unmounted (FR-69)"
                );
            }
        }
        debug_assert!(
            EXTRACTED.iter().all(|id| graph::MODULES.contains(id)),
            "every extracted module must be in the graph"
        );

        #[allow(unused_mut)]
        let mut modules = Self::default();

        #[cfg(feature = "saas")]
        {
            modules.saas =
                init_one::<roomler_ai_mod_saas::SaasState>(core.clone(), settings).await?;
            if let Some(m) = &modules.saas {
                modules.ws.extend(m.ws().handlers);
            }
        }
        #[cfg(feature = "chat")]
        {
            modules.chat =
                init_one::<roomler_ai_mod_chat::ChatState>(core.clone(), settings).await?;
            if let Some(m) = &modules.chat {
                modules.ws.extend(m.ws().handlers);
            }
        }
        #[cfg(feature = "conference")]
        {
            modules.conference =
                init_one::<roomler_ai_mod_conference::ConferenceState>(core.clone(), settings)
                    .await?;
            if let Some(m) = &modules.conference {
                modules.ws.extend(m.ws().handlers);
            }
        }

        let _ = core;
        Ok(modules)
    }

    /// The module ids actually mounted on this pod.
    pub fn mounted(&self) -> Vec<&'static str> {
        #[allow(unused_mut)]
        let mut ids = Vec::new();
        #[cfg(feature = "saas")]
        if self.saas.is_some() {
            ids.push("saas");
        }
        #[cfg(feature = "chat")]
        if self.chat.is_some() {
            ids.push("chat");
        }
        #[cfg(feature = "conference")]
        if self.conference.is_some() {
            ids.push("conference");
        }
        ids
    }

    /// Mount every module's governed routes onto the `/api` router.
    pub fn mount<S>(&self, api: Router<S>) -> Router<S>
    where
        S: Clone + Send + Sync + 'static,
    {
        #[allow(unused_mut)]
        let mut api = api;
        #[cfg(feature = "saas")]
        if let Some(saas) = &self.saas {
            api = api.merge(saas.routes().with_state(()));
        }
        #[cfg(feature = "chat")]
        if let Some(chat) = &self.chat {
            api = api.merge(chat.routes().with_state(()));
        }
        #[cfg(feature = "conference")]
        if let Some(conference) = &self.conference {
            api = api.merge(conference.routes().with_state(()));
        }
        api
    }

    /// Mount every module's ungoverned routes onto the root router (the
    /// ones with their own authentication, like the Stripe webhook).
    pub fn mount_unlimited<S>(&self, root: Router<S>) -> Router<S>
    where
        S: Clone + Send + Sync + 'static,
    {
        #[allow(unused_mut)]
        let mut root = root;
        #[cfg(feature = "saas")]
        if let Some(saas) = &self.saas {
            root = root.merge(saas.unlimited_routes().with_state(()));
        }
        #[cfg(feature = "chat")]
        if let Some(chat) = &self.chat {
            root = root.merge(chat.unlimited_routes().with_state(()));
        }
        #[cfg(feature = "conference")]
        if let Some(conference) = &self.conference {
            root = root.merge(conference.unlimited_routes().with_state(()));
        }
        root
    }

    /// Every mounted module's index sets, in composition order. Applied by
    /// the host after the core plan; snapshotted by the composition test.
    pub fn index_sets(&self) -> Vec<IndexSet> {
        #[allow(unused_mut)]
        let mut sets = Vec::new();
        #[cfg(feature = "saas")]
        if let Some(saas) = &self.saas {
            sets.extend(saas.indexes());
        }
        #[cfg(feature = "chat")]
        if let Some(chat) = &self.chat {
            sets.extend(chat.indexes());
        }
        #[cfg(feature = "conference")]
        if let Some(conference) = &self.conference {
            sets.extend(conference.indexes());
        }
        sets
    }

    /// Every mounted module's declared jobs, in composition order.
    fn jobs(&self) -> Vec<Job> {
        #[allow(unused_mut)]
        let mut jobs = Vec::new();
        #[cfg(feature = "saas")]
        if let Some(saas) = &self.saas {
            jobs.extend(saas.jobs());
        }
        #[cfg(feature = "chat")]
        if let Some(chat) = &self.chat {
            jobs.extend(chat.jobs());
        }
        #[cfg(feature = "conference")]
        if let Some(conference) = &self.conference {
            jobs.extend(conference.jobs());
        }
        jobs
    }

    /// Run every mounted module's startup jobs, in composition order — the
    /// leader-gated ones only when this pod holds the startup lease. A job
    /// that fails is logged and the next one runs: maintenance must never
    /// keep a pod from serving. Periodic jobs are declared but not scheduled
    /// by the host yet (no module needs one); a stray declaration is logged
    /// so it cannot be silently ignored.
    pub async fn run_startup_jobs(&self, leader: bool) {
        for job in self.jobs() {
            match job.cadence {
                Cadence::AtStartup => {
                    if job.leader_gated && !leader {
                        info!(
                            job = job.name,
                            "leader-gated startup job skipped — the lease is held elsewhere"
                        );
                        continue;
                    }
                    match (job.run)().await {
                        Ok(()) => info!(job = job.name, "startup job done"),
                        Err(e) => error!(job = job.name, %e, "startup job failed"),
                    }
                }
                Cadence::Every(_) => warn!(
                    job = job.name,
                    "periodic module jobs are not scheduled by the host yet (FR-69) — declared, not run"
                ),
            }
        }
    }

    /// The handler a module registered for this role and message type, if
    /// any. The namespace is the message type's prefix before the first
    /// `:` (`typing:start` → `typing`), matching how the wire groups.
    pub fn ws_handler(&self, role: Role, msg_type: &str) -> Option<Arc<dyn WsHandler>> {
        let namespace = msg_type.split(':').next().unwrap_or(msg_type);
        self.ws
            .iter()
            .find(|spec| spec.role == role && spec.namespace == namespace)
            .map(|spec| spec.handler.clone())
    }

    /// A socket closed: tell every handler registered for its role, in
    /// registration order, after the host's own cleanup.
    pub async fn ws_closed(&self, ctx: &WsCtx) {
        for spec in self.ws.iter().filter(|spec| spec.role == ctx.role) {
            spec.handler.closed(ctx).await;
        }
    }

    /// Orderly stop of every mounted module, in REVERSE composition order
    /// (a module stops before the ones it depends on).
    pub async fn shutdown(&self) {
        #[cfg(feature = "conference")]
        if let Some(conference) = &self.conference {
            conference.shutdown().await;
        }
        #[cfg(feature = "chat")]
        if let Some(chat) = &self.chat {
            chat.shutdown().await;
        }
        #[cfg(feature = "saas")]
        if let Some(saas) = &self.saas {
            saas.shutdown().await;
        }
    }

    /// Conference's orphaned-call closer, for the host's stats rollup loop.
    /// `(0, 0)` when the module is not mounted — there are no call sessions
    /// to close then.
    pub async fn close_orphaned_call_state(&self) -> (u64, u64) {
        #[cfg(feature = "conference")]
        if let Some(conference) = &self.conference {
            return roomler_ai_mod_conference::maintenance::close_orphaned_call_state(conference)
                .await;
        }
        (0, 0)
    }

    /// The live media gauges for the cluster status snapshot:
    /// `(per-room rows, participants_total, consumers_total)`. Empty when
    /// the module is not mounted.
    pub fn media_gauges(&self) -> (Vec<serde_json::Value>, usize, usize) {
        #[cfg(feature = "conference")]
        if let Some(conference) = &self.conference {
            let g = conference.media_gauges();
            return (g.rooms, g.participants_total, g.consumers_total);
        }
        (Vec::new(), 0, 0)
    }
}

/// Initialise one module unless its switch is off.
#[allow(dead_code)]
async fn init_one<M: Module>(core: Core, settings: &Settings) -> anyhow::Result<Option<M>> {
    if !M::enabled(settings) {
        info!(
            module = M::ID,
            "module switched off by config — not mounted"
        );
        return Ok(None);
    }
    let module = M::init(core, settings).await?;
    info!(module = M::ID, "module mounted");
    Ok(Some(module))
}
