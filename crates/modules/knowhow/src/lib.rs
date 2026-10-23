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
use roomler_ai_db::indexes::{IndexSet, index, index_unique_partial};
use roomler_core::{Capabilities, Core, Module, TenantCtx};

pub mod dao;
pub mod model;
pub mod routes;
pub mod scope;
pub mod view;

/// The module's state: the core, and the map's three collections.
#[derive(Clone)]
pub struct KnowhowState {
    pub core: Core,
    pub entities: Arc<dao::EntityDao>,
    pub edges: Arc<dao::EdgeDao>,
    pub history: Arc<dao::HistoryDao>,
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

    /// No module state: P4-1 is the store, on core alone. Its edges come
    /// with the steps that use them — `fleet` and `network` (P4-2's sync)
    /// and `hive` (P4-4's session reads).
    type Deps = ();

    async fn init(core: Core, settings: &Settings, _deps: ()) -> anyhow::Result<Self> {
        let db = &core.db;
        let scope = scope::TenantScope::parse(&settings.knowhow.tenants);
        scope.log();
        Ok(Self {
            entities: Arc::new(dao::EntityDao::new(db)),
            edges: Arc::new(dao::EdgeDao::new(db)),
            history: Arc::new(dao::HistoryDao::new(db)),
            scope: Arc::new(scope),
            core,
        })
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
            .nest("/tenant/{tenant_id}/knowhow/entity", entity)
            .nest("/tenant/{tenant_id}/knowhow/edge", edge)
            .with_state(self.clone())
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
        ]
    }
}
