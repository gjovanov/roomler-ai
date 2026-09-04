// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
use std::collections::HashSet;

use bson::oid::ObjectId;
use dashmap::DashMap;
use mongodb::Database;
use roomler_ai_config::Settings;
use roomler_ai_remote_control::{
    signaling::ServerMsg,
    turn_creds::{TurnConfig, TurnMap},
    turn_url::{VariantCaps, expand_turn_url},
};
use roomler_ai_services::{
    AuthService, EmailService, OAuthService, PushService, TaskService,
    dao::{
        activation_code::ActivationCodeDao, agent::AgentDao, config_audit::ConfigAuditDao,
        consent_request::ConsentRequestDao, exec_audit::ExecAuditDao, invite::InviteDao,
        key_rotation_audit::KeyRotationAuditDao, notification::NotificationDao,
        overlay_network::OverlayNetworkDao, overlay_node::OverlayNodeDao,
        overlay_policy::OverlayPolicyDao, peer_relay_audit::PeerRelayAuditDao,
        push_subscription::PushSubscriptionDao, role::RoleDao, ssh_activity::SshActivityDao,
        ssh_audit::SshAuditDao, tenant::TenantDao, tunnel_audit::TunnelAuditDao,
        tunnel_client::TunnelClientDao, tunnel_policy::TunnelPolicyDao, user::UserDao,
    },
};
use tokio::sync::mpsc;

use std::sync::Arc;

use crate::core_state::Core;
use crate::ws::redis_pubsub::RedisPubSub;
use crate::ws::storage::WsStorage;

/// Outbound channel for a connected `roomler` client, keyed by
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
    /// FR-69 P1 — the 27 core fields (identity, tenancy, notifications,
    /// storage, the socket registry, the cluster bus, TURN credentials,
    /// metering). `AppState` derefs to it, so `state.settings`,
    /// `state.users` and the rest read exactly as before, while the
    /// module-owned fields below wait for their module PRs. See
    /// [`crate::core_state`].
    pub core: Core,

    /// FR-69 P2 — the module crates this build links, initialised (or `None`
    /// where the operator switched one off). Mounted by `build_router`;
    /// their index sets applied by the host after the core plan. P4 — the
    /// mediasoup room manager, the recordings DAO and the media cluster maps
    /// live on `modules.conference` now; the host holds no room state.
    pub modules: crate::compose::Modules,

    // Remote-control subsystem
    pub agents: Arc<AgentDao>,
    /// FR-51 P2 — reusable ephemeral enrollment keys + their per-use audit.
    pub enrollment_keys: Arc<roomler_ai_services::dao::enrollment_key::EnrollmentKeyDao>,
    pub remote_sessions: Arc<RemoteSessionDao>,
    pub remote_audit: Arc<RemoteAuditDao>,
    /// Fleet-RPC attempt log — every exec, allowed or denied.
    pub exec_audit: Arc<ExecAuditDao>,
    /// Roomler-SSH grant log — every session request, granted or refused.
    pub ssh_audit: Arc<SshAuditDao>,
    /// Remote-config decisions (`docs/remote-config.md`): what was ASKED for
    /// on a device, granted or refused — never what the device did.
    pub config_audit: Arc<ConfigAuditDao>,
    /// FR-40 — overlay-key rotation orders, dispatched or refused.
    pub key_rotation_audit: Arc<KeyRotationAuditDao>,
    /// FR-19 peer-relay decisions: approvals (who made a device a relay) and
    /// mints (what the server routed through it), granted or refused.
    pub peer_relay_audit: Arc<PeerRelayAuditDao>,
    /// P8 — device-reported session activity. Separate from `ssh_audit`
    /// because one is the server's decision and the other is a claim by the
    /// device; see `SshActivityEvent`.
    pub ssh_activity: Arc<SshActivityDao>,
    pub agent_crashes: Arc<roomler_ai_services::dao::agent_crash::AgentCrashDao>,
    pub agent_logs: Arc<roomler_ai_services::dao::agent_log::AgentLogDao>,
    /// Phase 4 — owner-side consent requests (email/push approve-link tokens).
    pub consent_requests: Arc<ConsentRequestDao>,
    pub rc_hub: Arc<roomler_ai_mod_fleet::Hub>,
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

    // tunnel subsystem
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
    /// Per-(caller, device) ceilings for the exec / SSH control planes. Shared
    /// by the HTTP routes and the device-originated WS legs — both funnel
    /// through the same `authorize`, so neither transport is unlimited.
    pub exec_rate_limiter: Arc<crate::rate_limit::RateLimiter>,
    pub ssh_rate_limiter: Arc<crate::rate_limit::RateLimiter>,
    /// FR-19 — per-(requesting node, relay node) mint ceiling
    /// (`peer_relay_limits::MINT_RATE_LIMIT_PER_MINUTE`), checked by the mint
    /// in `ws::overlay` AFTER the identity gates so a refusal is attributable.
    pub relay_rate_limiter: Arc<crate::rate_limit::RateLimiter>,
    /// FR-40 — one rotation order per device per minute. Keyed on the DEVICE
    /// (the limiter's `caller` slot carries the device id too): a second
    /// admin clicking inside the window is the same storm, not a new budget.
    pub key_rotation_rate_limiter: Arc<crate::rate_limit::RateLimiter>,
    /// FR-19 — the org-relay mint's pod-local state: minted sessions (keyed
    /// by pair), per-relay VNI cursors, the server-wide generation clock,
    /// join extras (primary-org flag, relay port) and reachability reports.
    /// Pod-local is correct here for the same reason `relay_pair_churn` is:
    /// tenant affinity puts a tenant's nodes on ONE pod, and the relay's own
    /// table is the truth a restart falls back on (sessions then run out
    /// their `max_lifetime`).
    pub org_relay: Arc<crate::ws::org_relay::OrgRelayState>,

    /// The one GitHub-releases cache, shared by `/api/agent/*`,
    /// `/api/tunnel/*` and `/api/setup/*` — they all read the same
    /// upstream list and only differ in the tag prefix they filter on.
    /// TTL comes from `settings.releases.cache_ttl_secs`;
    /// `POST /api/releases/refresh` busts it cluster-wide on a release.
    /// See `routes::releases` for the lifecycle.
    pub releases_cache: Arc<roomler_ai_mod_fleet::releases::ReleasesCache>,
}

/// FR-69 P1 — every `state.<core field>` in the tree keeps reading through
/// this. Transitional on purpose: when the last module-owned field has left
/// `AppState` (P7), the host state IS the core and this goes with it.
impl std::ops::Deref for AppState {
    type Target = Core;

    fn deref(&self) -> &Core {
        &self.core
    }
}

impl AppState {
    /// FR-69 P5a — the fleet module's state, for the host code that still
    /// serves the agent socket from it. Always mounted: `AppState::new`
    /// refuses to boot without it (the module cannot be switched off while
    /// the socket lives here — the P5 kill switch is the previous tag).
    pub fn fleet(&self) -> &roomler_ai_mod_fleet::FleetState {
        self.modules
            .fleet
            .as_ref()
            .expect("the fleet module is mounted (AppState::new refuses to boot without it)")
    }
}

impl AppState {
    pub async fn new(db: Database, settings: Settings) -> anyhow::Result<Self> {
        let auth = Arc::new(AuthService::new(settings.jwt.clone()));
        // A secret rotation is otherwise invisible in the record. The correct
        // shape is `verify_keys` going 1 → 2 while `signing_kid` changes; a new
        // `signing_kid` with `verify_keys=1` is the flag-day mistake — every
        // live token, including every agent's one-year token, just died.
        {
            let (signing_kid, verify_keys) = auth.key_summary();
            tracing::info!(%signing_kid, verify_keys, "jwt: signing key");
        }
        let users = Arc::new(UserDao::new(&db));
        let activation_codes = Arc::new(ActivationCodeDao::new(&db));
        let tenants = Arc::new(TenantDao::new(&db));
        let invites = Arc::new(InviteDao::new(&db));
        let notifications = Arc::new(NotificationDao::new(&db));
        let roles = Arc::new(RoleDao::new(&db));
        let tasks = Arc::new(TaskService::new(&db));

        let ws_storage = Arc::new(WsStorage::new());

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

        // Remote-control subsystem
        let used_tokens = Arc::new(roomler_ai_services::dao::used_token::UsedTokenDao::new(&db));
        let ssh_audit = Arc::new(SshAuditDao::new(&db));
        let key_rotation_audit = Arc::new(KeyRotationAuditDao::new(&db));
        let peer_relay_audit = Arc::new(PeerRelayAuditDao::new(&db));
        let ssh_activity = Arc::new(SshActivityDao::new(&db));
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
        let geoip = Arc::new(crate::user_analytics::GeoIp::open(
            settings.stats.geoip_mmdb.as_deref(),
        ));

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
            // FR-20 B — the network → tenant resolver. Gated on the same flag:
            // stats off ⇒ no metering, and the load poller is unaffected.
            //
            // A plain DAO rather than the configured `overlay_networks` below:
            // that one is constructed later in this function AND carries the
            // block-prefix allocator config, which only the ALLOCATION paths
            // need. The poller does exactly one read — `_id` → `tenant_id` —
            // so a read-only instance is both sufficient and honest about it.
            settings
                .stats
                .enabled
                .then(|| Arc::new(OverlayNetworkDao::new(&db))),
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

        // C-1 — the directory heartbeat: every 30 s, re-assert this pod's
        // agent presence records (gated on STILL holding the hub slot).
        // Redundant with the per-received-heartbeat refresh in the socket
        // handler — deliberately: this sweep is the single pattern later
        // stages extend to tunnel/derp/media registries, and it heals a
        // record lost to a Redis flap even while the agent is quiet.
        // A CONFLICT (foreign owner) is the fold signal: log it; the
        // socket-level machinery (displacement Goodbye / A2b counter)
        // owns the reaction.
        let tunnel_presence_tokens: Arc<DashMap<ObjectId, String>> = Arc::new(DashMap::new());
        let derp_presence_tokens: Arc<DashMap<crate::ws::derp::DerpKey, String>> =
            Arc::new(DashMap::new());
        let derp_registry: crate::ws::derp::DerpRegistry = Arc::new(DashMap::new());
        let tunnel_clients_by_session: TunnelClientOutbound = Arc::new(DashMap::new());
        let tunnel_sessions_by_target_agent: Arc<DashMap<ObjectId, HashSet<ObjectId>>> =
            Arc::new(DashMap::new());
        let tunnel_sessions_by_origin_agent: Arc<DashMap<ObjectId, HashSet<ObjectId>>> =
            Arc::new(DashMap::new());

        // C-2 — the global-channel subscriber + forwarder, MOVED here from
        // main.rs so two-pod TestApps exercise cross-pod delivery (chat,
        // presence, and the new rc ctrl events). Same reconnect-and-backoff
        // subscription; the forwarder gains the ctrl lane.
        let redis_sub_alive = Arc::new(std::sync::atomic::AtomicBool::new(false));

        // FR-69 P1/P5a — the core, then the modules, BEFORE the host tasks
        // below: the fleet module's init builds the Hub and the presence and
        // nudge maps that the global-channel subscriber, the nudge handler
        // and the tail spawns capture. The core's fields are cloned in
        // (every one is an `Arc`, a handle, or `Settings` once), so the
        // locals stay usable for the rest of the constructor.
        let core = Core {
            db: db.clone(),
            settings: settings.clone(),
            auth: auth.clone(),
            users: users.clone(),
            activation_codes: activation_codes.clone(),
            tenants: tenants.clone(),
            invites: invites.clone(),
            roles: roles.clone(),
            notifications: notifications.clone(),
            oauth: oauth.clone(),
            email: email.clone(),
            push: push.clone(),
            push_subscriptions: push_subscriptions.clone(),
            used_tokens: used_tokens.clone(),
            tasks: tasks.clone(),
            ws_storage: ws_storage.clone(),
            redis_pubsub: redis_pubsub.clone(),
            redis_sub_alive: redis_sub_alive.clone(),
            storage: storage.clone(),
            stats: stats.clone(),
            platform_admins: platform_admins.clone(),
            geoip: geoip.clone(),
            pod: pod.clone(),
            cluster_directory: cluster_directory.clone(),
            cluster_bus: cluster_bus.clone(),
            turn_map: turn_map.clone(),
            relay_load: relay_load.clone(),
            hooks: roomler_core::HookRegistry::default(),
            agent_socket: roomler_core::AgentSocketRegistry::default(),
        };
        // FR-69 P2 — the module crates, after the core and before the host
        // state that mounts them.
        let modules = crate::compose::Modules::init(core.clone(), &core.settings).await?;
        // P5a — the host serves the agent socket from the fleet module's
        // handles until P5b moves the socket; a build that switches fleet
        // off has nothing to serve it from, so it refuses to boot rather
        // than come up with a dead socket.
        let fleet = modules.fleet.clone().ok_or_else(|| {
            anyhow::anyhow!(
                "[modules] fleet = false is not supported while the host serves the agent \
                 socket (FR-69 P5a) — redeploy the previous tag instead"
            )
        })?;
        // #1186 — `overlay_removes` ctrl envelopes need the FULL state to
        // re-fan (a Mongo peer read + the overlay send paths), which this
        // subscriber closure cannot capture (it is spawned mid-construction;
        // `apply_rc_ctrl` is hub-only for the same reason). Ferry them to an
        // applier task spawned once the state exists. Bounded + try_send:
        // losing one under pressure costs a push the peer's rejoin heals.
        let (overlay_ctrl_tx, overlay_ctrl_rx) =
            tokio::sync::mpsc::channel::<serde_json::Value>(256);
        if let Some(pubsub) = &redis_pubsub {
            let (redis_tx, _) = tokio::sync::broadcast::channel::<String>(1024);
            let mut redis_rx = redis_tx.subscribe();
            let own_instance = pubsub.instance_id().to_string();
            let fwd_storage = ws_storage.clone();
            let ctrl_hub = fleet.rc_hub.clone();
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
                                // #1186 — the one state-needing ctrl event
                                // rides its own channel to the post-
                                // construction applier.
                                if ctrl.get("evt").and_then(|v| v.as_str())
                                    == Some("overlay_removes")
                                {
                                    let _ = overlay_ctrl_tx.try_send(ctrl.clone());
                                    continue;
                                }
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
            let hub = fleet.rc_hub.clone();
            let tunnel_targets = tunnel_sessions_by_target_agent.clone();
            let tunnel_origins = tunnel_sessions_by_origin_agent.clone();
            let cooldowns = fleet.agent_nudge_cooldowns.clone();
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
                    use roomler_ai_mod_fleet::hub::NudgeOutcome;
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

        // C-3/C-5 — the directory heartbeat for the host's own classes
        // (tunnel sessions, derp registrations). The agent-presence half is
        // the fleet module's (FR-69 P5a), spawned by its init.
        if let Some(dir) = &cluster_directory {
            let dir = dir.clone();
            let tunnel_tokens = tunnel_presence_tokens.clone();
            let tunnel_sessions = tunnel_clients_by_session.clone();
            let derp_tokens = derp_presence_tokens.clone();
            let derp_reg = derp_registry.clone();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(30)).await;
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

        // tunnel subsystem
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
            core,
            modules,

            // FR-69 P5a — ALIASES of the fleet module's handles (the same
            // `Arc`s; the module owns them) for the host code that still
            // serves the agent socket. Each alias leaves with the host file
            // that reads it (P7).
            agents: fleet.agents.clone(),
            enrollment_keys: fleet.enrollment_keys.clone(),
            exec_audit: fleet.exec_audit.clone(),
            ssh_audit,
            config_audit: fleet.config_audit.clone(),
            key_rotation_audit,
            peer_relay_audit,
            ssh_activity,
            agent_crashes: fleet.agent_crashes.clone(),
            agent_logs: fleet.agent_logs.clone(),
            consent_requests: fleet.consent_requests.clone(),
            rc_hub: fleet.rc_hub.clone(),
            derp_ticket,
            agent_presence_tokens: fleet.agent_presence_tokens.clone(),
            presence_fanout: fleet.presence_fanout.clone(),
            tunnel_presence_tokens: tunnel_presence_tokens.clone(),
            tunnel_clients,
            tunnel_policies,
            tunnel_audit,
            tunnel_clients_by_session: tunnel_clients_by_session.clone(),
            tunnel_sessions_by_target_agent: tunnel_sessions_by_target_agent.clone(),
            tunnel_sessions_by_origin_agent: tunnel_sessions_by_origin_agent.clone(),
            agent_nudge_cooldowns: fleet.agent_nudge_cooldowns.clone(),
            agent_nudge_throttle: fleet.agent_nudge_throttle.clone(),
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
            exec_rate_limiter: fleet.exec_rate_limiter.clone(),
            ssh_rate_limiter: Arc::new(crate::rate_limit::RateLimiter::new()),
            relay_rate_limiter: Arc::new(crate::rate_limit::RateLimiter::new()),
            key_rotation_rate_limiter: Arc::new(crate::rate_limit::RateLimiter::new()),
            org_relay: Arc::new(crate::ws::org_relay::OrgRelayState::new()),
            releases_cache: fleet.releases_cache.clone(),
        };

        // FR-69 P4 — the media claim-or-route bus handlers + claim heartbeat
        // and the media sampler are wired by the `conference` module's init.
        // C-5 — derp rehome: the owner-side close handler.
        crate::ws::derp_cluster::wire_derp_cluster(&state);
        // Split-brain observability: the per-pod DERP registry census.
        crate::ws::derp::spawn_registry_census(&state);
        // PR-2 — the cross-pod rc signalling relay is wired by the `remote`
        // module's init (FR-69 P6).
        // FR-51 — the ephemeral-node reaper (cluster-singleton per cycle via
        // the same DB-name-scoped claim pattern). Spawns NOTHING unless
        // `rc.ephemeral_reaper_enabled` — the P1 kill switch, default off.
        crate::ws::ephemeral::spawn_reaper(state.clone());
        // #1186 — apply cross-pod `overlay_removes` envelopes (the channel is
        // fed by the redis ctrl subscriber above; with no Redis the sender
        // side never fires and this task just parks).
        crate::ws::overlay::spawn_removes_applier(state.clone(), overlay_ctrl_rx);
        // Stats PR-1 — the rollup compactor (raw → _1h → _1d), cluster-
        // singleton per cycle via the same DB-name-scoped claim pattern.
        // No-op task when `stats.enabled=false`.
        crate::stats_rollup::spawn_stats_rollup(state.clone());
        // FR-20 P1 — drain the per-network DERP byte counters into the
        // `stats_usage` cost ledger every 60 s. Runs on BOTH pods on purpose:
        // each writes only the bytes it relayed, into the same deterministic
        // `_id`, and `$inc` sums them.
        crate::ws::derp::spawn_derp_usage_flush(state.clone());

        // FR-69 P5a — the inverse edges (D6). Each mounted module registers
        // its hooks; the host registers its TRANSITIONAL implementation of
        // the network steps of the agent cascade (overlay release, MagicDNS
        // rename) under the `network` id, so the fleet module's cascades
        // already run in HOOK_ORDER through the core registry.
        state.modules.register_hooks(&state.core);
        state.core.hooks.register(
            "network",
            roomler_core::Hooks {
                fleet: Some(Arc::new(crate::hooks::HostNetworkHooks {
                    state: state.clone(),
                })),
                tenant: None,
            },
        );
        // FR-69 P5c — the agent socket is the fleet module's; the host
        // registers its TRANSITIONAL `network` half (the tunnel/overlay
        // relays with their per-connection state, SSH, key rotation, DERP
        // tickets, probe reports) so the module dispatches by owner without
        // naming the host. The `remote` half registers itself (P6).
        let net = Arc::new(crate::ws::agent_socket_host::HostNetworkAgentSocket::new(
            state.clone(),
        ));
        state.core.agent_socket.register(
            "network",
            roomler_core::AgentSocketHooks {
                handler: Some(net.clone()),
                lifecycle: Some(net),
            },
        );

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

    // FR-69 P4/P5a — the modules release what they own, in reverse
    // composition order: fleet the agent sweep (cancel every local agent
    // socket, bulk-mark them Offline, compare-DEL their presence claims),
    // conference its media claims. After the host's own directory releases
    // above, so the sweep's settling beat never delays them.
    state.modules.shutdown().await;
}

#[cfg(test)]
mod notify_semantics_tests {

    /// A `Notify` signal must survive a task that is NOT parked on
    /// `notified()` at the instant it is fired.
    ///
    /// These two tests outlive FR-28's reverted P0 on purpose. The hazard
    /// is general: anywhere a `select!`-driven loop is signalled, the
    /// future is rebuilt each iteration, so there is no registered waiter
    /// while the loop is doing work. `notify_waiters` drops the signal
    /// there; `notify_one` stores a permit. FR-28 P0 shipped the wrong one
    /// and the field measured it (12 `hard-errored` lines vs a baseline of
    /// 8) — the code is gone, the lesson should not be.
    ///
    /// ⚠️ This is the test the first version needed and did not have. It
    /// only checked the env kill switch, which cannot observe the defect:
    /// `notify_waiters()` wakes waiters registered AT THAT MOMENT and
    /// stores nothing, while the real socket loop rebuilds its
    /// `cancel.notified()` future every `select!` iteration. So exactly
    /// the busy sockets — the ones carrying a live session — missed it,
    /// and the field test measured MORE `hard-errored` lines (12) than
    /// the undrained baseline (8).
    #[tokio::test]
    async fn cancel_reaches_a_socket_that_is_not_currently_awaiting() {
        let cancel = std::sync::Arc::new(tokio::sync::Notify::new());

        // Fire while nobody is parked — the busy-forwarding case.
        cancel.notify_one();

        // The loop comes back around and awaits: it must return at once.
        let woke =
            tokio::time::timeout(std::time::Duration::from_millis(200), cancel.notified()).await;
        assert!(
            woke.is_ok(),
            "a cancel fired while the socket was busy must still be observed"
        );
    }

    /// The same shape with `notify_waiters`, pinned so the difference is
    /// documented rather than rediscovered: it is LOST when nobody waits.
    #[tokio::test]
    async fn notify_waiters_loses_a_signal_when_nobody_is_parked() {
        let cancel = std::sync::Arc::new(tokio::sync::Notify::new());
        cancel.notify_waiters();
        let woke =
            tokio::time::timeout(std::time::Duration::from_millis(100), cancel.notified()).await;
        assert!(
            woke.is_err(),
            "notify_waiters stores no permit — this is the bug"
        );
    }
}
