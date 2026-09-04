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

use std::sync::Arc;

use axum::Router;
use roomler_ai_config::Settings;
use roomler_core::{Core, IndexSet, Module, Role, WsHandler, WsHandlerSpec, graph};
use tracing::{info, warn};

/// The module ids whose crates exist — the switches that are effective.
pub const EXTRACTED: &[&str] = &[
    #[cfg(feature = "saas")]
    "saas",
    #[cfg(feature = "chat")]
    "chat",
];

/// The modules this build links, initialised — `None` where the operator
/// switched one off.
#[derive(Clone, Default)]
pub struct Modules {
    #[cfg(feature = "saas")]
    pub saas: Option<roomler_ai_mod_saas::SaasState>,
    #[cfg(feature = "chat")]
    pub chat: Option<roomler_ai_mod_chat::ChatState>,
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
        sets
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
