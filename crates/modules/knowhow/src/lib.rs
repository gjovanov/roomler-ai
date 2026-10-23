// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! `knowhow` — the organization's map, the server side (FR-90 P4,
//! `docs/roomler-hive-design.md` §9, `docs/knowhow.md`).
//!
//! What exists — machines, repositories and their checkouts, databases,
//! clusters, services — where it lives, and how a session reaches each. Its
//! facts are meant to be verified by probes rather than remembered (P4-5),
//! and every one says where it came from.
//!
//! ⚠️ **A map, not a gate.** Knowhow grants nothing, and nothing that
//! decides consults it: the vault decides every credential and every egress,
//! the network every route, the device its own gates. A wrong fact misleads a
//! session toward a host; it never opens anything.
//!
//! P4-1 builds the store: entities and the edges between them, each unique
//! among the live of its organization ([`model`]), every write a
//! compare-and-set on its `rev` appended to `knowhow_history` ([`dao`]), and
//! the people's routes ([`routes`]) — search, a card, a neighbourhood, create,
//! update, archive — read with `KNOWHOW_VIEW` and written with `KNOWHOW_EDIT`
//! (decisions 36 and 37). Sync from fleet and network (P4-2), device scans
//! (P4-3), a session's reads and proposals (P4-4), probes (P4-5) and
//! `roomler_connect` (P4-6) are built on it.
//!
//! The module's switch defaults OFF (`[modules] knowhow`), and a
//! `knowhow.tenants` list both switches it on and limits it to the
//! organizations listed, as `vault.tenants` does. Clearing the list is the
//! kill switch.

use std::sync::Arc;

use axum::{
    Router,
    extract::FromRef,
    routing::{get, patch, post},
};
use roomler_ai_config::Settings;
use roomler_ai_db::indexes::{IndexSet, index, index_ttl, index_unique_partial};
use roomler_core::{AgentSocketHooks, Capabilities, Core, Hooks, Job, Module, TenantCtx};

pub mod dao;
pub mod intake;
pub mod model;
pub mod routes;
pub mod scan;
pub mod scope;
pub mod sync;
pub mod view;
mod write;

/// The module's state: the core, and the map's three collections.
#[derive(Clone)]
pub struct KnowhowState {
    pub core: Core,
    pub entities: Arc<dao::EntityDao>,
    pub edges: Arc<dao::EdgeDao>,
    pub history: Arc<dao::HistoryDao>,
    /// P4-2a — the fleet's devices, read to mirror them (the shared
    /// stateless DAO, as the vault reads them: no module edge).
    pub agents: Arc<roomler_ai_services::dao::agent::AgentDao>,
    /// P4-2b — the network's word on them: each device's live overlay node,
    /// and the organization's network, read through the same shared DAOs.
    pub overlay_nodes: Arc<roomler_ai_services::dao::overlay_node::OverlayNodeDao>,
    pub overlay_networks: Arc<roomler_ai_services::dao::overlay_network::OverlayNetworkDao>,
    /// P4-3a — the scans asked of devices, and this pod's connections of the
    /// devices that can scan, each told what was asked of it.
    pub scans: Arc<dao::ScanDao>,
    pub links: Arc<scan::Links>,
    /// The organizations whose map it keeps (`knowhow.tenants`).
    pub scope: Arc<scope::TenantScope>,
}

impl std::ops::Deref for KnowhowState {
    type Target = Core;

    fn deref(&self) -> &Core {
        &self.core
    }
}

/// `State<Core>` in this module's handlers, and the core extractors.
impl FromRef<KnowhowState> for Core {
    fn from_ref(state: &KnowhowState) -> Self {
        state.core.clone()
    }
}

impl Module for KnowhowState {
    const ID: &'static str = "knowhow";

    /// No module state, and no module edge yet. P4-2 reads the fleet's
    /// devices and the network's overlay through the shared stateless DAOs
    /// and hears of devices through core's hooks and agent socket, so it
    /// calls no module (FR-69 rule 5: a stateless dependency is no edge).
    /// `knowhow → fleet` comes with the first use of fleet's Hub (P4-3a,
    /// asking a device to scan), and `knowhow → hive` with P4-4's session
    /// reads; `knowhow → network` only with a step that needs the network's
    /// live state.
    type Deps = ();

    async fn init(core: Core, settings: &Settings, _deps: ()) -> anyhow::Result<Self> {
        let db = &core.db;
        let scope = scope::TenantScope::parse(&settings.knowhow.tenants);
        scope.log();
        let state = Self {
            entities: Arc::new(dao::EntityDao::new(db)),
            edges: Arc::new(dao::EdgeDao::new(db)),
            history: Arc::new(dao::HistoryDao::new(db)),
            agents: Arc::new(roomler_ai_services::dao::agent::AgentDao::new(db)),
            overlay_nodes: Arc::new(roomler_ai_services::dao::overlay_node::OverlayNodeDao::new(
                db,
            )),
            overlay_networks: Arc::new(
                roomler_ai_services::dao::overlay_network::OverlayNetworkDao::new(db),
            ),
            scans: Arc::new(dao::ScanDao::new(db)),
            links: Arc::new(scan::Links::default()),
            scope: Arc::new(scope),
            core,
        };
        // P4-2a — a device's hello mirrors it (an enrolment's first, every
        // reconnect); the agent socket is fleet's, the registry core's.
        // P4-3a — and links its connection for scans, whose answers
        // (`rc:knowhow.scanned`) `ClientMsg::namespace()` dispatches here.
        state.core.agent_socket.register(
            Self::ID,
            AgentSocketHooks {
                handler: Some(Arc::new(scan::ScanAnswers {
                    state: state.clone(),
                })),
                lifecycle: Some(Arc::new(scan::SocketHooks {
                    state: state.clone(),
                })),
            },
        );
        // And every pod reconciles, for what no hook carries.
        let secs = match settings.knowhow.sync_secs {
            0 => sync::DEFAULT_SYNC_SECS,
            secs => secs,
        };
        sync::spawn_reconciler(state.clone(), secs);
        // P4-3a — and sweeps for the scans asked of its devices.
        let sweep = match settings.knowhow.scan_sweep_secs {
            0 => scan::DEFAULT_SCAN_SWEEP_SECS,
            secs => secs,
        };
        scan::spawn_sweeper(state.clone(), sweep);
        Ok(state)
    }

    /// The module switch, or a `knowhow.tenants` list.
    fn enabled(settings: &Settings) -> bool {
        settings.knowhow_on()
    }

    fn capabilities(&self, _tenant: &TenantCtx) -> Capabilities {
        Capabilities::enabled(Self::ID)
    }

    fn routes(&self) -> Router {
        let entity = Router::new()
            .route("/", get(routes::lookup).post(routes::create_entity))
            .route(
                "/{entity_id}",
                get(routes::get_entity)
                    .patch(routes::update_entity)
                    .delete(routes::archive_entity),
            );
        let edge = Router::new().route("/", post(routes::create_edge)).route(
            "/{edge_id}",
            patch(routes::update_edge).delete(routes::archive_edge),
        );
        Router::new()
            .route("/tenant/{tenant_id}/knowhow", get(routes::status))
            .route("/tenant/{tenant_id}/knowhow/search", get(routes::search))
            .route("/tenant/{tenant_id}/knowhow/history", get(routes::history))
            .route(
                "/tenant/{tenant_id}/knowhow/map/{entity_id}",
                get(routes::map),
            )
            .route(
                "/tenant/{tenant_id}/knowhow/scan",
                get(scan::list).post(scan::request),
            )
            .nest("/tenant/{tenant_id}/knowhow/entity", entity)
            .nest("/tenant/{tenant_id}/knowhow/edge", edge)
            .with_state(self.clone())
    }

    /// P4-2a — fleet's events reach the map: a rename, an update, a removal.
    /// Last in `HOOK_ORDER`, and never the cascade's refusal.
    fn hooks(&self) -> Hooks {
        Hooks {
            fleet: Some(std::sync::Arc::new(sync::FleetSync {
                state: self.clone(),
            })),
            ..Default::default()
        }
    }

    /// P4-2a — at boot, every served organization's machines mirrored, on
    /// the pod that holds the startup lease.
    fn jobs(&self) -> Vec<Job> {
        let state = self.clone();
        vec![Job::at_startup("knowhow.sync", true, move || {
            let state = state.clone();
            async move { sync::at_boot(&state).await }
        })]
    }

    fn indexes(&self) -> Vec<IndexSet> {
        vec![
            IndexSet {
                collection: model::KnowhowEntity::COLLECTION,
                pre_ops: Vec::new(),
                indexes: vec![
                    // One live entity per kind and key in an organization; an
                    // archived one frees its key. `$type: "null"`: a live row
                    // stores an explicit null.
                    index_unique_partial(
                        bson::doc! { "tenant_id": 1, "kind": 1, "key": 1 },
                        bson::doc! { "deleted_at": { "$type": "null" } },
                    ),
                    // A search and a count: the live, by kind and key.
                    index(bson::doc! { "tenant_id": 1, "deleted_at": 1, "kind": 1, "key": 1 }),
                    // P4-3a-2 — a device's scan part: what its next complete
                    // scan compares with what it found.
                    index(bson::doc! { "tenant_id": 1, "device": 1, "deleted_at": 1 }),
                ],
            },
            IndexSet {
                collection: model::KnowhowEdge::COLLECTION,
                pre_ops: Vec::new(),
                indexes: vec![
                    // One live link of a kind between two entities.
                    index_unique_partial(
                        bson::doc! { "tenant_id": 1, "from": 1, "kind": 1, "to": 1 },
                        bson::doc! { "deleted_at": { "$type": "null" } },
                    ),
                    // A neighbourhood walks both ways: the links from an
                    // entity, and the links to it.
                    index(bson::doc! { "tenant_id": 1, "from": 1, "deleted_at": 1 }),
                    index(bson::doc! { "tenant_id": 1, "to": 1, "deleted_at": 1 }),
                    // P4-3a-2 — a device's scan links, as its entities.
                    index(bson::doc! { "tenant_id": 1, "device": 1, "deleted_at": 1 }),
                ],
            },
            // Every write, kept as long as the map: a fact's provenance may
            // be asked for years after it was written, so no TTL.
            IndexSet {
                collection: model::KnowhowHistory::COLLECTION,
                pre_ops: Vec::new(),
                indexes: vec![
                    // The organization's newest, and one entity's, its links'
                    // included (`entity_ids` holds an edge's two ends).
                    index(bson::doc! { "tenant_id": 1, "at": -1, "_id": -1 }),
                    index(bson::doc! { "tenant_id": 1, "entity_ids": 1, "at": -1, "_id": -1 }),
                ],
            },
            // P4-3a — the scans asked of devices.
            IndexSet {
                collection: model::KnowhowScan::COLLECTION,
                pre_ops: Vec::new(),
                indexes: vec![
                    // A device's one pending scan: a second request finds it.
                    index_unique_partial(
                        bson::doc! { "tenant_id": 1, "device_id": 1 },
                        bson::doc! { "pending": true },
                    ),
                    // A device's scans, newest first.
                    index(bson::doc! { "tenant_id": 1, "device_id": 1, "requested_at": -1 }),
                    // A sweep: what was asked of the devices connected here.
                    index(bson::doc! { "device_id": 1, "state": 1 }),
                    // Kept 90 days.
                    index_ttl(bson::doc! { "requested_at": 1 }, dao::SCAN_KEEP_SECS),
                ],
            },
        ]
    }
}
