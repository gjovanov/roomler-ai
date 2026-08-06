use std::collections::HashSet;

use bson::oid::ObjectId;
use dashmap::DashMap;
use mongodb::Database;
use roomler_ai_config::Settings;
use roomler_ai_remote_control::{
    Hub,
    audit::AuditSink,
    hub::ConsentEvent,
    models::ConsentMode,
    signaling::ServerMsg,
    turn_creds::{TurnConfig, TurnMap},
    turn_url::{VariantCaps, expand_turn_url},
};
use roomler_ai_services::{
    AuthService, EmailService, GiphyService, OAuthService, PushService, RecognitionService,
    TaskService,
    dao::{
        activation_code::ActivationCodeDao, agent::AgentDao, consent_request::ConsentRequestDao,
        exec_audit::ExecAuditDao, file::FileDao, invite::InviteDao, message::MessageDao,
        notification::NotificationDao, overlay_network::OverlayNetworkDao,
        overlay_node::OverlayNodeDao, overlay_policy::OverlayPolicyDao,
        push_subscription::PushSubscriptionDao, reaction::ReactionDao, recording::RecordingDao,
        remote_audit::RemoteAuditDao, remote_session::RemoteSessionDao, role::RoleDao,
        room::RoomDao, tenant::TenantDao, tunnel_audit::TunnelAuditDao,
        tunnel_client::TunnelClientDao, tunnel_policy::TunnelPolicyDao, user::UserDao,
    },
    media::{room_manager::RoomManager, worker_pool::WorkerPool},
};
use tokio::sync::mpsc;

use std::sync::Arc;

use crate::ws::redis_pubsub::RedisPubSub;
use crate::ws::storage::WsStorage;

/// Outbound channel for a connected `roomler-tunnel` client, keyed by
/// the `tunnel_session_id` issued on `rc:tunnel.open`. The tunnel WS
/// handler registers its sender on TunnelOpen success and unregisters
/// on disconnect / TunnelTerminate; the agent WS handler reads this
/// map to relay `TcpForwardAccept` / `TcpForwardReject` /
/// `TcpHalfClose` / `TcpClosed` / `TunnelTerminate` from agent →
/// client.
///
/// Mirror of the Hub's per-agent `tx` registry, but kept in `AppState`
/// rather than in the `remote_control::Hub` because the Hub is the
/// remote-control session state machine and tunnel-clients are a
/// distinct lifecycle.
pub type TunnelClientOutbound = Arc<DashMap<ObjectId, mpsc::Sender<ServerMsg>>>;

#[derive(Clone)]
pub struct AppState {
    pub db: Database,
    pub settings: Settings,
    pub auth: Arc<AuthService>,
    pub users: Arc<UserDao>,
    pub activation_codes: Arc<ActivationCodeDao>,
    pub tenants: Arc<TenantDao>,
    pub rooms: Arc<RoomDao>,
    pub invites: Arc<InviteDao>,
    pub messages: Arc<MessageDao>,
    pub notifications: Arc<NotificationDao>,
    pub reactions: Arc<ReactionDao>,
    pub roles: Arc<RoleDao>,
    pub files: Arc<FileDao>,
    pub recordings: Arc<RecordingDao>,

    pub tasks: Arc<TaskService>,
    pub room_manager: Arc<RoomManager>,
    pub ws_storage: Arc<WsStorage>,
    pub recognition: RecognitionService,
    pub oauth: Option<Arc<OAuthService>>,
    pub giphy: Option<Arc<GiphyService>>,
    pub email: Option<Arc<EmailService>>,
    pub push: Option<Arc<PushService>>,
    pub push_subscriptions: Arc<PushSubscriptionDao>,
    pub redis_pubsub: Option<Arc<RedisPubSub>>,
    /// True while the Redis pub/sub subscriber holds a live subscription —
    /// flipped by `RedisPubSub::subscribe_with_reconnect` (started in
    /// `AppState::new` since C-2, so TestApps exercise cross-pod delivery),
    /// read by `/health/ready`.
    pub redis_sub_alive: Arc<std::sync::atomic::AtomicBool>,
    /// File-storage backend for uploads + export artifacts (local disk or
    /// S3/MinIO, picked from `s3.enabled` at startup). See [`crate::storage`].
    pub storage: Arc<crate::storage::FileStorage>,

    // Remote-control subsystem
    pub agents: Arc<AgentDao>,
    /// Single-use ledger for enrollment-token jtis. See
    /// [`roomler_ai_services::dao::used_token`].
    pub used_tokens: Arc<roomler_ai_services::dao::used_token::UsedTokenDao>,
    pub remote_sessions: Arc<RemoteSessionDao>,
    pub remote_audit: Arc<RemoteAuditDao>,
    /// Fleet-RPC attempt log — every exec, allowed or denied.
    pub exec_audit: Arc<ExecAuditDao>,
    pub agent_crashes: Arc<roomler_ai_services::dao::agent_crash::AgentCrashDao>,
    pub agent_logs: Arc<roomler_ai_services::dao::agent_log::AgentLogDao>,
    /// Observability sample sinks (`stats_*` collections) — idempotent
    /// deterministic-`_id` upserts, so every collector is 2-pod safe.
    /// Writers gate on `settings.stats.enabled`.
    pub stats: Arc<roomler_ai_services::dao::stats::StatsDao>,
    /// Stats PR-3 — platform-operator allowlist parsed from
    /// `ROOMLER__STATS__PLATFORM_ADMINS` (user OBJECTIDS, deliberately not
    /// emails: OAuth links accounts by bare email, so an email allowlist
    /// would turn a provider-asserted address into platform-root).
    pub platform_admins: Arc<std::collections::HashSet<ObjectId>>,
    /// Phase 4 — owner-side consent requests (email/push approve-link tokens).
    pub consent_requests: Arc<ConsentRequestDao>,
    pub rc_hub: Arc<Hub>,
    /// Region-keyed TURN issuance (the Hub holds its own clone). Built once at
    /// startup from `turn.*` + `relay.*` settings; `cfg_for(None)` == the
    /// legacy single-region config.
    pub turn_map: Arc<roomler_ai_remote_control::turn_creds::TurnMap>,
    /// P6b — live per-region load written by the `/stats` poller; consulted
    /// by the Hub (session freeze) and the overlay pair-region pick.
    pub relay_load: roomler_ai_remote_control::turn_creds::RelayLoadMap,
    /// Multi-region DERP ticket signer (`relay.derp_ticket_private_key`).
    /// `None` = no key configured — ticket requests go unanswered and agents
    /// keep using the central `/derp`.
    pub derp_ticket: Option<Arc<roomler_ai_remote_control::derp_ticket::DerpTicketSigner>>,
    /// Phase A-1 — the Redis presence owner-token per locally-registered
    /// agent WS. Written/removed by `ws::remote_control::handle_agent_socket`;
    /// read by [`shutdown_cleanup`] so the SIGTERM sweep can compare-DEL
    /// each key (an unconditional DEL could erase a claim an agent already
    /// re-made on the surviving pod mid-roll).
    pub agent_presence_tokens: Arc<DashMap<ObjectId, String>>,
    /// P4 — per-tenant `device:presence` batching + member-list cache.
    /// See [`crate::ws::device_presence`].
    pub presence_fanout: Arc<crate::ws::device_presence::PresenceFanout>,
    /// C-3 — directory owner-tokens for LOCAL tunnel sessions
    /// (`roomler:own:tunnel:<session>`). Written on open; refreshed by the
    /// directory heartbeat while the session map still holds the session;
    /// never explicitly released — the 90 s TTL reaps ≤90 s after any of
    /// the four teardown paths drops the map entry.
    pub tunnel_presence_tokens: Arc<DashMap<ObjectId, String>>,
    /// C-4 — directory owner-tokens for LOCAL media rooms
    /// (`roomler:own:media:<room>`, the SET-NX namespace). Written when a
    /// claim is won (call/start or media:join); refreshed every 10 s by
    /// the media claim heartbeat; released on room close / shutdown.
    pub media_claim_tokens: Arc<DashMap<ObjectId, String>>,
    /// C-4 — LOCAL viewer connections joined to a REMOTE media room
    /// (conn_id → room). On WS close the owner pod is told to drop the
    /// participant's transports (`ws::media_cluster::forward_close_leave`).
    pub remote_media_conns: Arc<DashMap<String, ObjectId>>,

    // C-1 — cluster foundation (None without Redis; consumers fail soft).
    /// Stable pod identity + process epoch.
    pub pod: crate::cluster::identity::PodIdentity,
    /// Entity → owning-pod records (LWW / NX namespaces).
    pub cluster_directory: Option<crate::cluster::directory::OwnershipDirectory>,
    /// Per-pod request/reply bus.
    pub cluster_bus: Option<Arc<crate::cluster::bus::PodBus>>,

    // roomler-tunnel subsystem
    pub tunnel_clients: Arc<TunnelClientDao>,
    pub tunnel_policies: Arc<TunnelPolicyDao>,
    pub tunnel_audit: Arc<TunnelAuditDao>,
    /// Per-tunnel-session WS outbound channels. See [`TunnelClientOutbound`].
    pub tunnel_clients_by_session: TunnelClientOutbound,
    /// P7 flap resilience: which tunnel sessions TARGET a given agent —
    /// `agent_id → {tunnel_session_id}`. Maintained by `ws::tunnel`'s open +
    /// teardown paths and drained by
    /// [`crate::ws::tunnel::terminate_sessions_targeting_agent`] when the
    /// agent's WS drops: the agent's per-connection tunnel peers died with
    /// its socket, so every session targeting it is unrecoverable and its
    /// client must re-open rather than forward into a corpse forever.
    pub tunnel_sessions_by_target_agent: Arc<DashMap<ObjectId, HashSet<ObjectId>>>,
    /// PR-1 rehome — which tunnel sessions a given agent ORIGINATED
    /// (P3b-2 agent-as-tunnel-client, i.e. declared routes) —
    /// `origin_agent_id → {tunnel_session_id}`. Twin of the by-target
    /// index above, consulted by the `rc.agent_nudge` busy check: the
    /// per-connection session map is invisible here, so without this a
    /// routes-only agent read as IDLE and got its WS cycled (tearing
    /// every declared route plus its overlay carriers).
    pub tunnel_sessions_by_origin_agent: Arc<DashMap<ObjectId, HashSet<ObjectId>>>,
    /// PR-1 rehome — owner-side per-agent nudge pacing (cooldown trio;
    /// see `ws::rc_cluster`). Settings: `rc.nudge_*`.
    pub agent_nudge_cooldowns: Arc<crate::ws::rc_cluster::NudgeCooldowns>,
    /// PR-1 rehome — requester-side per-agent throttle for
    /// `rc.agent_nudge` RPCs (click storms sent 11 in 15 s pre-PR-1).
    pub agent_nudge_throttle: Arc<crate::ws::rc_cluster::NudgeRequestThrottle>,
    /// PR-2 relay — owner-side proxy controllers for cross-pod rc
    /// sessions, keyed by the ORIGIN connection id. See `ws::rc_relay`.
    pub rc_proxy_controllers: Arc<crate::ws::rc_relay::ProxyControllers>,
    /// PR-2 relay — controller-side: conn id → owner pods hosting its
    /// proxied rc sessions (WS close forwards `rc.conn_closed` there;
    /// mirrors C-4's `remote_media_conns`).
    pub remote_rc_conns: Arc<crate::ws::rc_relay::RemoteRcConns>,

    // Overlay-network subsystem (Tailscale-style L3 mesh)
    pub overlay_networks: Arc<OverlayNetworkDao>,
    pub overlay_nodes: Arc<OverlayNodeDao>,
    /// Overlay L3 ACL. Read on every netmap event (join / leave / admin edit),
    /// not per flow — the overlay data plane never touches the server.
    pub overlay_policies: Arc<OverlayPolicyDao>,
    /// Connection-lifetime WS outbound channels for **tunnel-client**
    /// overlay nodes, keyed by `tunnel_client_id` (agent nodes are
    /// reached via [`Hub::send_to_agent`]). Used by the overlay broker
    /// to fan netmaps/deltas to client nodes. Distinct from
    /// `tunnel_clients_by_session`, which is keyed per forward-session.
    pub overlay_nodes_by_id: TunnelClientOutbound,
    /// DERP relay registry: `(network_id, wg_pubkey)` → the outbound frame
    /// sender for that node's live `/derp` WS. The pubkey-addressed forwarding
    /// map for the both-UDP-blocked carrier tier. See [`crate::ws::derp`].
    pub derp_registry: crate::ws::derp::DerpRegistry,
    /// Overlay ACL — per-network relay allow tables consulted by
    /// `ws::derp::forward_frame`. Precomputed because forwarding is a
    /// synchronous per-datagram path; a missing entry fails OPEN. See
    /// [`crate::ws::derp_acl`].
    pub derp_acl: crate::ws::derp_acl::DerpAclCache,
    /// C-5 — per-connection rehome-close signals (cluster convergence).
    pub derp_cancels: crate::ws::derp::DerpCancelRegistry,
    /// C-5 — directory owner-tokens for LOCAL derp registrations
    /// (`roomler:own:derp:<net>:<pubkey>`); refreshed by the directory
    /// heartbeat while the registry holds the key.
    pub derp_presence_tokens: Arc<DashMap<crate::ws::derp::DerpKey, String>>,
    /// C-5 — per-(network, pubkey) rehome pacing (60 s cooldown, 3
    /// attempts / 10 min, then the split-evidence counter).
    pub derp_rehome_cooldowns: Arc<crate::ws::derp_cluster::RehomeCooldowns>,
    /// P7 — per-pair TURN-relay churn state for the forced-DERP escalation,
    /// keyed by the symmetric `pair_key`. Manual TTL (checked on access) +
    /// a size-capped retain sweep — see [`crate::ws::overlay::PairChurn`].
    pub relay_pair_churn: Arc<DashMap<String, crate::ws::overlay::PairChurn>>,

    /// The one GitHub-releases cache, shared by `/api/agent/*`,
    /// `/api/tunnel/*` and `/api/setup/*` — they all read the same
    /// upstream list and only differ in the tag prefix they filter on.
    /// TTL comes from `settings.releases.cache_ttl_secs`;
    /// `POST /api/releases/refresh` busts it cluster-wide on a release.
    /// See `routes::releases` for the lifecycle.
    pub releases_cache: Arc<crate::routes::releases::ReleasesCache>,
}

impl AppState {
    pub async fn new(db: Database, settings: Settings) -> anyhow::Result<Self> {
        let auth = Arc::new(AuthService::new(settings.jwt.clone()));
        let users = Arc::new(UserDao::new(&db));
        let activation_codes = Arc::new(ActivationCodeDao::new(&db));
        let tenants = Arc::new(TenantDao::new(&db));
        let rooms = Arc::new(RoomDao::new(&db));
        let invites = Arc::new(InviteDao::new(&db));
        let messages = Arc::new(MessageDao::new(&db));
        let notifications = Arc::new(NotificationDao::new(&db));
        let reactions = Arc::new(ReactionDao::new(&db));
        let roles = Arc::new(RoleDao::new(&db));
        let files = Arc::new(FileDao::new(&db));
        let recordings = Arc::new(RecordingDao::new(&db));
        let tasks = Arc::new(TaskService::new(&db));

        let worker_pool = Arc::new(WorkerPool::new(&settings.mediasoup).await?);
        let room_manager = Arc::new(RoomManager::new(worker_pool, &settings.mediasoup));

        let ws_storage = Arc::new(WsStorage::new());
        let recognition = RecognitionService::new(
            settings.claude.api_key.clone(),
            settings.claude.model.clone(),
            settings.claude.max_tokens,
        );

        let oauth = if !settings.oauth.google.client_id.is_empty()
            || !settings.oauth.facebook.client_id.is_empty()
            || !settings.oauth.github.client_id.is_empty()
            || !settings.oauth.linkedin.client_id.is_empty()
            || !settings.oauth.microsoft.client_id.is_empty()
        {
            Some(Arc::new(OAuthService::new(settings.oauth.clone())))
        } else {
            None
        };

        // `from_settings` picks SendGrid when `email.api_key` is set
        // (prod), SMTP when `email.smtp_host` + `email.smtp_port` are
        // set (e2e Mailpit), or returns None otherwise (dev / no email).
        let email = EmailService::from_settings(&settings.email).map(Arc::new);

        let push_subscriptions = Arc::new(PushSubscriptionDao::new(&db));
        let push = if !settings.push.vapid_private_key.is_empty() {
            match PushService::new(
                &settings.push.vapid_private_key,
                settings.push.contact.clone(),
            ) {
                Ok(svc) => Some(Arc::new(svc)),
                Err(e) => {
                    tracing::warn!("Failed to initialize push service: {}", e);
                    None
                }
            }
        } else {
            None
        };

        let storage = Arc::new(crate::storage::FileStorage::from_settings(&settings.s3)?);

        // C-1 — cluster identity precedes the Redis layer: the pub/sub
        // origin, the ownership directory and the per-pod bus all speak
        // `<pod_id>/<epoch>`.
        let pod = crate::cluster::identity::PodIdentity::resolve(settings.app.pod_id.clone());
        tracing::info!(pod_id = %pod.pod_id, epoch = %pod.epoch, "cluster identity resolved");

        let redis_pubsub = match RedisPubSub::new(&settings.redis.url, pod.origin()).await {
            Ok(ps) => Some(Arc::new(ps)),
            Err(e) => {
                tracing::warn!(
                    "Failed to initialize Redis Pub/Sub: {} — cross-instance WS delivery disabled",
                    e
                );
                None
            }
        };

        // C-1 — ownership directory + per-pod bus, both riding the
        // pub/sub publisher's connection manager. Absent Redis ⇒ absent
        // cluster layer ⇒ every consumer fails soft to pod-local
        // behavior (the standing degradation rule). Constructed HERE
        // (not main.rs) so two-pod TestApps exercise them.
        let (cluster_directory, cluster_bus) = match &redis_pubsub {
            Some(ps) => (
                Some(crate::cluster::directory::OwnershipDirectory::new(
                    ps.connection(),
                    pod.origin(),
                )),
                Some(crate::cluster::bus::PodBus::start(
                    pod.clone(),
                    ps.connection(),
                    settings.redis.url.clone(),
                )),
            ),
            None => (None, None),
        };

        let giphy = if !settings.giphy.api_key.is_empty() {
            Some(Arc::new(GiphyService::new(settings.giphy.api_key.clone())))
        } else {
            None
        };

        // Remote-control subsystem
        let agents = Arc::new(AgentDao::new(&db));
        let used_tokens = Arc::new(roomler_ai_services::dao::used_token::UsedTokenDao::new(&db));
        let remote_sessions = Arc::new(RemoteSessionDao::new(&db));
        let remote_audit = Arc::new(RemoteAuditDao::new(&db));
        let exec_audit = Arc::new(ExecAuditDao::new(&db));
        let agent_crashes = Arc::new(roomler_ai_services::dao::agent_crash::AgentCrashDao::new(
            &db,
        ));
        let agent_logs = Arc::new(roomler_ai_services::dao::agent_log::AgentLogDao::new(&db));

        let consent_requests = Arc::new(ConsentRequestDao::new(&db));
        let stats = Arc::new(roomler_ai_services::dao::stats::StatsDao::new(&db));
        // Stats PR-3 — malformed allowlist entries are skipped LOUDLY (a
        // silent skip would read as "dashboards mysteriously 404").
        let platform_admins: std::collections::HashSet<ObjectId> = settings
            .stats
            .platform_admins
            .as_deref()
            .unwrap_or("")
            .split(',')
            .filter_map(|s| {
                let s = s.trim();
                if s.is_empty() {
                    return None;
                }
                match ObjectId::parse_str(s) {
                    Ok(id) => Some(id),
                    Err(_) => {
                        tracing::warn!(
                            entry = %s,
                            "ROOMLER__STATS__PLATFORM_ADMINS entry is not a 24-hex ObjectId — skipped"
                        );
                        None
                    }
                }
            })
            .collect();
        if !platform_admins.is_empty() {
            tracing::info!(count = platform_admins.len(), "platform admins configured");
        }
        let platform_admins = Arc::new(platform_admins);

        let turn_map = Arc::new(build_turn_map(&settings));
        // P6b — live per-region load (written by the /stats poller, consulted
        // at issuance). Spawned only when regions are enabled and at least
        // one carries a derp_url (the stats host). The stats sink makes each
        // poll tick durable (`stats_relay` buckets) when stats are enabled.
        let relay_load: roomler_ai_remote_control::turn_creds::RelayLoadMap = Default::default();
        crate::relay_load::spawn_poller(
            turn_map.clone(),
            relay_load.clone(),
            &settings.relay,
            settings.stats.enabled.then(|| stats.clone()),
        );
        // Multi-region DERP tickets: load the signer when a key is configured;
        // log the derived public key so the operator can copy it to PoPs.
        // Regions carrying derp_urls without a key = loud warn, never fatal.
        let derp_ticket = match settings.relay.derp_ticket_private_key.as_deref() {
            Some(key) => {
                match roomler_ai_remote_control::derp_ticket::DerpTicketSigner::from_pkcs8_b64(key)
                {
                    Ok(s) => {
                        tracing::info!(
                            public_key = %s.public_key_b64(),
                            "derp ticket signer loaded — set DERP_TICKET_PUBLIC_KEY to this on every PoP relay"
                        );
                        Some(Arc::new(s))
                    }
                    Err(e) => {
                        tracing::error!(%e, "ROOMLER__RELAY__DERP_TICKET_PRIVATE_KEY is unusable; regional DERP disabled");
                        None
                    }
                }
            }
            None => {
                if turn_map.enabled && turn_map.specs.iter().any(|s| s.derp_url.is_some()) {
                    tracing::warn!(
                        "relay regions carry derp_urls but no ROOMLER__RELAY__DERP_TICKET_PRIVATE_KEY is set — regional DERP disabled"
                    );
                }
                None
            }
        };
        let (audit_sink, _audit_handle) = AuditSink::spawn(db.clone());
        // Phase 4 — owner-side consent: the Hub emits a `ConsentEvent` for each
        // Email/Push session; this consumer resolves the owner + persists a
        // `ConsentRequest` + sends the email / web-push. Wiring `Some(consent_tx)`
        // is what turns those modes on; with `None` (tests) they'd just time out.
        let (consent_tx, consent_rx) = mpsc::channel::<ConsentEvent>(64);
        let rc_hub = Arc::new(Hub::new_with_consent(
            audit_sink,
            (*turn_map).clone(),
            Some(consent_tx),
            relay_load.clone(),
        ));

        // C-1 — the directory heartbeat: every 30 s, re-assert this pod's
        // agent presence records (gated on STILL holding the hub slot).
        // Redundant with the per-received-heartbeat refresh in the socket
        // handler — deliberately: this sweep is the single pattern later
        // stages extend to tunnel/derp/media registries, and it heals a
        // record lost to a Redis flap even while the agent is quiet.
        // A CONFLICT (foreign owner) is the fold signal: log it; the
        // socket-level machinery (displacement Goodbye / A2b counter)
        // owns the reaction.
        let agent_presence_tokens: Arc<DashMap<ObjectId, String>> = Arc::new(DashMap::new());
        let tunnel_presence_tokens: Arc<DashMap<ObjectId, String>> = Arc::new(DashMap::new());
        let media_claim_tokens: Arc<DashMap<ObjectId, String>> = Arc::new(DashMap::new());
        let remote_media_conns: Arc<DashMap<String, ObjectId>> = Arc::new(DashMap::new());
        let derp_presence_tokens: Arc<DashMap<crate::ws::derp::DerpKey, String>> =
            Arc::new(DashMap::new());
        let derp_registry: crate::ws::derp::DerpRegistry = Arc::new(DashMap::new());
        let tunnel_clients_by_session: TunnelClientOutbound = Arc::new(DashMap::new());
        let tunnel_sessions_by_target_agent: Arc<DashMap<ObjectId, HashSet<ObjectId>>> =
            Arc::new(DashMap::new());
        let tunnel_sessions_by_origin_agent: Arc<DashMap<ObjectId, HashSet<ObjectId>>> =
            Arc::new(DashMap::new());
        let agent_nudge_cooldowns: Arc<crate::ws::rc_cluster::NudgeCooldowns> =
            Arc::new(DashMap::new());

        // C-2 — the global-channel subscriber + forwarder, MOVED here from
        // main.rs so two-pod TestApps exercise cross-pod delivery (chat,
        // presence, and the new rc ctrl events). Same reconnect-and-backoff
        // subscription; the forwarder gains the ctrl lane.
        let redis_sub_alive = Arc::new(std::sync::atomic::AtomicBool::new(false));
        if let Some(pubsub) = &redis_pubsub {
            let (redis_tx, _) = tokio::sync::broadcast::channel::<String>(1024);
            let mut redis_rx = redis_tx.subscribe();
            let own_instance = pubsub.instance_id().to_string();
            let fwd_storage = ws_storage.clone();
            let ctrl_hub = rc_hub.clone();
            RedisPubSub::subscribe_with_reconnect(
                settings.redis.url.clone(),
                redis_tx,
                redis_sub_alive.clone(),
            );
            tokio::spawn(async move {
                loop {
                    match redis_rx.recv().await {
                        Ok(payload) => {
                            let Ok(envelope) = serde_json::from_str::<serde_json::Value>(&payload)
                            else {
                                continue;
                            };
                            // Self-echo guard: this instance already delivered
                            // locally at publish time.
                            if envelope["origin"].as_str() == Some(own_instance.as_str()) {
                                continue;
                            }
                            // C-2 — rc control events (consent verdicts, admin
                            // kicks): idempotent hub operations every pod
                            // applies locally; the one holding the entity acts.
                            if let Some(ctrl) = envelope.get("ctrl") {
                                crate::ws::remote_control::apply_rc_ctrl(&ctrl_hub, ctrl);
                                continue;
                            }
                            // C-4 — conn-addressed delivery (media replies +
                            // pushes from a room's owner pod): whichever pod
                            // holds the socket delivers; everyone else drops.
                            if let Some(conn) = envelope["conn"].as_str() {
                                if let Some(message) = envelope.get("message") {
                                    crate::ws::dispatcher::send_to_connection(
                                        &fwd_storage,
                                        conn,
                                        message,
                                    )
                                    .await;
                                }
                                continue;
                            }
                            // S6 broadcast envelopes (presence fan-out): each
                            // pod delivers to its own local connections.
                            if envelope["broadcast"].as_bool() == Some(true) {
                                if let Some(message) = envelope.get("message") {
                                    let ids = fwd_storage.all_user_ids();
                                    crate::ws::dispatcher::broadcast(&fwd_storage, &ids, message)
                                        .await;
                                }
                                continue;
                            }
                            if let (Some(user_ids_val), Some(message)) =
                                (envelope["user_ids"].as_array(), envelope.get("message"))
                            {
                                let ids: Vec<ObjectId> = user_ids_val
                                    .iter()
                                    .filter_map(|v| {
                                        v.as_str().and_then(|s| ObjectId::parse_str(s).ok())
                                    })
                                    .collect();
                                crate::ws::dispatcher::broadcast(&fwd_storage, &ids, message).await;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            tracing::warn!("Redis Pub/Sub forwarder lagged; dropped {n} events");
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
                tracing::error!("Redis Pub/Sub forwarding task ended (channel closed)");
            });
            tracing::info!("Redis Pub/Sub cross-instance WS delivery enabled");

            // S6 — online-registry heartbeat (also moved from main.rs).
            let hb_pubsub = pubsub.clone();
            let hb_storage = ws_storage.clone();
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    tick.tick().await;
                    for uid in hb_storage.all_user_ids() {
                        if let Err(e) = hb_pubsub.online_add(&uid.to_hex()).await {
                            tracing::debug!("online-registry heartbeat failed: {e}");
                            break; // Redis down — retry whole set next tick.
                        }
                    }
                }
            });
        }

        // C-2/PR-1 — the idle-agent nudge handler: the pod OWNING an
        // agent's WS receives `rc.agent_nudge` from a pod whose
        // controller found the agent foreign, and cycles that WS iff the
        // agent is FULLY idle — no rc sessions, no tunnel sessions
        // targeting it, and (PR-1) none it ORIGINATED (declared routes) —
        // so both ends re-land at the current LB hash. PR-1 adds the
        // cooldown trio (a cycle tears the agent's rc/tunnel/overlay
        // planes; it must never flap), truthful refusal reasons on the
        // reply, and refusal/stuck counters.
        if let Some(bus) = &cluster_bus {
            let hub = rc_hub.clone();
            let tunnel_targets = tunnel_sessions_by_target_agent.clone();
            let tunnel_origins = tunnel_sessions_by_origin_agent.clone();
            let cooldowns = agent_nudge_cooldowns.clone();
            let pacing = crate::ws::rc_cluster::NudgePacing {
                cooldown: std::time::Duration::from_secs(settings.rc.nudge_cooldown_secs),
                max_attempts: settings.rc.nudge_max_attempts,
                attempts_reset_after: std::time::Duration::from_secs(
                    settings.rc.nudge_attempts_reset_secs,
                ),
            };
            bus.register("rc.agent_nudge", move |body| {
                let hub = hub.clone();
                let tunnel_targets = tunnel_targets.clone();
                let tunnel_origins = tunnel_origins.clone();
                let cooldowns = cooldowns.clone();
                Box::pin(async move {
                    use roomler_ai_remote_control::hub::NudgeOutcome;
                    let hex = body
                        .get("agent_id")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| "missing agent_id".to_string())?;
                    let aid = ObjectId::parse_str(hex).map_err(|_| "bad agent_id".to_string())?;
                    let target_busy = tunnel_targets
                        .get(&aid)
                        .map(|s| !s.is_empty())
                        .unwrap_or(false);
                    let origin_busy = tunnel_origins
                        .get(&aid)
                        .map(|s| !s.is_empty())
                        .unwrap_or(false);
                    // Gate (peek) -> fire -> book: attempts count FIRED
                    // cycles only, so busy refusals can never trip the
                    // stuck/split-evidence signal.
                    let outcome = if target_busy || origin_busy {
                        NudgeOutcome::ExtraBusy
                    } else {
                        match crate::ws::rc_cluster::nudge_gate(&cooldowns, aid, pacing) {
                            crate::ws::rc_cluster::NudgeGate::Allow => {
                                let o = hub.nudge_agent_if_idle(aid, false);
                                if o == NudgeOutcome::Nudged {
                                    crate::ws::rc_cluster::nudge_book(&cooldowns, aid, pacing);
                                }
                                o
                            }
                            crate::ws::rc_cluster::NudgeGate::Cooldown
                            | crate::ws::rc_cluster::NudgeGate::Stuck => {
                                crate::cluster::metrics::bump(
                                    &crate::cluster::metrics::AGENT_NUDGE_REFUSED_TOTAL,
                                );
                                return Ok(serde_json::json!({
                                    "nudged": false,
                                    "reason": "cooldown",
                                }));
                            }
                        }
                    };
                    if outcome == NudgeOutcome::Nudged {
                        crate::cluster::metrics::bump(&crate::cluster::metrics::AGENT_NUDGE_TOTAL);
                        return Ok(serde_json::json!({ "nudged": true, "reason": "nudged" }));
                    }
                    crate::cluster::metrics::bump(
                        &crate::cluster::metrics::AGENT_NUDGE_REFUSED_TOTAL,
                    );
                    // The truthful reason, at info: pre-PR-1 refusals were
                    // debug-only and the 2026-08-04 stuck loop was
                    // invisible without pod-log spelunking.
                    let reason = if origin_busy && !target_busy {
                        "origin_busy"
                    } else {
                        outcome.reason()
                    };
                    tracing::info!(agent = %aid, reason, "agent nudge refused");
                    Ok(serde_json::json!({ "nudged": false, "reason": reason }))
                })
            });
        }
        // The releases cache is constructed here rather than inline in the
        // struct literal because the bus handler below has to capture it:
        // `POST /api/releases/refresh` lands on ONE pod, and this handler is
        // how the other pods get busted too.
        let releases_cache = crate::routes::releases::ReleasesCache::new();
        if let Some(bus) = &cluster_bus {
            let cache = releases_cache.clone();
            let pod_id = pod.pod_id.clone();
            bus.register(crate::routes::releases::BUS_CLASS_REFRESH, move |body| {
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

        if let Some(dir) = &cluster_directory {
            let dir = dir.clone();
            let hub = rc_hub.clone();
            let tokens = agent_presence_tokens.clone();
            let tunnel_tokens = tunnel_presence_tokens.clone();
            let tunnel_sessions = tunnel_clients_by_session.clone();
            let derp_tokens = derp_presence_tokens.clone();
            let derp_reg = derp_registry.clone();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                    for entry in tokens.iter() {
                        let (agent_id, token) = (*entry.key(), entry.value().clone());
                        if !hub.is_agent_online(agent_id) {
                            continue;
                        }
                        let key = crate::cluster::directory::agent_key(&agent_id.to_hex());
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
                    // C-3 — tunnel session records: refresh while the session
                    // map holds the session (its four teardown paths all drop
                    // the map entry, which stops the refresh here; the 90 s
                    // TTL then reaps the record — no explicit release).
                    let dead: Vec<ObjectId> = tunnel_tokens
                        .iter()
                        .filter(|e| !tunnel_sessions.contains_key(e.key()))
                        .map(|e| *e.key())
                        .collect();
                    for sid in dead {
                        tunnel_tokens.remove(&sid);
                    }
                    for entry in tunnel_tokens.iter() {
                        let (sid, token) = (*entry.key(), entry.value().clone());
                        let key = crate::cluster::directory::tunnel_key(&sid.to_hex());
                        if let Err(e) = dir.refresh_if_mine(&key, &token, 90).await {
                            tracing::debug!(session = %sid, %e, "tunnel directory refresh failed");
                        }
                    }
                    // C-5 — derp registration records: prune tokens whose
                    // registry entry is gone (socket closed / displaced),
                    // refresh the rest. A CONFLICT here just means the
                    // node re-registered on another pod (LWW) while our
                    // stale socket lingers — nothing to fold.
                    let dead: Vec<crate::ws::derp::DerpKey> = derp_tokens
                        .iter()
                        .filter(|e| !derp_reg.contains_key(e.key()))
                        .map(|e| *e.key())
                        .collect();
                    for k in dead {
                        derp_tokens.remove(&k);
                    }
                    for entry in derp_tokens.iter() {
                        let ((net, pk), token) = (*entry.key(), entry.value().clone());
                        let key = crate::cluster::directory::derp_key(
                            &net.to_hex(),
                            &crate::ws::derp_cluster::pk_hex(&pk),
                        );
                        if let Err(e) = dir.refresh_if_mine(&key, &token, 90).await {
                            tracing::debug!(network = %net, %e, "derp directory refresh failed");
                        }
                    }
                }
            });
        }
        spawn_consent_consumer(
            consent_rx,
            ConsentConsumerDeps {
                agents: agents.clone(),
                users: users.clone(),
                consent_requests: consent_requests.clone(),
                push_subscriptions: push_subscriptions.clone(),
                email: email.clone(),
                push: push.clone(),
                base_url: settings.oauth.base_url.clone(),
                notifications: notifications.clone(),
                ws_storage: ws_storage.clone(),
                redis_pubsub: redis_pubsub.clone(),
            },
        );

        // roomler-tunnel subsystem
        let tunnel_clients = Arc::new(TunnelClientDao::new(&db));
        let tunnel_policies = Arc::new(TunnelPolicyDao::new(&db));
        let tunnel_audit = Arc::new(TunnelAuditDao::new(&db));

        // Overlay-network subsystem
        // P2b — carving is opt-in per deployment. With the flag off the DAO
        // behaves exactly as it did pre-P2b (no registry reads at all).
        let overlay_networks = Arc::new(
            OverlayNetworkDao::new(&db).with_block_prefix(
                settings
                    .overlay
                    .blocks_enabled
                    .then_some(settings.overlay.block_prefix),
            ),
        );
        let overlay_nodes = Arc::new(OverlayNodeDao::new(&db));
        let overlay_policies = Arc::new(OverlayPolicyDao::new(&db));

        let state = Self {
            db,
            settings,
            pod,
            cluster_directory,
            cluster_bus,
            auth,
            users,
            activation_codes,
            tenants,
            rooms,
            invites,
            messages,
            notifications,
            reactions,
            roles,
            files,
            recordings,

            tasks,
            room_manager,
            ws_storage,
            recognition,
            oauth,
            giphy,
            email,
            push,
            push_subscriptions,
            redis_pubsub,
            redis_sub_alive,
            storage,
            agents,
            used_tokens,
            remote_sessions,
            remote_audit,
            exec_audit,
            agent_crashes,
            agent_logs,
            stats,
            platform_admins,
            consent_requests,
            rc_hub,
            turn_map,
            relay_load,
            derp_ticket,
            agent_presence_tokens,
            presence_fanout: Arc::new(crate::ws::device_presence::PresenceFanout::default()),
            tunnel_presence_tokens: tunnel_presence_tokens.clone(),
            media_claim_tokens,
            remote_media_conns,
            tunnel_clients,
            tunnel_policies,
            tunnel_audit,
            tunnel_clients_by_session: tunnel_clients_by_session.clone(),
            tunnel_sessions_by_target_agent: tunnel_sessions_by_target_agent.clone(),
            tunnel_sessions_by_origin_agent: tunnel_sessions_by_origin_agent.clone(),
            agent_nudge_cooldowns: agent_nudge_cooldowns.clone(),
            agent_nudge_throttle: Arc::new(DashMap::new()),
            rc_proxy_controllers: Arc::new(DashMap::new()),
            remote_rc_conns: Arc::new(DashMap::new()),
            overlay_networks,
            overlay_nodes,
            overlay_policies,
            overlay_nodes_by_id: Arc::new(DashMap::new()),
            derp_registry: derp_registry.clone(),
            derp_acl: Arc::new(DashMap::new()),
            derp_cancels: Arc::new(DashMap::new()),
            derp_presence_tokens: derp_presence_tokens.clone(),
            derp_rehome_cooldowns: Arc::new(DashMap::new()),
            relay_pair_churn: Arc::new(DashMap::new()),
            releases_cache,
        };

        // C-4 — media claim-or-route: bus handlers (owner-side command
        // execution) + the 10 s claim heartbeat. Registered on the built
        // state because the handlers need the full AppState.
        crate::ws::media_cluster::wire_media_cluster(&state);
        // C-5 — derp rehome: the owner-side close handler.
        crate::ws::derp_cluster::wire_derp_cluster(&state);
        // PR-2 — cross-pod rc signalling relay: owner-side rc.cmd /
        // rc.conn_closed / rc.conn_alive + the proxy janitor sweep.
        crate::ws::rc_relay::wire_rc_relay(&state);
        // P4 — the presence staleness sweep (cluster-singleton per cycle
        // via a DB-name-scoped Redis NX claim; first tick a full interval
        // out so tests driving `run_presence_sweep` directly stay
        // deterministic).
        crate::ws::device_presence::spawn_sweeper(state.clone());
        // Stats PR-1 — the rollup compactor (raw → _1h → _1d), cluster-
        // singleton per cycle via the same DB-name-scoped claim pattern.
        // No-op task when `stats.enabled=false`.
        crate::stats_rollup::spawn_stats_rollup(state.clone());
        // Stats PR-2 — per-pod mediasoup conference sampler (this pod's
        // own rooms only; media ownership is single-pod per room).
        crate::media_stats::spawn_media_sampler(state.clone());

        Ok(state)
    }
}

/// Build a [`TurnConfig`] from settings. Returns `None` when `shared_secret` is
/// absent (e.g. dev environments using static username/password instead).
/// `pub(crate)` so the tunnel WS handler (`ws/tunnel.rs`) can mint
/// per-session QUIC-over-TURN creds the same way (Phase 3c).
pub(crate) fn build_turn_config(turn: &roomler_ai_config::TurnSettings) -> Option<TurnConfig> {
    let secret = turn.shared_secret.as_ref()?.clone();
    let base = turn.url.as_deref()?;

    // Same-worker TURN affinity (2026-07-14): optional comma-separated
    // per-worker base URLs, each expanded into the same transport variants
    // as the generic hostname. The Hub then pins BOTH sides of a session to
    // one worker (see `turn_creds::issue_for_session`) — the generic
    // hostname is 3 DNS A records, so without this each ICE side resolves
    // independently and relay↔relay sessions straddle two coturn workers.
    // Unset → empty → exactly the old single-hostname behaviour.
    let workers: Vec<Vec<String>> = turn
        .worker_urls
        .as_deref()
        .map(|s| {
            s.split(',')
                .map(str::trim)
                .filter(|w| !w.is_empty())
                .map(|w| expand_turn_url(w, &VariantCaps::default()))
                .collect()
        })
        .unwrap_or_default();

    Some(TurnConfig {
        urls: expand_turn_url(base, &VariantCaps::default()),
        workers,
        shared_secret: secret,
        ttl_secs: turn.ttl_secs.unwrap_or(600),
    })
}

/// Build the region-keyed [`TurnMap`]: the legacy `turn.*` config as the
/// default region plus one [`TurnConfig`] per enabled spec in
/// `ROOMLER__RELAY__REGIONS`. A malformed JSON or a region without any usable
/// shared secret is logged and skipped — never fatal, and with
/// `relay.regions_enabled=false` the map degrades to exactly the legacy
/// behaviour.
pub(crate) fn build_turn_map(settings: &Settings) -> TurnMap {
    use roomler_ai_remote_control::turn_creds::RelayRegionSpec;

    let default = build_turn_config(&settings.turn);
    let ttl_secs = settings.turn.ttl_secs.unwrap_or(600);
    let mut regions = std::collections::HashMap::new();
    let mut specs: Vec<RelayRegionSpec> = Vec::new();
    if let Some(json) = settings.relay.regions.as_deref() {
        match serde_json::from_str::<Vec<RelayRegionSpec>>(json) {
            Ok(list) => {
                for spec in list {
                    if !spec.enabled {
                        specs.push(spec);
                        continue;
                    }
                    let Some(secret) = spec
                        .shared_secret
                        .clone()
                        .or_else(|| settings.turn.shared_secret.clone())
                    else {
                        tracing::warn!(
                            region = %spec.id,
                            "relay region has no shared secret (own or global turn.shared_secret); skipping"
                        );
                        continue;
                    };
                    regions.insert(
                        spec.id.clone(),
                        TurnConfig {
                            urls: expand_turn_url(&spec.turn_url, &spec.caps),
                            workers: spec
                                .worker_urls
                                .iter()
                                .map(|w| expand_turn_url(w, &spec.caps))
                                .collect(),
                            shared_secret: secret,
                            ttl_secs,
                        },
                    );
                    specs.push(spec);
                }
            }
            Err(e) => {
                tracing::error!(%e, "ROOMLER__RELAY__REGIONS is not valid JSON; ignoring regions");
            }
        }
    }
    if settings.relay.regions_enabled && regions.is_empty() {
        tracing::warn!(
            "relay.regions_enabled=true but no usable regions parsed — all issuance stays on the default region"
        );
    }
    TurnMap {
        default,
        regions,
        specs,
        enabled: settings.relay.regions_enabled,
    }
}

/// Dependencies the Phase-4 owner-consent consumer needs — cheap `Arc` clones of
/// the relevant DAOs / services, captured when [`AppState`] is built.
struct ConsentConsumerDeps {
    agents: Arc<AgentDao>,
    users: Arc<UserDao>,
    consent_requests: Arc<ConsentRequestDao>,
    push_subscriptions: Arc<PushSubscriptionDao>,
    email: Option<Arc<EmailService>>,
    push: Option<Arc<PushService>>,
    base_url: String,
    // P4 — the owner also gets an IN-APP notification row + `notification:new`
    // WS push (the email/web-push above are useless when the owner is sitting
    // in the app on another org's page).
    notifications: Arc<NotificationDao>,
    ws_storage: Arc<WsStorage>,
    redis_pubsub: Option<Arc<RedisPubSub>>,
}

/// P4 — persist an in-app Notification for the device owner and push it over
/// WS (`notification:new`, same payload shape as `routes::helpers`). Consent
/// requests carry the approve/deny page as their link; break-glass notices
/// link the device list. Best-effort — the email/push legs stay authoritative.
async fn consent_in_app_notification(
    deps: &ConsentConsumerDeps,
    ev: &ConsentEvent,
    owner_id: bson::oid::ObjectId,
    title: String,
    body: String,
    link: String,
) {
    let created = deps
        .notifications
        .create(
            ev.tenant_id,
            owner_id,
            roomler_ai_db::models::NotificationType::ConsentRequest,
            title,
            body,
            Some(link),
            roomler_ai_db::models::NotificationSource {
                entity_type: "remote_session".to_string(),
                entity_id: ev.session_id,
                actor_id: Some(ev.controller_user_id),
            },
        )
        .await;
    match created {
        Ok(n) => {
            let event = serde_json::json!({
                "type": "notification:new",
                "data": {
                    "id": n.id.map(|i| i.to_hex()).unwrap_or_default(),
                    "tenant_id": ev.tenant_id.to_hex(),
                    "title": n.title,
                    "body": n.body,
                    "link": n.link,
                    "notification_type": "consent_request",
                    "created_at": n.created_at.try_to_rfc3339_string().unwrap_or_default(),
                }
            });
            crate::ws::dispatcher::send_to_user_with_redis(
                &deps.ws_storage,
                &deps.redis_pubsub,
                &owner_id,
                &event,
            )
            .await;
        }
        Err(e) => {
            tracing::warn!(session = %ev.session_id, %e, "consent in-app notification failed");
        }
    }
}

/// Spawn the background task that turns Hub [`ConsentEvent`]s (Email/Push sessions
/// awaiting the device owner) into a `ConsentRequest` row + an email / web-push
/// carrying the approve-link. One task for the process lifetime; a per-event
/// failure is logged, never fatal.
fn spawn_consent_consumer(mut rx: mpsc::Receiver<ConsentEvent>, deps: ConsentConsumerDeps) {
    tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            if let Err(e) = handle_consent_event(&deps, &ev).await {
                tracing::warn!(session = %ev.session_id, %e, "owner-consent notification failed");
            }
        }
    });
}

async fn handle_consent_event(deps: &ConsentConsumerDeps, ev: &ConsentEvent) -> anyhow::Result<()> {
    // Resolve the device owner + display name (the Hub is DB-agnostic, so it
    // only knows the agent_id).
    let agent = deps.agents.base.find_by_id(ev.agent_id).await?;
    let owner_id = agent.owner_user_id;
    let device_name = agent.name.clone();

    // Phase 5 — break-glass NOTICE: an admin already forced the session, so this
    // is informational (no approval, no ConsentRequest). Tell the owner their
    // device was accessed + why, then we're done.
    if let Some(reason) = &ev.override_reason {
        consent_in_app_notification(
            deps,
            ev,
            owner_id,
            "Device accessed (admin override)".to_string(),
            format!(
                "{} accessed {} via admin break-glass. Reason: {}",
                ev.controller_name, device_name, reason
            ),
            format!("/tenant/{}/devices", ev.tenant_id.to_hex()),
        )
        .await;
        if let Some(email) = &deps.email {
            let owner = deps.users.base.find_by_id(owner_id).await?;
            let _ = email
                .send_override_notice(&owner.email, &ev.controller_name, &device_name, reason)
                .await;
        }
        if let Some(push) = &deps.push {
            let subs = deps
                .push_subscriptions
                .find_by_user(owner_id)
                .await
                .unwrap_or_default();
            let body = format!(
                "{} accessed {} via admin break-glass. Reason: {}",
                ev.controller_name, device_name, reason
            );
            for sub in subs {
                let _ = push
                    .send(
                        &sub.endpoint,
                        &sub.keys.auth,
                        &sub.keys.p256dh,
                        "Device accessed (admin override)",
                        &body,
                        None,
                    )
                    .await;
            }
        }
        return Ok(());
    }

    // Persist the request with a fresh capability token + a TTL that matches the
    // session's consent window (a stale link can't resolve a long-gone session).
    let req = deps
        .consent_requests
        .create(
            ev.tenant_id,
            ev.session_id,
            ev.agent_id,
            ev.controller_user_id,
            ev.controller_name.clone(),
            owner_id,
            ev.timeout_secs as i64,
        )
        .await?;

    let consent_url = format!(
        "{}/consent/{}",
        deps.base_url.trim_end_matches('/'),
        req.token
    );

    // P4 — in-app row + WS for the owner alongside the email/push leg. The
    // link is the RELATIVE approve/deny page (in-app navigation).
    consent_in_app_notification(
        deps,
        ev,
        owner_id,
        "Remote control request".to_string(),
        format!("{} wants to control {}", ev.controller_name, device_name),
        format!("/consent/{}", req.token),
    )
    .await;

    match ev.mode {
        // Email + PromptThenEmail both email the owner an approve-link. For
        // PromptThenEmail the agent ALSO prompts on the host in parallel — either
        // the person at the console or the owner via the link can approve, first
        // wins (both resolve the same slot within the shared timeout).
        ConsentMode::Email | ConsentMode::PromptThenEmail => {
            let owner = deps.users.base.find_by_id(owner_id).await?;
            match &deps.email {
                Some(email) => {
                    email
                        .send_consent_request(
                            &owner.email,
                            &ev.controller_name,
                            &device_name,
                            &consent_url,
                        )
                        .await?;
                }
                None => tracing::warn!(
                    session = %ev.session_id,
                    "Email consent mode but no email service is configured — owner cannot approve"
                ),
            }
        }
        ConsentMode::Push => match &deps.push {
            Some(push) => {
                let subs = deps.push_subscriptions.find_by_user(owner_id).await?;
                if subs.is_empty() {
                    tracing::warn!(
                        session = %ev.session_id,
                        "Push consent mode but the owner has no push subscriptions"
                    );
                }
                let title = "Remote control request";
                let body = format!("{} wants to control {}", ev.controller_name, device_name);
                for sub in subs {
                    // Best-effort per subscription (a stale endpoint shouldn't
                    // block the others).
                    let _ = push
                        .send(
                            &sub.endpoint,
                            &sub.keys.auth,
                            &sub.keys.p256dh,
                            title,
                            &body,
                            Some(&consent_url),
                        )
                        .await;
                }
            }
            None => tracing::warn!(
                session = %ev.session_id,
                "Push consent mode but no push service is configured — owner cannot approve"
            ),
        },
        // The Hub only emits events for Email/Push; other modes never reach here.
        _ => {}
    }

    Ok(())
}

/// Phase A-1 graceful shutdown (SIGTERM/CTRL-C): make this pod's death
/// honest BEFORE the process exits, so a roll never strands
/// `agents.status = 'Online'` rows + stale presence claims (the
/// green-but-dead badge class). Budget: well under the ~3 s drain window
/// `main.rs` allows.
///
/// Mechanics: fire every registered agent's displacement-cancel notify —
/// each read loop exits within milliseconds and runs its OWN teardown
/// (identity-gated unregister, mark-offline, presence compare-DEL); the
/// bulk writes below are belt-and-braces for sockets that don't finish
/// in time. Agents see a plain socket close (never a Goodbye — those are
/// fatal client-side) and reconnect with backoff through the LB.
pub async fn shutdown_cleanup(state: &AppState) {
    // C-4/C-6 — release directory records for every locally-owned class
    // FIRST (before the agent sweep's early return): a graceful deploy
    // hands each entity off with a ZERO-length ownerless window instead
    // of waiting out the TTLs (media 30 s, tunnel/derp 90 s).
    if let Some(dir) = &state.cluster_directory {
        let held: Vec<(bson::oid::ObjectId, String)> = state
            .media_claim_tokens
            .iter()
            .map(|e| (*e.key(), e.value().clone()))
            .collect();
        for (rid, token) in held {
            state.media_claim_tokens.remove(&rid);
            if let Err(e) = dir
                .release(&crate::cluster::directory::media_key(&rid.to_hex()), &token)
                .await
            {
                tracing::debug!(room = %rid, %e, "shutdown: media claim release failed");
            }
        }
        // C-6 — tunnel session records (their sessions die with this
        // pod; the CLI redials and re-opens on the survivor).
        let held: Vec<(bson::oid::ObjectId, String)> = state
            .tunnel_presence_tokens
            .iter()
            .map(|e| (*e.key(), e.value().clone()))
            .collect();
        for (sid, token) in held {
            state.tunnel_presence_tokens.remove(&sid);
            let _ = dir
                .release(
                    &crate::cluster::directory::tunnel_key(&sid.to_hex()),
                    &token,
                )
                .await;
        }
        // C-6 — derp registrations (+ the per-network member index, so
        // the survivor's convergence sweep sees a clean roster).
        let held: Vec<(crate::ws::derp::DerpKey, String)> = state
            .derp_presence_tokens
            .iter()
            .map(|e| (*e.key(), e.value().clone()))
            .collect();
        for ((net, pk), token) in held {
            state.derp_presence_tokens.remove(&(net, pk));
            let net_hex = net.to_hex();
            let member = crate::ws::derp_cluster::pk_hex(&pk);
            if let Ok(true) = dir
                .release(
                    &crate::cluster::directory::derp_key(&net_hex, &member),
                    &token,
                )
                .await
            {
                let _ = dir
                    .set_remove(&crate::cluster::directory::derpnet_key(&net_hex), &member)
                    .await;
            }
        }
    }

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
        .mark_status_many(
            &ids,
            roomler_ai_remote_control::models::AgentStatus::Offline,
        )
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
