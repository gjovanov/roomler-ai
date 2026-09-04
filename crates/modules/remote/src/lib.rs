// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! `remote` — remote desktop as a module (FR-69 P6): what a CONTROLLER
//! reaches. The session routes (get / terminate / audit), TURN credentials
//! and the relay-region listing; the `rc:*` dispatch a browser tab drives
//! with its authz + consent-mode gate; the cross-pod RC relay (PR-2); and
//! the agent-side session-stats merge.
//!
//! The session state machine itself is NOT here: the Hub is the fleet
//! module's (its consumers span fleet, remote and network), and this crate
//! is built ON fleet — the first module whose [`Module::Deps`] is another
//! module's state, because the Hub is one live object and a module that
//! re-created it would dispatch into an empty registry. `remote → fleet` is
//! the graph edge; nothing here names the host.
//!
//! # What the host keeps
//!
//! The user socket: its upgrade, the Hub registration of the controller's
//! sender and the pump. It hands each `rc:*` text frame to
//! [`controller::handle_controller_frame`] WITH that sender (through the
//! host's `Modules::remote_controller_frame`), and forwards a closed
//! connection to [`relay::forward_conn_closed`]. A namespace handler could
//! not carry the sender, which is why the controller path is a call the host
//! makes rather than a `Module::ws` registration.

use std::sync::Arc;

use axum::{
    Router,
    extract::FromRef,
    routing::{get, post},
};
use roomler_ai_config::Settings;
use roomler_ai_db::indexes::{IndexSet, index, index_ttl};
use roomler_ai_mod_fleet::FleetState;
use roomler_ai_services::dao::{remote_audit::RemoteAuditDao, remote_session::RemoteSessionDao};
use roomler_core::{AgentSocketHooks, Capabilities, Core, Module, TenantCtx};

pub mod agent_socket;
pub mod controller;
pub mod relay;
pub mod routes;

/// The module's state: the core, the fleet module it is built on, and what
/// remote owns.
#[derive(Clone)]
pub struct RemoteState {
    pub core: Core,
    /// `remote → fleet`: the Hub, the agent rows, the nudge machinery.
    pub fleet: FleetState,
    pub remote_sessions: Arc<RemoteSessionDao>,
    pub remote_audit: Arc<RemoteAuditDao>,
    /// PR-2 relay — owner-side proxy controllers for cross-pod rc sessions,
    /// keyed by the ORIGIN connection id. See [`relay`].
    pub rc_proxy_controllers: Arc<relay::ProxyControllers>,
    /// PR-2 relay — controller-side: conn id → owner pods hosting its
    /// proxied rc sessions (a WS close forwards `rc.conn_closed` there;
    /// mirrors conference's `remote_media_conns`).
    pub remote_rc_conns: Arc<relay::RemoteRcConns>,
}

impl std::ops::Deref for RemoteState {
    type Target = Core;

    fn deref(&self) -> &Core {
        &self.core
    }
}

/// `State<Core>` in this module's handlers, and the core extractors.
impl FromRef<RemoteState> for Core {
    fn from_ref(state: &RemoteState) -> Self {
        state.core.clone()
    }
}

impl Module for RemoteState {
    const ID: &'static str = "remote";

    type Deps = FleetState;

    async fn init(core: Core, _settings: &Settings, fleet: FleetState) -> anyhow::Result<Self> {
        let db = &core.db;
        let state = Self {
            remote_sessions: Arc::new(RemoteSessionDao::new(db)),
            remote_audit: Arc::new(RemoteAuditDao::new(db)),
            rc_proxy_controllers: Arc::new(relay::ProxyControllers::new()),
            remote_rc_conns: Arc::new(relay::RemoteRcConns::new()),
            fleet,
            core,
        };
        // PR-2 — the cross-pod rc signalling relay: owner-side rc.cmd /
        // rc.conn_closed / rc.conn_alive + the proxy janitor sweep.
        relay::wire_rc_relay(&state);
        // The agent socket is fleet's; this module's arm (the session-stats
        // merge) is dispatched to it by `ClientMsg::namespace()` (P5c).
        state.core.agent_socket.register(
            Self::ID,
            AgentSocketHooks {
                handler: Some(Arc::new(agent_socket::RemoteAgentSocket::new(
                    state.clone(),
                ))),
                lifecycle: None,
            },
        );
        Ok(state)
    }

    fn enabled(settings: &Settings) -> bool {
        settings.modules.remote
    }

    fn capabilities(&self, _tenant: &TenantCtx) -> Capabilities {
        Capabilities::enabled(Self::ID)
    }

    /// Exactly the paths the host mounted before P6.
    fn routes(&self) -> Router {
        // Remote-control session routes (tenant-scoped).
        let session = Router::new()
            .route("/{session_id}", get(routes::get_session))
            .route("/{session_id}/terminate", post(routes::terminate_session))
            .route("/{session_id}/audit", get(routes::session_audit));
        // TURN credentials (user-scoped, no tenant prefix).
        let turn = Router::new().route("/credentials", get(routes::turn_credentials));
        // Multi-region relay PoP topology (user-scoped, read-only, no secrets).
        let relay = Router::new().route("/regions", get(routes::relay_regions));

        Router::new()
            .nest("/tenant/{tenant_id}/session", session)
            .nest("/turn", turn)
            .nest("/relay", relay)
            .with_state(self.clone())
    }

    /// The two collections this module owns. The specs are the ones the db
    /// crate's plan held before P6, unchanged — `remote_sessions` has two
    /// sets there (the session indexes, then the Wave-3 per-tenant usage
    /// read), in that order.
    fn indexes(&self) -> Vec<IndexSet> {
        vec![
            IndexSet {
                collection: "remote_sessions",
                pre_ops: Vec::new(),
                indexes: vec![
                    index(bson::doc! { "agent_id": 1, "created_at": -1 }),
                    index(bson::doc! { "controller_user_id": 1, "created_at": -1 }),
                    index(bson::doc! { "tenant_id": 1, "phase": 1 }),
                ],
            },
            // Remote-control audit log — 90-day retention.
            IndexSet {
                collection: "remote_audit",
                pre_ops: Vec::new(),
                indexes: vec![
                    index(bson::doc! { "session_id": 1, "at": 1 }),
                    index(bson::doc! { "tenant_id": 1, "at": -1 }),
                    index_ttl(bson::doc! { "at": 1 }, 90 * 24 * 60 * 60),
                ],
            },
            // Wave 3 — per-user usage reads scan sessions by (tenant, time).
            IndexSet {
                collection: "remote_sessions",
                pre_ops: Vec::new(),
                indexes: vec![index(bson::doc! { "tenant_id": 1, "created_at": -1 })],
            },
        ]
    }
}
