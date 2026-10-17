// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! `vault` — IAM for secrets, the server side (FR-90 P3,
//! `docs/roomler-hive-design.md` §8, `docs/vault.md`).
//!
//! ⚠️ **Managed custody.** This module holds the key-encryption key and can
//! decrypt every secret it stores, so a compromised server can read them
//! all. That is the v1 model, said first and plainly; customer-managed keys
//! and sovereign custody are P7.
//!
//! P3-1 builds the store: secrets and their versions, each version sealed
//! under its own data key wrapped to an `age` KEK ([`crypto`]), named by
//! RRNs ([`rrn`]), every decision recorded in `vault_audit` — and an
//! administrator's metadata routes ([`routes`]), none of which answers a
//! value. Roles and Cedar policies (P3-2), a device's leases (P3-3) and the
//! session's tools (P3-4) are built on it.
//!
//! The module's switch defaults OFF (`[modules] vault`), and a
//! `vault.tenants` list both switches it on and limits it to the
//! organizations listed — the form a hosted server uses, as `hive.tenants`
//! is for Hive. Clearing the list is the kill switch.

use std::sync::Arc;

use axum::{
    Router,
    extract::FromRef,
    routing::{get, post, put},
};
use roomler_ai_config::Settings;
use roomler_ai_db::indexes::{IndexSet, index, index_ttl, index_unique, index_unique_partial};
use roomler_core::{Capabilities, Core, Module, TenantCtx};

pub mod crypto;
pub mod dao;
pub mod model;
pub mod routes;
pub mod rrn;
pub mod scope;

/// The module's state: the core, and what the vault owns.
#[derive(Clone)]
pub struct VaultState {
    pub core: Core,
    pub secrets: Arc<dao::VaultSecretDao>,
    pub versions: Arc<dao::VaultVersionDao>,
    pub audit: Arc<dao::VaultAuditDao>,
    /// The organizations the vault serves (`vault.tenants`).
    pub scope: Arc<scope::TenantScope>,
    /// The KEK, or why there is none: read once at boot.
    pub kek: crypto::KekState,
}

impl std::ops::Deref for VaultState {
    type Target = Core;

    fn deref(&self) -> &Core {
        &self.core
    }
}

/// `State<Core>` in this module's handlers, and the core extractors.
impl FromRef<VaultState> for Core {
    fn from_ref(state: &VaultState) -> Self {
        state.core.clone()
    }
}

impl Module for VaultState {
    const ID: &'static str = "vault";

    /// Core alone in P3-1: the edges to `hive` and `fleet` come with the
    /// steps that use them.
    type Deps = ();

    async fn init(core: Core, settings: &Settings, _deps: ()) -> anyhow::Result<Self> {
        let db = &core.db;
        let scope = scope::TenantScope::parse(&settings.vault.tenants);
        scope.log();
        // Never a boot refusal: a missing or unusable KEK leaves the
        // metadata routes working and answers 503 where the key is needed.
        let kek = crypto::KekState::load(&settings.vault);
        kek.log();
        Ok(Self {
            secrets: Arc::new(dao::VaultSecretDao::new(db)),
            versions: Arc::new(dao::VaultVersionDao::new(db)),
            audit: Arc::new(dao::VaultAuditDao::new(db)),
            scope: Arc::new(scope),
            kek,
            core,
        })
    }

    /// The module switch, or a `vault.tenants` list.
    fn enabled(settings: &Settings) -> bool {
        settings.vault_on()
    }

    fn capabilities(&self, _tenant: &TenantCtx) -> Capabilities {
        Capabilities::enabled(Self::ID)
    }

    fn routes(&self) -> Router {
        let secret = Router::new()
            .route("/", get(routes::list).post(routes::create))
            .route(
                "/{secret_id}",
                get(routes::describe)
                    .patch(routes::update)
                    .delete(routes::delete),
            )
            .route("/{secret_id}/value", put(routes::put_value));
        Router::new()
            .route("/tenant/{tenant_id}/vault", get(routes::status))
            .route("/tenant/{tenant_id}/vault/verify", post(routes::verify))
            .route("/tenant/{tenant_id}/vault/audit", get(routes::audit))
            .nest("/tenant/{tenant_id}/vault/secret", secret)
            .with_state(self.clone())
    }

    fn indexes(&self) -> Vec<IndexSet> {
        vec![
            IndexSet {
                collection: model::VaultSecret::COLLECTION,
                pre_ops: Vec::new(),
                indexes: vec![
                    // One live secret per path in an organization; a
                    // tombstone frees its path. `$type: "null"`: a live row
                    // stores an explicit null.
                    index_unique_partial(
                        bson::doc! { "tenant_id": 1, "path": 1 },
                        bson::doc! { "deleted_at": { "$type": "null" } },
                    ),
                    // The listing: live rows, by path.
                    index(bson::doc! { "tenant_id": 1, "deleted_at": 1, "path": 1 }),
                ],
            },
            IndexSet {
                collection: model::VaultVersion::COLLECTION,
                pre_ops: Vec::new(),
                indexes: vec![
                    // One row per (secret, field, number): the counter hands
                    // numbers out, the index holds it to that.
                    index_unique(bson::doc! { "secret_id": 1, "field": 1, "n": 1 }),
                    // An organization's versions: the counts, a verify's
                    // scan, those wrapped to another KEK.
                    index(bson::doc! { "tenant_id": 1, "kek": 1 }),
                ],
            },
            // The server's decisions — 90-day retention, like the exec, SSH
            // and Hive audits.
            IndexSet {
                collection: model::VaultAuditEvent::COLLECTION,
                pre_ops: Vec::new(),
                indexes: vec![
                    index(bson::doc! { "tenant_id": 1, "at": -1 }),
                    index(bson::doc! { "secret_id": 1, "at": 1 }),
                    index_ttl(bson::doc! { "at": 1 }, 90 * 24 * 60 * 60),
                ],
            },
        ]
    }
}
