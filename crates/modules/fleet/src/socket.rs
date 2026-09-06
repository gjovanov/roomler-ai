// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! The agent socket (`/ws?role=agent`): the hello, the Hub registration,
//! presence, the read loop, the teardown order — fleet's since FR-69 P5c.
//!
//! Moved from the host's `ws/remote_control.rs` with one structural change:
//! the pipeline of relays and explicit arms that served `remote`- and
//! `network`-owned messages is ONE dispatch by `ClientMsg::namespace()`
//! through `Core::agent_socket` — the handlers those modules registered (the
//! host's transitional ones until P6/P7) — and the per-connection state
//! those arms kept in this loop's locals (the tunnel originator, its
//! sessions, the probe-persist throttle) lives behind their lifecycles,
//! keyed by the connection id minted here. The teardown order is unchanged
//! and written once (see `roomler_core::agent_socket`).
//!
//! The host keeps the `/ws` upgrade and the role gate (D7) and calls
//! [`handle_agent_socket`] from its agent branch: the URL, the wire and the
//! LB pinning are untouched.

use axum::extract::ws::{Message, WebSocket};
use bson::oid::ObjectId;
use futures::{SinkExt, StreamExt, stream::SplitSink};
use roomler_ai_remote_control::{
    models::{ConsentMode, RpcCap},
    signaling::{AgentSysStats, ClientMsg, Owner, RelayRegionRtt, Role, ServerMsg},
    turn_creds::relay_regions_wire,
};
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};
use tracing::{debug, info, warn};

use crate::FleetState;
use crate::hub::DispatchCtx;
use axum::{extract::WebSocketUpgrade, response::Response};
use roomler_core::ws::upgrade::{MAX_WS_MESSAGE_BYTES, send_goodbye_and_close, tid_matches_claim};

/// Handle a socket that authenticated as an agent.
///
/// Lifecycle: verify + look up agent, expect `rc:agent.hello`, register with
/// the Hub, then relay `rc:*` traffic in both directions until the socket closes.
pub async fn handle_agent_socket(
    state: FleetState,
    socket: WebSocket,
    agent_id: ObjectId,
    tenant_id: ObjectId,
    owner_user_id: ObjectId,
    // PR-1 rehome — the affinity key this agent dialed with (None =
    // pre-S6 agent build); direction input when THIS agent originates
    // tunnel opens toward a foreign-homed target.
    dialed_tid: Option<String>,
) {
    info!(%agent_id, %tenant_id, "remote-control agent WS connected");
    let conn_established_ms = bson::DateTime::now().timestamp_millis();

    let (socket_tx, mut socket_rx) = socket.split();
    let socket_tx = Arc::new(Mutex::new(socket_tx));

    // Wait for the agent's hello message — it announces OS + capabilities.
    let hello = match read_next_rc(&mut socket_rx).await {
        Some(ClientMsg::AgentHello {
            machine_name,
            os,
            agent_version,
            displays,
            caps,
            advertised_routes,
            supports_relay_regions,
            ssh_host_pubkey,
        }) => (
            machine_name,
            os,
            agent_version,
            displays,
            caps,
            advertised_routes,
            supports_relay_regions,
            ssh_host_pubkey,
        ),
        other => {
            warn!(?other, "agent opened WS without rc:agent.hello — closing");
            return;
        }
    };
    let (
        machine_name,
        os,
        agent_version,
        displays,
        caps,
        advertised_routes,
        supports_relay_regions,
        ssh_host_pubkey,
    ) = hello;

    // Persist: mark online, update hello fields on the Mongo row. Best-effort —
    // signaling still works if Mongo lags.
    if let Err(e) = state
        .agents
        .update_hello(
            agent_id,
            &agent_version,
            &displays,
            &caps,
            &advertised_routes,
            &ssh_host_pubkey,
        )
        .await
    {
        warn!(%agent_id, %e, "agent update_hello failed");
    }

    // Register with the Hub and start pumping server → socket.
    //
    // rc.53: register_agent now returns `(tx, cancel, rx)`:
    //   * `tx` is captured for the eventual `unregister_agent` call so
    //     a displaced-handler late unregister doesn't evict a newer
    //     connection's entry (critique #4 race fix).
    //   * `cancel` is an `Arc<Notify>` the read-loop `select!`s on,
    //     so a displacement triggers an immediate read-loop exit
    //     instead of waiting up to one 25 s keepalive interval.
    //   * `rx` feeds the pump task as before.
    let max_sessions = caps.max_simultaneous_sessions.max(1);
    // P6 — an arbiter-capable agent serializes + fences concurrent input
    // itself; the hub then skips the P3 single-INPUT-holder downgrade.
    let input_arbiter = caps.input.iter().any(|c| c == "arbiter");
    // Fleet RPC — distilled here so the Hub can refuse a pre-feature agent
    // with 412 instead of pushing a frame it drops in its unknown-tag branch,
    // which would leave the caller waiting out its whole deadline.
    let supports_exec = caps.has_rpc(RpcCap::Exec);
    // Roomler SSH — same distillation, same reason: a grant pushed to a build
    // without the `ssh-server` feature is recorded by nobody, and the caller
    // then dials a port that authenticates them against an empty table.
    let supports_ssh = caps.has_rpc(RpcCap::Ssh);
    let (registered_tx, cancel, rx) = state.rc_hub.register_agent(
        agent_id,
        tenant_id,
        owner_user_id,
        os,
        max_sessions,
        input_arbiter,
        supports_exec,
    );
    state.rc_hub.set_agent_ssh_support(agent_id, supports_ssh);
    let pump_socket_tx = socket_tx.clone();
    let pump = tokio::spawn(pump_server_messages(rx, pump_socket_tx));

    debug!(%agent_id, %machine_name, "agent registered in Hub");

    // Multi-region relay PoPs: seed the Hub's live relay_home from the row
    // (probing refreshes it within seconds when enabled), and push the probe
    // targets — ONLY to an agent that advertised the capability (its ServerMsg
    // deserializer would error on the unknown variant otherwise) and only
    // when regions are actually configured+enabled.
    let mut device_name = machine_name.clone();
    // Remote config — read off the row we are already fetching, pushed below.
    let mut desired_config = None;
    // FR-40 — a standing rotation order with no answer yet is re-ordered on
    // connect (the offline case, through the same path as the online one).
    // ⚠️ NOT an order delivered seconds ago: the device that just rotated
    // reconnects before its `rotated` report is written, and re-pushing the
    // same order there made it refuse a duplicate and overwrite its own
    // success (P1b, first field run) — see `overlay_key::should_redeliver`.
    // ⚠️ And never an order the device has ALREADY executed: its join under
    // another key is the proof (P1d, third cycle — a lost report plus two
    // reconnects turned one click into three rotations).
    let mut pending_key_rotation: Option<String> = None;
    if let Ok(row) = state.agents.find_in_tenant(tenant_id, agent_id).await {
        if let Some(req) = row.key_rotation.as_ref()
            && !roomler_ai_remote_control::models::order_is_satisfied(
                req,
                row.overlay_identity.as_ref(),
            )
            && roomler_ai_remote_control::models::should_redeliver(
                req,
                row.key_rotation_report.as_ref(),
                bson::DateTime::now(),
            )
        {
            pending_key_rotation = Some(req.request_id.clone());
        }
        // Prefs = the persisted probe table ordered by RTT (nearest first) —
        // the load-aware fallback ladder until a fresh report replaces it.
        let prefs = prefs_from_rtt(row.relay_rtt.as_deref().unwrap_or(&[]));
        state
            .rc_hub
            .set_agent_relay_home(agent_id, row.relay_home.clone(), prefs);
        // The row name wins over the hello machine_name — admins rename devices.
        device_name = row.name;
        if !row.desired_config.is_empty() {
            desired_config = Some(row.desired_config);
        }
    }
    // P4 — announce the online transition (`agents.last_presence` ledger
    // CAS: a reconnect that was already broadcast as online stays silent).
    crate::presence::note_transition(
        &state,
        tenant_id,
        agent_id,
        &device_name,
        crate::presence::ONLINE,
    )
    .await;
    if supports_relay_regions && let Some((regions, rev)) = relay_regions_wire(&state.turn_map) {
        let _ = registered_tx.try_send(ServerMsg::RelayRegions { regions, rev });
    }

    // Remote config (docs/remote-config.md) — RECONCILE-ON-CONNECT is the only
    // delivery path, deliberately: a device that was offline for a week and one
    // that was online when an admin hit save converge through the same code,
    // so the offline case is exercised on every connect rather than only when
    // nobody is watching.
    if let Some(desired) = desired_config {
        if caps.has_rpc(RpcCap::Config) {
            let _ = registered_tx.try_send(ServerMsg::ConfigPush {
                revision: desired.revision,
                desired,
            });
        } else {
            // Unlike `Goodbye`/`UpdateNow`, this one must not be sent blind.
            // A pre-feature agent drops the unknown tag in its `debug!` branch
            // and the admin is left looking at a pending change that quietly
            // evaporated — so say so here instead.
            info!(
                %agent_id, %device_name, revision = desired.revision,
                "desired config not pushed — this device's agent predates \
                 rc:agent.config; update it or apply the config locally"
            );
        }
    }

    // FR-40 — reconcile-on-connect for a rotation order the device has not
    // answered. Cap-gated for the same reason as the config push: a
    // pre-feature agent would drop the frame silently while the dashboard
    // showed a rotation in flight — and this one is a security action.
    if let Some(request_id) = pending_key_rotation {
        if caps.has_rpc(RpcCap::KeyRotate) {
            if registered_tx
                .try_send(ServerMsg::KeyRotate {
                    request_id: request_id.clone(),
                })
                .is_ok()
            {
                info!(%agent_id, %device_name, %request_id, "overlay-key rotation order delivered on connect");
                if let Err(e) = state
                    .agents
                    .mark_key_rotation_delivered(tenant_id, agent_id, &request_id)
                    .await
                {
                    warn!(%agent_id, %e, "key_rotation delivered_at write failed");
                }
            }
        } else {
            info!(
                %agent_id, %device_name, %request_id,
                "overlay-key rotation not ordered — this device's agent predates \
                 rc:agent.key_rotate; update it first"
            );
        }
    }

    // Phase A-1 — mirror the registration into the cross-pod presence
    // directory. The owner token is per-REGISTRATION (fresh conn_id), so a
    // same-pod reconnect's late teardown can't release the new socket's
    // claim. Best-effort: Redis down ⇒ pod-local behavior only.
    let agent_hex = agent_id.to_hex();
    let presence_token = state.redis_pubsub.as_ref().map(|r| {
        r.agent_owner_token(
            &uuid::Uuid::new_v4().to_string(),
            bson::DateTime::now().timestamp_millis(),
        )
    });
    if let (Some(redis), Some(token)) = (&state.redis_pubsub, &presence_token) {
        if let Err(e) = redis.agent_presence_set(&agent_hex, token).await {
            warn!(%agent_id, %e, "agent presence SET failed (register)");
        }
        // Registered-token registry for the SIGTERM sweep (newest wins —
        // a displacing registration overwrites the displaced one's entry).
        state.agent_presence_tokens.insert(agent_id, token.clone());
    }

    // FR-69 P5c — what the other modules' arms need about this connection,
    // built once. `conn_id` is per CONNECTION (a displacing connection for
    // the same agent gets its own), which is the key a module uses for the
    // state it creates on `hello` and releases on `closing`: the tunnel
    // originator with its sessions and transports, the probe-persist
    // throttle. The lifecycles run in HOOK_ORDER.
    let actx = roomler_core::AgentCtx {
        conn_id: uuid::Uuid::new_v4().to_string(),
        agent_id,
        tenant_id,
        owner_user_id,
        agent_version: agent_version.clone(),
        os,
        dialed_tid,
        conn_established_ms,
        tx: registered_tx.clone(),
    };
    let lifecycles = state.agent_socket.lifecycles();
    for (_, lc) in &lifecycles {
        lc.hello(&actx).await;
    }

    // Build a ctx once — it's Copy-able across messages for this connection.
    let ctx = DispatchCtx {
        role: Role::Agent,
        user_id: None,
        agent_id: Some(agent_id),
        controller_name: None,
        controller_tx: None,
        // Unused for agent-role dispatch (only a controller's SessionRequest
        // consumes these); harmless defaults.
        consent_mode: ConsentMode::Prompt,
        override_reason: None,
        input_mode: None,
        tenant_name: None,
    };

    // Phase A-1 — server-side receive-liveness, symmetric to the agent's
    // rc.293 deadline: reconnect/reap must key on frames RECEIVED, because
    // a TLS-inspecting middlebox keeps the TCP leg alive (ACKing) long
    // after the peer is gone, and the server never pings (deliberately:
    // a ping arm would need the pump's shared sink mutex, which can be
    // held while blocked on that same half-open peer — wedging the read
    // loop in exactly the failure mode being detected). The agent's own
    // 25 s pings + 30 s heartbeats mean a healthy inbound leg is never
    // silent for 90 s. Breaking the loop runs the normal teardown below —
    // the read loop IS the reaper; no registry, no sweeper task.
    let rx_deadline = std::time::Duration::from_secs(state.settings.rc.ws_rx_deadline_secs.max(2));
    let mut liveness = tokio::time::interval(std::time::Duration::from_secs(
        state.settings.rc.ws_liveness_tick_secs.max(1),
    ));
    liveness.tick().await; // arm; first tick fires immediately
    let mut last_rx = std::time::Instant::now();

    // Read loop. rc.53: wrapped in `tokio::select!` so the Hub's
    // displacement-cancel notify exits this loop within milliseconds
    // — without the cancel arm, a displaced socket would linger up
    // to one 25 s keepalive interval (auto-fail #3 in v2 plan).
    loop {
        tokio::select! {
            // `biased` so cancel fires deterministically when both
            // arms are ready in the same poll cycle. Without this,
            // tokio's random arm selection could starve the cancel
            // for several iterations in a hot read loop.
            biased;
            _ = cancel.notified() => {
                info!(%agent_id, "agent connection cancelled by Hub (replaced by newer); exiting read-loop");
                break;
            }
            _ = liveness.tick() => {
                if last_rx.elapsed() > rx_deadline {
                    warn!(%agent_id, elapsed_s = last_rx.elapsed().as_secs(),
                        "agent WS receive-liveness deadline exceeded — reaping half-open socket");
                    break;
                }
                continue;
            }
            maybe_msg = socket_rx.next() => {
                let Some(msg) = maybe_msg else { break };
                // EVERY inbound frame proves the leg alive — including
                // Pings/Pongs/Binary (the catch-all below).
                last_rx = std::time::Instant::now();
                match msg {
                    Ok(Message::Text(text)) => match serde_json::from_str::<ClientMsg>(&text) {
                        Ok(parsed) => {
                            // FR-69 P5c — every message has ONE owner
                            // (`ClientMsg::namespace()`). Fleet's own arms run
                            // below; `remote`'s and `network`'s run through the
                            // handlers their modules registered on the core —
                            // the tunnel relays, the overlay relay, the probe
                            // report, the DERP ticket, SSH, key rotation, the
                            // session stats. A handler hands back what it did
                            // not consume, and that reaches the Hub's own
                            // dispatch below exactly as before.
                            let parsed = match parsed.namespace() {
                                Owner::Fleet => parsed,
                                owner => match state.agent_socket.handler(owner.id()) {
                                    Some(handler) => match handler.handle(&actx, parsed).await {
                                        Some(rest) => rest,
                                        None => continue,
                                    },
                                    None => {
                                        debug!(
                                            %agent_id, owner = owner.id(),
                                            "no agent-socket handler registered for this owner — the message reaches the Hub dispatch"
                                        );
                                        parsed
                                    }
                                },
                            };
                            // Fleet RPC. Rebinding through a `match` rather
                            // than two `if let`s keeps `parsed` un-moved on
                            // the fall-through path that still reaches the
                            // Hub dispatch below.
                            let parsed = match parsed {
                                // A command's answer — resolve whoever is
                                // parked on it. An unknown id is normal: that
                                // caller already hit its deadline and gave up.
                                ClientMsg::RpcResult {
                                    request_id,
                                    exit_code,
                                    stdout,
                                    stderr,
                                    truncated,
                                    duration_ms,
                                    error,
                                } => {
                                    let delivered = state.rc_hub.deliver_exec_result(
                                        &request_id,
                                        roomler_ai_remote_control::models::ExecOutcome {
                                            exit_code,
                                            stdout,
                                            stderr,
                                            truncated,
                                            duration_ms,
                                            error,
                                        },
                                    );
                                    if !delivered {
                                        debug!(
                                            %agent_id, %request_id,
                                            "rc:rpc.result for an unknown request — caller gave up"
                                        );
                                    }
                                    continue;
                                }
                                // A device asking to run something on ANOTHER
                                // device (`roomler exec`). Spawned: the
                                // target's command can take minutes and this
                                // is the REQUESTING agent's read loop.
                                ClientMsg::RpcExecRequest {
                                    request_id,
                                    target,
                                    shell,
                                    command,
                                    timeout_ms,
                                } => {
                                    let state = state.clone();
                                    let reply_tx = registered_tx.clone();
                                    tokio::spawn(async move {
                                        handle_agent_exec_request(
                                            &state, tenant_id, agent_id, request_id, target, shell,
                                            command, timeout_ms, reply_tx,
                                        )
                                        .await;
                                    });
                                    continue;
                                }
                                // Remote config — the device's own account of
                                // what it did with a pushed desired-config
                                // (`docs/remote-config.md`). Recorded on the
                                // agent row rather than in `config_audit`:
                                // that collection holds the SERVER's decisions
                                // and is authoritative, while this is a claim
                                // by a host that may be compromised. Folding
                                // them together would leave a reader unable to
                                // tell which is which.
                                //
                                // ⚠️ `tenant_id` / `agent_id` come from the
                                // authenticated WS, never the frame, so a
                                // device can only report about itself.
                                ClientMsg::ConfigStatus {
                                    revision,
                                    outcome,
                                    live,
                                    needs_restart,
                                    detail,
                                } => {
                                    let state = state.clone();
                                    tokio::spawn(async move {
                                        record_config_report(
                                            &state,
                                            tenant_id,
                                            agent_id,
                                            revision,
                                            outcome,
                                            live,
                                            needs_restart,
                                            detail,
                                        )
                                        .await;
                                    });
                                    continue;
                                }
                                other => other,
                            };
                            // Phase 7: refresh last_seen_at on every heartbeat. Hub
                            // dispatch is a no-op for AgentHeartbeat (handled here);
                            // we still call dispatch so any future routing logic
                            // (e.g. metrics fan-out) only needs one entry point.
                            // Stats PR-1: destructure the fields BEFORE `parsed`
                            // moves into dispatch — the sample ingest below needs
                            // them. The legacy rss/cpu scalars are hardcoded 0 on
                            // every shipped agent, so they are deliberately NOT
                            // persisted (a real zero must stay distinguishable
                            // from "not measured"; the v2 `sys` block covers it).
                            let heartbeat_sessions = if let ClientMsg::AgentHeartbeat {
                                active_sessions,
                                sys,
                                srflx_count,
                                warm_relay,
                                companion_version,
                                caps,
                                ..
                            } = &parsed
                            {
                                Some((
                                    *active_sessions,
                                    sys.clone(),
                                    *srflx_count,
                                    warm_relay.clone(),
                                    companion_version.clone(),
                                    caps.clone(),
                                ))
                            } else {
                                None
                            };
                            if let Err(e) = state.rc_hub.dispatch(&ctx, parsed) {
                                warn!(%agent_id, %e, "rc:* dispatch failed (agent)");
                            }
                            if let Some((
                                active_sessions,
                                sys,
                                srflx_count,
                                warm_relay,
                                companion_version,
                                caps,
                            )) = heartbeat_sessions
                            {
                                // FR-43 P2c — an agent announces changed
                                // capabilities here because caps otherwise
                                // travel only in `rc:agent.hello`, and a macOS
                                // daemon's GUI worker attaches after that.
                                //
                                // ⚠️ `None` means "no news", NOT "no
                                // capabilities": the stored blob must be left
                                // alone, the same rule as `permissions`' own
                                // `None` vs `Some([])`.
                                if let Some(caps) = caps.as_ref()
                                    && let Err(e) =
                                        state.agents.update_capabilities(agent_id, caps).await
                                {
                                    warn!(%agent_id, %e, "agent update_capabilities failed");
                                }
                                if let Err(e) = state
                                    .agents
                                    .touch_heartbeat(
                                        agent_id,
                                        warm_relay.as_deref(),
                                        companion_version.as_deref(),
                                    )
                                    .await
                                {
                                    warn!(%agent_id, %e, "agent touch_heartbeat failed");
                                }
                                // C4 stage 2 (PR-B) — the warm leg mirrored onto
                                // the overlay-node row is `network`'s write
                                // (FR-69 P5c): its lifecycle does it.
                                for (_, lc) in &lifecycles {
                                    lc.heartbeat(&actx, warm_relay.as_deref()).await;
                                }
                                if state.settings.stats.enabled {
                                    let unix = std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .unwrap_or_default()
                                        .as_secs()
                                        as i64;
                                    // Wave 2 — the per-peer edges ride the
                                    // same block; pulled out before `sys`
                                    // is consumed below.
                                    let mesh_links: Vec<bson::Document> = sys
                                        .as_ref()
                                        .map(|s| s.links.iter().map(mesh_link_doc).collect())
                                        .unwrap_or_default();
                                    let sys_doc = sys.map(|s| machine_sys_doc(&s, srflx_count));
                                    if let Err(e) = state
                                        .stats
                                        .upsert_machine_sample(
                                            tenant_id,
                                            agent_id,
                                            unix,
                                            active_sessions,
                                            sys_doc,
                                        )
                                        .await
                                    {
                                        debug!(%agent_id, %e, "machine sample persist failed");
                                    }
                                    // Wave 2 — this agent's view of the
                                    // mesh, replaced whole (one row per
                                    // agent). Kept OUT of the minute
                                    // buckets: it's current-state, not a
                                    // time series, and the graph reads
                                    // the newest snapshot per agent.
                                    if !mesh_links.is_empty()
                                        && let Err(e) = state
                                            .stats
                                            .upsert_mesh_snapshot(tenant_id, agent_id, &mesh_links)
                                            .await
                                    {
                                        debug!(%agent_id, %e, "mesh snapshot persist failed");
                                    }
                                }
                                // Phase A-1 — refresh the presence claim, gated
                                // on STILL holding the hub slot (an admin-kicked
                                // agent whose socket lingers must not re-assert
                                // a record the hub no longer serves).
                                if state.rc_hub.is_agent_online(agent_id)
                                    && let (Some(redis), Some(token)) =
                                        (&state.redis_pubsub, &presence_token)
                                    && let Err(e) =
                                        redis.agent_presence_set(&agent_hex, token).await
                                {
                                    debug!(%agent_id, %e, "agent presence refresh failed");
                                }
                            }
                        }
                        Err(e) => {
                            debug!(%agent_id, %e, "ignoring non-rc:* message on agent socket");
                        }
                    },
                    Ok(Message::Ping(data)) => {
                        let mut guard = socket_tx.lock().await;
                        let _ = guard.send(Message::Pong(data)).await;
                    }
                    Ok(Message::Close(_)) | Err(_) => break,
                    _ => {}
                }
            }
        }
    }

    // Teardown: unregister + mark offline. rc.53: thread `registered_tx`
    // into unregister_agent so a displaced-handler late unregister
    // doesn't evict the newer connection's registry entry
    // (critique #4 race fix). Pump task exits when the Hub drops its
    // sender (during unregister_agent), so we don't need to abort it
    // explicitly — but the `pump.abort()` is kept as a belt-and-
    // suspenders for the case where the tx-identity check skipped
    // the removal and the pump is still wired to the live channel.
    // FR-69 P5c — the holders release first, BEFORE the Hub unregistration
    // and regardless of who owns the slot: `network` tears down the tunnel
    // sessions this connection originated (P3b-2 — they live on its peers)
    // and terminates every session targeting the agent (P7 flap resilience:
    // a reconnected instance rejects every forward on them forever).
    for (_, lc) in &lifecycles {
        lc.closing(&actx).await;
    }

    // Phase A-1 — the Mongo Offline write + presence release are GATED on
    // the hub removal being OURS: a displaced handler's late teardown must
    // not clobber the displacing connection's Online status or its fresh
    // presence claim (the status-race twin of the rc.53 registry fix).
    let removal_was_ours = state
        .rc_hub
        .unregister_agent(agent_id, Some(&registered_tx));
    pump.abort();
    // FR-69 P5c — the holders learn whether this removal was ours: `network`
    // runs the overlay leave only then (rc.307 B — a displaced connection's
    // late teardown must not mark the overlay row Offline AFTER the replacing
    // connection's re-join set it Online; the fleet gate right below is the
    // same pattern for the Mongo status and the presence claim).
    for (_, lc) in &lifecycles {
        lc.closed(&actx, removal_was_ours).await;
    }
    if removal_was_ours {
        if let Err(e) = state
            .agents
            .mark_status(
                agent_id,
                roomler_ai_remote_control::models::AgentStatus::Offline,
            )
            .await
        {
            warn!(%agent_id, %e, "agent mark_status(offline) failed");
        }
        // P4 — offline event, SUPPRESSED when a newer registration owns the
        // directory record (a re-homed agent's late teardown on the old pod:
        // the device is alive on another pod, and its ledger stays "online").
        let mut foreign_claim = false;
        if let (Some(redis), Some(token)) = (&state.redis_pubsub, &presence_token) {
            match redis.agent_presence_del_if_owned(&agent_hex, token).await {
                Ok(true) => {}
                Ok(false) => {
                    foreign_claim = redis
                        .agent_presence_exists(&agent_hex)
                        .await
                        .unwrap_or(false);
                }
                Err(e) => {
                    debug!(%agent_id, %e, "agent presence release failed");
                }
            }
            // Drop OUR token from the sweep registry (identity-gated: a
            // displacing registration's newer token must survive).
            state
                .agent_presence_tokens
                .remove_if(&agent_id, |_, stored| stored == token);
        }
        if !foreign_claim {
            crate::presence::note_transition(
                &state,
                tenant_id,
                agent_id,
                &machine_name,
                crate::presence::OFFLINE,
            )
            .await;
        }
    } else {
        debug!(%agent_id, "teardown skipped Offline+presence (newer connection owns the slot)");
    }
    info!(%agent_id, "remote-control agent WS disconnected");
}

/// Fleet RPC — a device asked to run a command on another device
/// (`roomler exec`, whose CLI has no user credentials of its own and so goes
/// through the daemon's already-authenticated agent WS).
///
/// Two things make this leg safe beyond the four normal gates:
///
/// * The acting principal is the ORIGINATING device's `owner_user_id`, never
///   anything taken off the wire — so `EXEC_DEVICE` is still evaluated against
///   a real person.
/// * `ExecPolicy::can_originate` must be set on the origin device (checked in
///   [`crate::agent_exec::authorize`]). Without it, one compromised laptop
///   would inherit its owner's exec rights across the whole fleet.
///
/// Cross-tenant is impossible by construction: the target is resolved WITHIN
/// `tenant_id`, which is this socket's own enrollment.
#[allow(clippy::too_many_arguments)]
async fn handle_agent_exec_request(
    state: &FleetState,
    tenant_id: bson::oid::ObjectId,
    origin_agent_id: bson::oid::ObjectId,
    request_id: String,
    target: String,
    shell: String,
    command: String,
    timeout_ms: u64,
    reply_tx: roomler_ai_remote_control::session::ClientTx,
) {
    use roomler_ai_remote_control::models::ExecOutcome;

    let reply = |outcome: ExecOutcome| ServerMsg::RpcExecResponse {
        request_id: request_id.clone(),
        exit_code: outcome.exit_code,
        stdout: outcome.stdout,
        stderr: outcome.stderr,
        truncated: outcome.truncated,
        duration_ms: outcome.duration_ms,
        error: outcome.error,
    };
    let fail = |msg: String| {
        reply(ExecOutcome {
            error: Some(msg),
            ..Default::default()
        })
    };

    // Resolve the origin's owner — the person whose permissions this runs
    // under. A device whose row vanished mid-flight has no principal, so it
    // gets nothing.
    let origin = match state
        .agents
        .find_in_tenant(tenant_id, origin_agent_id)
        .await
    {
        Ok(a) => a,
        Err(e) => {
            let _ = reply_tx.try_send(fail(format!("origin device unknown: {e}")));
            return;
        }
    };

    // Target by hex id, else by device name — an operator types `winhost-a`,
    // not an ObjectId.
    let agent = match resolve_exec_target(state, tenant_id, &target).await {
        Some(a) => a,
        None => {
            let _ = reply_tx.try_send(fail(format!("no device named {target:?} in this org")));
            return;
        }
    };

    // "<person> (via <device>)" — the consent prompt on the target names both
    // the human accountable for the command and the box it came from.
    let who = state
        .users
        .base
        .find_by_id(origin.owner_user_id)
        .await
        .map(|u| u.display_name)
        .unwrap_or_else(|_| origin.owner_user_id.to_hex());
    let caller = crate::agent_exec::Caller {
        user_id: origin.owner_user_id,
        display: format!("{who} (via {})", origin.name),
        origin_agent_id: Some(origin_agent_id),
        source: "cli",
    };
    let body = crate::agent_exec::ExecRequestBody {
        shell,
        command,
        timeout_ms,
        max_output_bytes: 0,
        cwd: None,
    };
    let res = crate::agent_exec::dispatch(state, tenant_id, &agent, &caller, &body).await;
    if reply_tx.try_send(reply(res.outcome)).is_err() {
        warn!(%origin_agent_id, %request_id, "rc:rpc.response undeliverable — origin WS gone");
    }
}

/// Persist a device's [`ClientMsg::ConfigStatus`] onto its agent row.
///
/// Last-report-wins rather than a collection: unlike `ssh_activity` there is
/// no history worth keeping here — an operator asks "did this land?", which is
/// a question about the CURRENT desired revision. The trail of who asked for
/// what already exists in `config_audit`, on the authoritative side.
///
/// ⚠️ `reported_at` is stamped HERE, not taken off the wire. A device's clock
/// is not a fact the control plane should inherit — a skewed host would
/// otherwise be able to make its report look newer (or older) than it is.
#[allow(clippy::too_many_arguments)]
async fn record_config_report(
    state: &FleetState,
    tenant_id: ObjectId,
    agent_id: ObjectId,
    revision: u64,
    outcome: roomler_ai_remote_control::models::ConfigOutcome,
    live: Vec<String>,
    needs_restart: Vec<String>,
    detail: Option<String>,
) {
    use roomler_ai_remote_control::models::ConfigReport;

    // Re-clamped on receipt: a bound that exists only on the reporting side is
    // not a bound, since the reporting side is the untrusted one.
    let detail = detail.map(|mut d| {
        if d.chars().count() > ConfigReport::MAX_DETAIL {
            d = d.chars().take(ConfigReport::MAX_DETAIL).collect();
            d.push('…');
        }
        d
    });
    let report = ConfigReport {
        revision,
        outcome,
        live,
        needs_restart,
        detail,
        reported_at: bson::DateTime::now(),
    };
    if let Err(e) = state
        .agents
        .record_config_report(tenant_id, agent_id, &report)
        .await
    {
        warn!(%agent_id, %e, "config report write failed");
    }
}

/// Resolve an exec target within one org: hex agent id first, then an
/// exact-then-case-insensitive device name.
pub async fn resolve_exec_target(
    state: &FleetState,
    tenant_id: bson::oid::ObjectId,
    target: &str,
) -> Option<roomler_ai_remote_control::models::Agent> {
    if let Ok(oid) = bson::oid::ObjectId::parse_str(target)
        && let Ok(a) = state.agents.find_in_tenant(tenant_id, oid).await
    {
        return Some(a);
    }
    let page = state
        .agents
        .list_for_tenant(
            tenant_id,
            &roomler_ai_services::dao::base::PaginationParams {
                page: 1,
                per_page: 500,
                before: None,
            },
        )
        .await
        .ok()?;
    page.items
        .iter()
        .find(|a| a.name == target)
        .or_else(|| {
            page.items
                .iter()
                .find(|a| a.name.eq_ignore_ascii_case(target))
        })
        .cloned()
}

/// The agent's measured regions ordered by RTT (nearest first) — the
/// load-aware fallback ladder handed to the Hub alongside `relay_home`.
pub fn prefs_from_rtt(results: &[RelayRegionRtt]) -> Vec<String> {
    let mut measured: Vec<(u32, &str)> = results
        .iter()
        .filter_map(|r| r.rtt_ms.map(|ms| (ms, r.region.as_str())))
        .collect();
    measured.sort_unstable();
    measured.into_iter().map(|(_, r)| r.to_string()).collect()
}

/// Parse the next inbound WS text frame as [`ClientMsg`]. Skips non-text frames.
async fn read_next_rc(
    socket_rx: &mut futures::stream::SplitStream<WebSocket>,
) -> Option<ClientMsg> {
    while let Some(msg) = socket_rx.next().await {
        match msg {
            Ok(Message::Text(text)) => {
                if let Ok(parsed) = serde_json::from_str::<ClientMsg>(&text) {
                    return Some(parsed);
                }
            }
            Ok(Message::Close(_)) | Err(_) => return None,
            _ => continue,
        }
    }
    None
}

/// Forwards [`ServerMsg`] values from a Hub-owned [`mpsc::Receiver`] to a
/// WebSocket sink. Exits when the channel closes or a send fails.
pub async fn pump_server_messages(
    mut rx: mpsc::Receiver<ServerMsg>,
    socket_tx: Arc<Mutex<SplitSink<WebSocket, Message>>>,
) {
    while let Some(msg) = rx.recv().await {
        let json = match serde_json::to_string(&msg) {
            Ok(s) => s,
            Err(e) => {
                warn!(%e, "serializing ServerMsg failed");
                continue;
            }
        };
        let mut guard = socket_tx.lock().await;
        if guard.send(Message::text(json)).await.is_err() {
            break;
        }
    }
}

/// The persisted `sys` sub-document of a machine sample.
///
/// Hand-built rather than `bson::to_document(&s)` because the shape is NOT
/// the wire shape: the carrier tallies nest under `transports` to match the
/// rollup pipelines' `$sys.transports.*` paths, and `srflx_count` comes from
/// outside the struct.
///
/// The cost of that choice is real and was paid once: a new `AgentSysStats`
/// field is parsed off the wire and then **silently dropped** unless it is
/// also listed here. Wave 3's four volume counters shipped in rc.324, every
/// agent reported them, and every value went in the bin — the traffic chart
/// read "no telemetry" fleet-wide for days, which is indistinguishable from
/// a fleet that simply hasn't updated. `sys_doc_carries_every_counter` below
/// is the guard; extend it when you extend the struct.
fn machine_sys_doc(s: &AgentSysStats, srflx_count: Option<u8>) -> bson::Document {
    bson::doc! {
        "rss_mb": s.rss_mb as i32,
        "cpu_pct": f64::from(s.cpu_pct),
        "net_rx_bytes": s.net_rx_bytes as i64,
        "net_tx_bytes": s.net_tx_bytes as i64,
        "transports": {
            "direct": s.direct as i32,
            "relay": s.relay as i32,
            "derp": s.derp as i32,
        },
        "peer_rtt_ms": s.peer_rtt_ms.map(i64::from),
        // Wave 3 — mesh + tunnel volume.
        "overlay_rx_bytes": s.overlay_rx_bytes as i64,
        "overlay_tx_bytes": s.overlay_tx_bytes as i64,
        "tunnel_rx_bytes": s.tunnel_rx_bytes as i64,
        "tunnel_tx_bytes": s.tunnel_tx_bytes as i64,
        // NAT-traversal coverage. A node reporting 0 cannot hole-punch and
        // reads as UDP-blocked to every peer, so all its pairs degrade to
        // relay/DERP. Counting these across the fleet is the one number that
        // would have surfaced the 2026-08-06 coturn TTL=1 outage on day one
        // instead of after the whole mesh had silently fallen to DERP.
        // `null` = a pre-feature agent, which must stay distinct from a real 0.
        "srflx_count": srflx_count.map(i64::from),
    }
}

/// One mesh edge as persisted into `stats_mesh`. Same hand-built-doc
/// contract as [`machine_sys_doc`] and the same trap: a new `PeerLink`
/// field vanishes silently unless it is listed here.
/// `mesh_link_doc_carries_every_field` below is the guard.
fn mesh_link_doc(l: &roomler_ai_remote_control::signaling::PeerLink) -> bson::Document {
    bson::doc! {
        "node": &l.node,
        "carrier": &l.carrier,
        "rtt_ms": l.rtt_ms.map(i64::from),
        "stalled": l.stalled,
        // Wave 3 per-edge volume.
        "tx": l.tx as i64,
        "rx": l.rx as i64,
        // Wave 4 relay flavour ("turn/udp" / "derp/tcp"); Null on a
        // direct edge or a pre-wave-4 agent.
        "relay": l.relay.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every numeric counter the agent reports must survive into the
    /// persisted document. This test exists because they did not: the
    /// hand-built doc dropped four wave-3 fields for several releases while
    /// the agents dutifully sent them, and nothing anywhere failed — the
    /// dashboards just showed an empty series, which reads exactly like a
    /// fleet that hasn't upgraded yet.
    #[test]
    fn sys_doc_carries_every_counter() {
        let s = AgentSysStats {
            rss_mb: 87,
            cpu_pct: 1.5,
            net_rx_bytes: 1_000,
            net_tx_bytes: 2_000,
            direct: 3,
            relay: 1,
            derp: 1,
            peer_rtt_ms: Some(42),
            overlay_rx_bytes: 4_096,
            overlay_tx_bytes: 8_192,
            tunnel_rx_bytes: 65_536,
            tunnel_tx_bytes: 1_024,
            links: Vec::new(),
        };
        let d = machine_sys_doc(&s, Some(0));

        // The four that were silently dropped.
        assert_eq!(d.get_i64("overlay_rx_bytes").unwrap(), 4_096);
        assert_eq!(d.get_i64("overlay_tx_bytes").unwrap(), 8_192);
        assert_eq!(d.get_i64("tunnel_rx_bytes").unwrap(), 65_536);
        assert_eq!(d.get_i64("tunnel_tx_bytes").unwrap(), 1_024);

        // …and the ones that already worked, so a refactor can't trade one
        // for another.
        assert_eq!(d.get_i64("net_rx_bytes").unwrap(), 1_000);
        assert_eq!(d.get_i32("rss_mb").unwrap(), 87);
        assert_eq!(
            d.get_document("transports")
                .unwrap()
                .get_i32("direct")
                .unwrap(),
            3
        );
        // A measured zero must persist as 0, never as absent — "can't
        // hole-punch" and "didn't report" are different facts.
        assert_eq!(d.get_i64("srflx_count").unwrap(), 0);
    }

    /// Every `PeerLink` field must survive into the persisted edge — same
    /// silent-drop trap as the sys doc, same guard discipline.
    #[test]
    fn mesh_link_doc_carries_every_field() {
        let l = roomler_ai_remote_control::signaling::PeerLink {
            node: "6a1f00000000000000000001".into(),
            carrier: "relay".into(),
            rtt_ms: Some(87),
            stalled: true,
            tx: 512,
            rx: 0,
            relay: Some("turn/udp".into()),
        };
        let d = mesh_link_doc(&l);
        assert_eq!(d.get_str("node").unwrap(), "6a1f00000000000000000001");
        assert_eq!(d.get_str("carrier").unwrap(), "relay");
        assert_eq!(d.get_i64("rtt_ms").unwrap(), 87);
        assert!(d.get_bool("stalled").unwrap());
        assert_eq!(d.get_i64("tx").unwrap(), 512);
        assert_eq!(d.get_i64("rx").unwrap(), 0);
        assert_eq!(d.get_str("relay").unwrap(), "turn/udp");
    }
}

/// The `/ws?role=agent` upgrade (FR-69 P7b, from the host): verify the agent
/// JWT, the affinity key and the row, say why on refusal, then run the loop.
/// The host keeps the `/ws` route and the role gate and hands the token here.
pub fn ws_upgrade_agent(
    state: FleetState,
    token: String,
    tid: Option<String>,
    ws: WebSocketUpgrade,
) -> Response {
    let claims = match state.auth.verify_agent_token(&token) {
        Ok(c) => c,
        Err(_) => {
            return Response::builder()
                .status(401)
                .body("Unauthorized (agent)".into())
                .unwrap();
        }
    };

    // S6 — a present affinity key must match the token's tenant.
    if !tid_matches_claim(&tid, &claims.tenant_id) {
        return Response::builder()
            .status(403)
            .body("tid does not match token tenant".into())
            .unwrap();
    }

    let agent_id = match ObjectId::parse_str(&claims.sub) {
        Ok(id) => id,
        Err(_) => {
            return Response::builder()
                .status(400)
                .body("Invalid agent ID".into())
                .unwrap();
        }
    };
    let tenant_id = match ObjectId::parse_str(&claims.tenant_id) {
        Ok(id) => id,
        Err(_) => {
            return Response::builder()
                .status(400)
                .body("Invalid tenant ID".into())
                .unwrap();
        }
    };

    ws.max_message_size(MAX_WS_MESSAGE_BYTES)
        .max_frame_size(MAX_WS_MESSAGE_BYTES)
        .on_upgrade(move |socket| async move {
            // Verify the agent still exists and isn't quarantined/deleted before
            // we pump any signalling. One Mongo read per connect is cheap and
            // gives us a clean revocation story without needing a token blacklist.
            let agent = match state.agents.find_in_tenant(tenant_id, agent_id).await {
                Ok(a) => a,
                Err(e) => {
                    warn!(%agent_id, %e, "agent lookup failed on WS connect");
                    return;
                }
            };
            // One definition of "this agent may still act", shared with the
            // HTTP ingest routes' `AuthAgent` extractor. This path is the
            // original; the extractor exists because two HTTP routes had
            // silently never grown the equivalent.
            if let Some(reason) = crate::auth_agent::refusal_reason(&agent) {
                // rc.53: push a `ServerMsg::Goodbye { reason: AgentDeleted }`
                // text frame + a Close frame BEFORE dropping the socket so
                // the agent can log a useful "your row was deleted, re-enrol"
                // line instead of an opaque `ws read` (the failure mode
                // WINHOST-B wedged on for hours pre-rc.53). The agent's
                // `handle_server_msg::ServerMsg::Goodbye` arm decides this is
                // fatal + exits with `AGENT_DELETED_EXIT_CODE = 7`, which
                // the SCM supervisor's rc.53 code-7 fast-alarm fires on the
                // FIRST exit.
                info!(%agent_id, reason, "refusing WS with rc:goodbye");
                let goodbye = roomler_ai_remote_control::signaling::ServerMsg::Goodbye {
                    reason: roomler_ai_remote_control::signaling::AgentCloseReason::AgentDeleted,
                    message: "This agent's server-side row was deleted (or quarantined). \
                          Re-enrol with a fresh enrollment token from the admin UI to \
                          revive (soft-deleted rows rehydrate by (tenant_id, machine_id))."
                        .into(),
                };
                send_goodbye_and_close(socket, &goodbye, 4003, "agent_deleted").await;
                return;
            }
            // FR-69 P5c — the socket is the fleet module's; the host keeps
            // the upgrade and the role gate (D7), the module runs the loop.
            handle_agent_socket(
                state.clone(),
                socket,
                agent_id,
                tenant_id,
                agent.owner_user_id,
                tid,
            )
            .await;
        })
}
