// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! `fleet` — device management as a module (FR-69 P5a): agents, enrollment
//! tokens and keys, presence and its tokens, the agent Hub, crash and log
//! ingest, releases and installer proxies, owner consent, remote config, and
//! fleet RPC (exec) with its audit.
//!
//! The Hub came here from `roomler-ai-remote-control` — its consumers are
//! fleet's (the agent socket, presence, exec, consent, org join, forced
//! updates) and `remote`/`network` reach it through this crate, which is the
//! direction the module graph allows (`remote → fleet`, `network → fleet`).
//! The wire crate is wire-only for the server now.
//!
//! # What stays in the host for one more step
//!
//! The agent SOCKET (`ws/remote_control.rs::handle_agent_socket`) and the
//! controller-side `rc:*` dispatch are still the host's: its `rc:*` arms
//! call into overlay, tunnel and org-relay code that has not moved yet, and
//! a socket cannot dispatch to a module it cannot name. P5b moves the socket
//! here behind a core-owned handler registry. Until then the host keeps
//! `Arc` ALIASES of the handles below on `AppState` (`rc_hub`, `agents`, the
//! presence maps…), initialised FROM this module, so every host call site
//! reads as before and the object has one owner.
//!
//! # First use of the hooks
//!
//! Removing an agent releases its overlay lease first — `network`'s, which
//! fleet cannot call. [`removal::remove_agent_device`] runs the holders
//! through [`roomler_core::hooks::HookRegistry`] in `HOOK_ORDER` and then
//! deletes the row and kicks the socket; renaming an agent propagates the
//! MagicDNS label the same way. The host registers its transitional
//! implementation of those hooks under the `network` id.

use std::sync::Arc;

use axum::{
    Router,
    extract::FromRef,
    routing::{delete, get, post, put},
};
use bson::oid::ObjectId;
use dashmap::DashMap;
use roomler_ai_config::Settings;
use roomler_ai_db::indexes::{IndexSet, index, index_text, index_ttl, index_unique};
use roomler_ai_remote_control::{audit::AuditSink, models::AgentStatus};
use roomler_ai_services::dao::{
    agent::AgentDao, agent_crash::AgentCrashDao, agent_log::AgentLogDao,
    config_audit::ConfigAuditDao, consent_request::ConsentRequestDao,
    enrollment_key::EnrollmentKeyDao, exec_audit::ExecAuditDao,
};
use roomler_core::{Capabilities, Core, Module, TenantCtx, rate_limit::RateLimiter};
use tokio::sync::mpsc;

pub mod agent;
pub mod agent_crash;
pub mod agent_exec;
pub mod agent_log;
pub mod agent_org;
pub mod agent_release;
pub mod auth_agent;
pub mod consent;
pub mod consent_consumer;
pub mod ctrl;
pub mod enroll_key;
pub mod hub;
pub mod nudge;
pub mod presence;
pub mod releases;
pub mod remote_config;
pub mod removal;
pub mod setup_release;
pub mod tunnel_release;

pub use hub::Hub;

/// The module's state: the core plus what fleet owns.
#[derive(Clone)]
pub struct FleetState {
    pub core: Core,
    pub agents: Arc<AgentDao>,
    /// FR-51 P2 — reusable ephemeral enrollment keys + their per-use audit.
    pub enrollment_keys: Arc<EnrollmentKeyDao>,
    pub agent_crashes: Arc<AgentCrashDao>,
    pub agent_logs: Arc<AgentLogDao>,
    /// Phase 4 — owner-side consent requests (email/push approve-link tokens).
    pub consent_requests: Arc<ConsentRequestDao>,
    /// Fleet-RPC attempt log — every exec, allowed or denied.
    pub exec_audit: Arc<ExecAuditDao>,
    /// Remote-config decisions (`docs/remote-config.md`): what was ASKED for
    /// on a device, granted or refused — never what the device did.
    pub config_audit: Arc<ConfigAuditDao>,
    pub rc_hub: Arc<Hub>,
    /// Phase A-1 — the Redis presence owner-token per locally-registered
    /// agent WS. Written/removed by the agent socket; read by the shutdown
    /// sweep so it can compare-DEL each key (an unconditional DEL could erase
    /// a claim an agent already re-made on the surviving pod mid-roll).
    pub agent_presence_tokens: Arc<DashMap<ObjectId, String>>,
    /// P4 — per-tenant `device:presence` batching + member-list cache.
    pub presence_fanout: Arc<presence::PresenceFanout>,
    /// PR-1 rehome — owner-side per-agent nudge pacing (cooldown trio).
    pub agent_nudge_cooldowns: Arc<nudge::NudgeCooldowns>,
    /// PR-1 rehome — requester-side per-agent throttle for `rc.agent_nudge`
    /// RPCs (click storms sent 11 in 15 s pre-PR-1).
    pub agent_nudge_throttle: Arc<nudge::NudgeRequestThrottle>,
    /// The per-(caller, device) exec ceiling (`rate_limit.rs`).
    pub exec_rate_limiter: Arc<RateLimiter>,
    /// The GitHub-releases cache behind every installer and latest-release
    /// proxy; busted fleet-wide over the bus by `POST /api/releases/refresh`.
    pub releases_cache: Arc<releases::ReleasesCache>,
}

impl std::ops::Deref for FleetState {
    type Target = Core;

    fn deref(&self) -> &Core {
        &self.core
    }
}

/// `State<Core>` in this module's handlers, and the core extractors.
impl FromRef<FleetState> for Core {
    fn from_ref(state: &FleetState) -> Self {
        state.core.clone()
    }
}

impl Module for FleetState {
    const ID: &'static str = "fleet";

    async fn init(core: Core, settings: &Settings) -> anyhow::Result<Self> {
        let db = &core.db;
        let agents = Arc::new(AgentDao::new(db));
        let enrollment_keys = Arc::new(EnrollmentKeyDao::new(db));
        let consent_requests = Arc::new(ConsentRequestDao::new(db));

        let (audit_sink, _audit_handle) = AuditSink::spawn(core.db.clone());
        // Phase 4 — owner-side consent: the Hub emits a `ConsentEvent` for each
        // Email/Push session; the consumer resolves the owner + persists a
        // `ConsentRequest` + sends the email / web-push. Wiring `Some(consent_tx)`
        // is what turns those modes on; with `None` (tests) they'd just time out.
        let (consent_tx, consent_rx) = mpsc::channel::<hub::ConsentEvent>(64);
        let rc_hub = Arc::new(Hub::new_with_consent(
            audit_sink,
            (*core.turn_map).clone(),
            Some(consent_tx),
            core.relay_load.clone(),
        ));
        consent_consumer::spawn_consent_consumer(
            consent_rx,
            consent_consumer::ConsentConsumerDeps {
                agents: agents.clone(),
                users: core.users.clone(),
                consent_requests: consent_requests.clone(),
                push_subscriptions: core.push_subscriptions.clone(),
                email: core.email.clone(),
                push: core.push.clone(),
                base_url: settings.oauth.base_url.clone(),
                notifications: core.notifications.clone(),
                ws_storage: core.ws_storage.clone(),
                redis_pubsub: core.redis_pubsub.clone(),
            },
        );

        // The releases cache: `POST /api/releases/refresh` lands on ONE pod,
        // and the bus handler is how the other pods get busted too.
        let releases_cache = releases::ReleasesCache::new();
        if let Some(bus) = &core.cluster_bus {
            let cache = releases_cache.clone();
            let pod_id = core.pod.pod_id.clone();
            bus.register(releases::BUS_CLASS_REFRESH, move |body| {
                let cache = cache.clone();
                let pod_id = pod_id.clone();
                Box::pin(async move {
                    let expect = body
                        .get("expect_tag")
                        .and_then(|v| v.as_str())
                        .map(str::to_string);
                    let report = cache.force_refresh(&pod_id, expect.as_deref()).await;
                    serde_json::to_value(report).map_err(|e| e.to_string())
                })
            });
        }

        let state = Self {
            agents,
            enrollment_keys,
            agent_crashes: Arc::new(AgentCrashDao::new(db)),
            agent_logs: Arc::new(AgentLogDao::new(db)),
            consent_requests,
            exec_audit: Arc::new(ExecAuditDao::new(db)),
            config_audit: Arc::new(ConfigAuditDao::new(db)),
            rc_hub,
            agent_presence_tokens: Arc::new(DashMap::new()),
            presence_fanout: Arc::new(presence::PresenceFanout::default()),
            agent_nudge_cooldowns: Arc::new(DashMap::new()),
            agent_nudge_throttle: Arc::new(DashMap::new()),
            exec_rate_limiter: Arc::new(RateLimiter::new()),
            releases_cache,
            core,
        };

        // C-1 — the directory heartbeat: every 30 s, re-assert this pod's
        // agent presence records (gated on STILL holding the hub slot).
        // Redundant with the per-received-heartbeat refresh in the socket
        // handler — deliberately: it heals a record lost to a Redis flap
        // even while the agent is quiet. A CONFLICT (foreign owner) is the
        // fold signal: log it; the socket-level machinery (displacement
        // Goodbye / A2b counter) owns the reaction.
        if let Some(dir) = &state.cluster_directory {
            let dir = dir.clone();
            let hub = state.rc_hub.clone();
            let tokens = state.agent_presence_tokens.clone();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                    for entry in tokens.iter() {
                        let (agent_id, token) = (*entry.key(), entry.value().clone());
                        if !hub.is_agent_online(agent_id) {
                            continue;
                        }
                        let key = roomler_core::cluster::directory::agent_key(&agent_id.to_hex());
                        match dir.refresh_if_mine(&key, &token, 90).await {
                            Ok(true) => {}
                            Ok(false) => tracing::warn!(
                                agent = %agent_id,
                                "directory heartbeat CONFLICT — foreign pod owns this agent's record"
                            ),
                            Err(e) => {
                                tracing::debug!(agent = %agent_id, %e, "directory heartbeat failed");
                            }
                        }
                    }
                }
            });
        }
        // P4 — the presence staleness sweep (cluster-singleton per cycle via
        // a DB-name-scoped Redis NX claim; first tick a full interval out so
        // tests driving `run_presence_sweep` directly stay deterministic).
        presence::spawn_sweeper(state.clone());
        Ok(state)
    }

    fn enabled(settings: &Settings) -> bool {
        settings.modules.fleet
    }

    fn capabilities(&self, _tenant: &TenantCtx) -> Capabilities {
        Capabilities::enabled(Self::ID)
    }

    /// Exactly the paths the host mounted before P5a. The sub-routes under
    /// `/tenant/{tenant_id}/agent/{agent_id}/…` that belong to `remote` and
    /// `network` (ssh, ssh-policy, peer-relay-policy, overlay-key/rotate) are
    /// NOT here — the host still mounts those under the same prefix.
    fn routes(&self) -> Router {
        let agent = Router::new()
            .route("/", get(agent::list_agents))
            .route("/enroll-token", post(agent::issue_enrollment_token))
            // FR-51 P2 — reusable ephemeral enrollment keys (static segments
            // win over the `{agent_id}` param below, same as `/enroll-token`).
            .route(
                "/enroll-key",
                get(enroll_key::list_keys).post(enroll_key::mint_key),
            )
            .route("/enroll-key/{key_id}", delete(enroll_key::revoke_key))
            .route("/enroll-key/{key_id}/uses", get(enroll_key::list_key_uses))
            // S1a — operator-forced self-update: bulk + per-agent.
            .route("/update", post(agent::trigger_agents_update))
            .route("/{agent_id}/update", post(agent::trigger_agent_update))
            // Multi-org — add an already-enrolled device to a SECOND org.
            .route("/{agent_id}/join-org", post(agent_org::join_org))
            .route("/{agent_id}/join-targets", get(agent_org::join_targets))
            .route(
                "/{agent_id}",
                get(agent::get_agent)
                    .put(agent::update_agent)
                    .delete(agent::delete_agent),
            )
            .route("/{agent_id}/crash", get(agent_crash::list_for_agent))
            // rc.58 — centralized log batch ingest + listing.
            .route(
                "/{agent_id}/logs",
                post(agent_log::ingest_agent).get(agent_log::list_for_agent),
            )
            // Fleet RPC — remote command execution.
            .route("/exec", post(agent_exec::exec_bulk))
            .route("/{agent_id}/exec", post(agent_exec::exec))
            .route(
                "/{agent_id}/exec/{request_id}/cancel",
                post(agent_exec::cancel),
            )
            .route("/{agent_id}/exec-policy", put(agent_exec::set_policy))
            // Remote config — records an INTENT for the device to reconcile.
            .route(
                "/{agent_id}/desired-config",
                put(remote_config::set_desired_config),
            );

        // NB `/tenant/{tenant_id}/device` (the device listing) is NOT here: it
        // joins agents with tunnel clients and overlay nodes — a cross-pillar
        // view the host keeps until `network` exists.
        let exec_audit = Router::new().route("/", get(agent_exec::audit));
        let exec_settings = Router::new().route(
            "/",
            get(agent_exec::get_org_settings).put(agent_exec::set_org_settings),
        );
        // FR-51 P2 — the ephemeral-key class switch (MANAGE_TENANT).
        let ephemeral_key_settings = Router::new().route(
            "/",
            get(enroll_key::get_org_settings).put(enroll_key::set_org_settings),
        );
        let log = Router::new().route("/browser", post(agent_log::ingest_browser));

        // Public agent endpoints: enrollment uses an admin-issued enrollment
        // token (no user JWT); the release proxies are unauthenticated because
        // the agent's auto-updater calls them before any session; crash ingest
        // and self-unenroll authenticate with the agent's own JWT (`AuthAgent`).
        let public_agent = Router::new()
            .route("/enroll", post(agent::enroll_agent))
            .route("/latest-release", get(agent_release::latest_release))
            .route(
                "/installer/{flavour}/health",
                get(agent_release::installer_health),
            )
            .route("/installer/{flavour}", get(agent_release::installer_proxy))
            .route("/crash", post(agent_crash::ingest))
            .route("/self/unenroll", post(agent::self_unenroll));

        // Public owner-consent routes (Phase 4). No auth extractor — the
        // unguessable token IS the capability.
        let public_consent = Router::new()
            .route("/{token}/approve", post(consent::approve_consent))
            .route("/{token}/deny", post(consent::deny_consent));

        // `/api/tunnel/{latest-release,installer/{platform}}` — the roomler
        // CLI binary's proxy, a separate namespace from the agent's.
        let public_tunnel_release = Router::new()
            .route("/latest-release", get(tunnel_release::latest_release))
            .route(
                "/installer/{platform}/health",
                get(tunnel_release::installer_health),
            )
            .route(
                "/installer/{platform}",
                get(tunnel_release::installer_proxy),
            );

        // `/api/setup/…` — the unified `roomler-setup` wizard's proxy. The
        // static `/install.sh` + `/install.ps1` segments win over `/{platform}`.
        let public_setup = Router::new()
            .route("/latest-release", get(setup_release::setup_latest_release))
            .route("/install.sh", get(setup_release::install_script_sh))
            .route("/install.ps1", get(setup_release::install_script_ps1))
            .route(
                "/{platform}/health",
                get(setup_release::setup_installer_health),
            )
            .route("/{platform}", get(setup_release::setup_installer_proxy));

        // `/api/releases/refresh` — bearer-authenticated cache-bust called by
        // the release workflows on every published tag.
        let releases = Router::new().route("/refresh", post(releases::refresh));

        Router::new()
            .nest("/tenant/{tenant_id}/agent", agent)
            .nest("/tenant/{tenant_id}/exec-audit", exec_audit)
            .nest("/tenant/{tenant_id}/exec-settings", exec_settings)
            .nest(
                "/tenant/{tenant_id}/ephemeral-key-settings",
                ephemeral_key_settings,
            )
            .nest("/log", log)
            .nest("/agent", public_agent)
            .nest("/consent", public_consent)
            .nest("/tunnel", public_tunnel_release)
            .nest("/setup", public_setup)
            .nest("/releases", releases)
            .with_state(self.clone())
    }

    /// The eight collections this module owns. The specs are the ones the db
    /// crate's plan held before P5a, unchanged.
    fn indexes(&self) -> Vec<IndexSet> {
        vec![
            IndexSet {
                collection: "consent_requests",
                pre_ops: Vec::new(),
                indexes: vec![
                    index_unique(bson::doc! { "token": 1 }),
                    index(bson::doc! { "session_id": 1 }),
                    index_ttl(bson::doc! { "expires_at": 1 }, 0),
                ],
            },
            IndexSet {
                collection: "agents",
                pre_ops: Vec::new(),
                indexes: vec![
                    index_unique(bson::doc! { "tenant_id": 1, "machine_id": 1 }),
                    index(bson::doc! { "tenant_id": 1, "status": 1 }),
                    index(bson::doc! { "owner_user_id": 1 }),
                    // FR-51 — the reaper's candidate scan (ESR).
                    index(bson::doc! { "ephemeral": 1, "deleted_at": 1, "last_seen_at": 1 }),
                ],
            },
            IndexSet {
                collection: "agent_crashes",
                pre_ops: Vec::new(),
                indexes: vec![
                    index(bson::doc! { "tenant_id": 1, "agent_id": 1, "crashed_at_unix": -1 }),
                    index_ttl(bson::doc! { "reported_at": 1 }, 90 * 24 * 60 * 60),
                ],
            },
            IndexSet {
                collection: "enrollment_keys",
                pre_ops: Vec::new(),
                indexes: vec![
                    index_unique(bson::doc! { "jti": 1 }),
                    index(bson::doc! { "tenant_id": 1, "created_at": -1 }),
                ],
            },
            // FR-51 P2 — one row per successful key use: the trail that
            // survives the reap. 90-day TTL like the other audit collections.
            IndexSet {
                collection: "enrollment_key_uses",
                pre_ops: Vec::new(),
                indexes: vec![
                    index(bson::doc! { "tenant_id": 1, "key_id": 1, "created_at": -1 }),
                    index_ttl(bson::doc! { "created_at": 1 }, 90 * 24 * 60 * 60),
                ],
            },
            IndexSet {
                collection: "exec_audit",
                pre_ops: Vec::new(),
                indexes: vec![
                    index(bson::doc! { "tenant_id": 1, "at": -1 }),
                    index(bson::doc! { "agent_id": 1, "at": -1 }),
                    index(bson::doc! { "tenant_id": 1, "user_id": 1, "at": -1 }),
                    index_ttl(bson::doc! { "at": 1 }, 90 * 24 * 60 * 60),
                ],
            },
            // A config change that opens exec is the same class of event as
            // using it, so it must not age out sooner than exec_audit.
            IndexSet {
                collection: "config_audit",
                pre_ops: Vec::new(),
                indexes: vec![
                    index(bson::doc! { "tenant_id": 1, "at": -1 }),
                    index(bson::doc! { "agent_id": 1, "at": -1 }),
                    index(bson::doc! { "tenant_id": 1, "user_id": 1, "at": -1 }),
                    index_ttl(bson::doc! { "at": 1 }, 90 * 24 * 60 * 60),
                ],
            },
            IndexSet {
                collection: "agent_logs",
                pre_ops: Vec::new(),
                indexes: vec![
                    index(bson::doc! { "tenant_id": 1, "agent_id": 1, "created_at": -1 }),
                    index(bson::doc! { "tenant_id": 1, "user_id": 1, "created_at": -1 }),
                    index(bson::doc! { "tenant_id": 1, "source": 1, "created_at": -1 }),
                    index(bson::doc! { "tenant_id": 1, "session_id": 1 }),
                    index_text(bson::doc! { "lines.msg": "text" }),
                    index_ttl(bson::doc! { "created_at": 1 }, 7 * 24 * 60 * 60),
                ],
            },
        ]
    }

    /// Phase A-1 graceful shutdown: make this pod's death honest BEFORE the
    /// process exits, so a roll never strands `agents.status = 'Online'` rows
    /// + stale presence claims (the green-but-dead badge class). Fire every
    /// registered agent's displacement-cancel notify — each read loop exits
    /// within milliseconds and runs its OWN teardown; the bulk writes are
    /// belt-and-braces for sockets that don't finish in time. Agents see a
    /// plain socket close (never a Goodbye) and reconnect with backoff.
    fn shutdown(&self) -> impl std::future::Future<Output = ()> + Send {
        let state = self.clone();
        async move {
            let ids = state.rc_hub.cancel_all_agents();
            if ids.is_empty() {
                return;
            }
            tracing::info!(
                agents = ids.len(),
                "shutdown: cancelling local agent sockets + bulk offline sweep"
            );
            // Give the per-socket teardowns a beat to do the fine-grained cleanup.
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            if let Err(e) = state
                .agents
                .mark_status_many(&ids, AgentStatus::Offline)
                .await
            {
                tracing::warn!(%e, "shutdown: bulk mark_status(Offline) failed");
            }
            if let Some(redis) = &state.redis_pubsub {
                for id in &ids {
                    if let Some((_, token)) = state.agent_presence_tokens.remove(id)
                        && let Err(e) = redis
                            .agent_presence_del_if_owned(&id.to_hex(), &token)
                            .await
                    {
                        tracing::debug!(agent = %id, %e, "shutdown: presence release failed");
                    }
                }
            }
        }
    }
}
