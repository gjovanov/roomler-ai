// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! `saas` — the hosted-product layer, as a module (FR-69 P2).
//!
//! Stripe billing (plans, checkout, the customer portal, the webhook), the
//! public updates list and the newsletter sending program (FR-39 / FR-58),
//! and the platform-admin plan-compliance view (FR-32). Everything here
//! exists because roomler.ai is a hosted product; a self-hosted deployment
//! runs without it, which is why it is an **add-on feature** on the host
//! rather than part of any profile.
//!
//! It is also the first module crate, chosen for that reason: it is the
//! smallest, and it exercises the whole [`Module`] contract at the lowest
//! risk — `unlimited_routes` for the Stripe webhook (signature-authenticated
//! and deliberately outside the per-IP governor), `indexes` for its three
//! collections, a runtime switch (`[modules] saas = false`) that unmounts it
//! on a live pod.
//!
//! # Shape
//!
//! [`SaasState`] is the module's state: a [`Core`] plus the three DAOs it
//! owns. It derefs to `Core`, so handlers read `state.settings`,
//! `state.email`, `state.tenants` exactly as they did on the host's
//! `AppState`; `impl FromRef<SaasState> for Core` lets a handler that needs
//! only the core take `State<Core>`, and lets `roomler-core`'s extractors
//! (`AuthUser`, `TenantId`) serve this router unchanged.

use std::sync::Arc;

use axum::{
    Router,
    extract::FromRef,
    routing::{get, post, put},
};
use roomler_ai_config::Settings;
use roomler_ai_db::indexes::{IndexSet, index, index_unique};
use roomler_ai_services::dao::{
    newsletter_issue::NewsletterIssueDao, newsletter_send::NewsletterSendDao,
    subscriber::SubscriberDao,
};
use roomler_core::{Capabilities, Core, Module, TenantCtx};

pub mod newsletter;
pub mod newsletter_send;
pub mod plan_compliance;
pub mod stripe;
pub mod subscribe;

/// The module's state: the core plus the three collections it owns.
#[derive(Clone)]
pub struct SaasState {
    pub core: Core,
    /// FR-39 — the public updates list. Not a user store: no password, no
    /// tenant, no session.
    pub subscribers: Arc<SubscriberDao>,
    /// FR-58 — newsletter issues (platform-admin surface).
    pub newsletter_issues: Arc<NewsletterIssueDao>,
    /// FR-58 — the per-recipient delivery ledger; its unique index is the
    /// send program's at-most-once invariant.
    pub newsletter_sends: Arc<NewsletterSendDao>,
}

impl std::ops::Deref for SaasState {
    type Target = Core;

    fn deref(&self) -> &Core {
        &self.core
    }
}

/// `State<Core>` in this module's handlers, and the core extractors.
impl FromRef<SaasState> for Core {
    fn from_ref(state: &SaasState) -> Self {
        state.core.clone()
    }
}

impl Module for SaasState {
    const ID: &'static str = "saas";

    async fn init(core: Core, _settings: &Settings) -> anyhow::Result<Self> {
        let db = &core.db;
        Ok(Self {
            subscribers: Arc::new(SubscriberDao::new(db)),
            newsletter_issues: Arc::new(NewsletterIssueDao::new(db)),
            newsletter_sends: Arc::new(NewsletterSendDao::new(db)),
            core,
        })
    }

    fn enabled(settings: &Settings) -> bool {
        settings.modules.saas
    }

    fn capabilities(&self, _tenant: &TenantCtx) -> Capabilities {
        // Plan-independent: billing and the newsletter are platform features,
        // not something a tenant's plan switches on or off.
        Capabilities::enabled(Self::ID)
    }

    /// Everything under `/api`, inside the governor. Paths are the ones the
    /// host mounted before P2, unchanged — the composition baseline is the
    /// check.
    fn routes(&self) -> Router {
        // Stripe: plan listing + checkout + customer portal. The webhook is
        // in `unlimited_routes` (see there).
        let stripe = Router::new()
            .route("/plans", get(stripe::get_plans))
            .route("/checkout", post(stripe::create_checkout))
            .route("/portal", post(stripe::create_portal));

        // FR-58 — the newsletter sending program's admin surface. Platform-
        // admin gated, 404 on miss; `platform_admins` unset ⇒ the whole
        // family 404s.
        let admin_newsletter = Router::new()
            .route("/issues", post(newsletter::create).get(newsletter::list))
            .route(
                "/issues/{slug}",
                put(newsletter::update).get(newsletter::get_one),
            )
            .route("/issues/{slug}/preview", get(newsletter::preview))
            .route("/issues/{slug}/test-send", post(newsletter::test_send))
            .route("/issues/{slug}/send", post(newsletter::send))
            .route("/issues/{slug}/status", get(newsletter::status));

        Router::new()
            .nest("/stripe", stripe)
            .nest("/admin/newsletter", admin_newsletter)
            // FR-32 P1c — "who would break if enforcement were turned on?".
            // Platform-admin only (404 on miss, like the rest of /admin).
            .route(
                "/admin/plan-compliance",
                get(plan_compliance::admin_plan_compliance),
            )
            // FR-58 P4 — the signed-in newsletter toggle. A different door
            // into `subscribers`, not a second list. A static segment, so it
            // wins over the host's `/user/{user_id}` capture.
            .route(
                "/user/newsletter",
                get(newsletter::user_get).put(newsletter::user_set),
            )
            // FR-39 — the public updates list. No auth extractor: `subscribe`
            // is an open form, and for the other two the unguessable token IS
            // the capability. ⚠️ `subscribe` answers 202 for every outcome,
            // including failure — the uniform response is a membership-oracle
            // control, not error handling.
            .route("/subscribe", post(subscribe::subscribe))
            // FR-58 follow-up: the GET is a pure redirect to the confirm PAGE —
            // Gmail's link scanner burned the first field subscriber's
            // single-use token via the old confirming GET. The POST is the
            // deliberate click.
            .route(
                "/subscribe/confirm/{token}",
                get(subscribe::confirm_redirect).post(subscribe::confirm),
            )
            // FR-58: the POST leg is the RFC 8058 one-click target — same
            // path, same token capability, plain 200 (providers follow no
            // redirects).
            .route(
                "/subscribe/unsubscribe/{token}",
                get(subscribe::unsubscribe).post(subscribe::unsubscribe_oneclick),
            )
            .with_state(self.clone())
    }

    /// S5 — the Stripe webhook, OUTSIDE the governor: it is signature-
    /// authenticated, and retry bursts from Stripe's fixed IPs would trip the
    /// per-IP limiter and mark the endpoint failing on their dashboard.
    fn unlimited_routes(&self) -> Router {
        Router::new()
            .route("/api/stripe/webhook", post(stripe::webhook))
            .with_state(self.clone())
    }

    /// The three collections this module owns. The specs are the ones the
    /// db crate's plan held before P2, unchanged.
    fn indexes(&self) -> Vec<IndexSet> {
        vec![
            // Subscribers (FR-39).
            IndexSet {
                collection: "subscribers",
                pre_ops: Vec::new(),
                indexes: vec![
                    index_unique(bson::doc! { "email": 1 }),
                    index_unique(bson::doc! { "unsubscribe_token": 1 }),
                    index(bson::doc! { "confirm_token": 1 }),
                    index(bson::doc! { "created_at": -1 }),
                ],
            },
            // Newsletter issues (FR-58). `slug` is unique because create is
            // explicit (a typo'd slug on update must 404, never upsert a
            // second issue), and the unique index is what arbitrates two
            // concurrent creates.
            IndexSet {
                collection: "newsletter_issues",
                pre_ops: Vec::new(),
                indexes: vec![
                    index_unique(bson::doc! { "slug": 1 }),
                    index(bson::doc! { "created_at": -1 }),
                ],
            },
            // Newsletter delivery ledger (FR-58). 🔑 The unique pair IS the
            // send program's at-most-once invariant: rows are claimed
            // (inserted) before the send attempt, so a resume — or even two
            // pods fanning out concurrently — resolves each recipient to
            // exactly one winner.
            IndexSet {
                collection: "newsletter_sends",
                pre_ops: Vec::new(),
                indexes: vec![
                    index_unique(bson::doc! { "issue_id": 1, "subscriber_id": 1 }),
                    index(bson::doc! { "issue_id": 1, "status": 1 }),
                ],
            },
        ]
    }
}
