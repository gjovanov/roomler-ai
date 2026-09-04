// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! WebSocket glue for the remote-control subsystem.
//!
//! The `roomler-ai-remote-control` crate owns the state machine and the
//! registry of agents/controllers ([`Hub`]). This module is the thin bridge
//! between an Axum [`WebSocket`] and the Hub: it pumps [`ServerMsg`] values
//! from a per-connection [`mpsc::Receiver`] out to the socket, parses inbound
//! [`ClientMsg`] values and forwards them to [`Hub::dispatch`].

use axum::extract::ws::{Message, WebSocket};
use bson::oid::ObjectId;
use futures::{SinkExt, StreamExt, stream::SplitSink};
use roomler_ai_mod_fleet::hub::DispatchCtx;
use roomler_ai_remote_control::{
    models::{ConsentMode, RpcCap},
    signaling::{AgentSysStats, ClientMsg, RelayRegionRtt, Role, ServerMsg},
    turn_creds::relay_regions_wire,
};
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};
use tracing::{debug, info, warn};

use crate::state::AppState;

// FR-69 P5a: the rc control pair (publish + apply) is the fleet module s now;
// re-exported so every path in this crate reads as before.
pub use roomler_ai_mod_fleet::ctrl::{apply_rc_ctrl, publish_rc_ctrl};

/// Handle a socket that authenticated as an agent.
///
/// Lifecycle: verify + look up agent, expect `rc:agent.hello`, register with
/// the Hub, then relay `rc:*` traffic in both directions until the socket closes.
pub async fn handle_agent_socket(
    state: AppState,
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
            && !crate::routes::overlay_key::order_is_satisfied(req, row.overlay_identity.as_ref())
            && crate::routes::overlay_key::should_redeliver(
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
    crate::ws::device_presence::note_transition(
        state.fleet(),
        tenant_id,
        agent_id,
        &device_name,
        crate::ws::device_presence::ONLINE,
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

    // P3b-2: an agent may drive the tunnel-CLIENT role over this same WS
    // (originate tunnel-client sessions to OTHER nodes). Bundle its
    // originator identity once — the reply channel is a clone of the
    // Hub-owned `registered_tx`, so a target's answers relayed via
    // `tunnel_clients_by_session` reach this socket through the pump above.
    // `tunnel_orig_sessions` tracks the sessions THIS agent originated, so
    // the interception can demux the bidirectional variants + tear them
    // down when the WS drops.
    let tunnel_orig = crate::ws::tunnel::Originator {
        principal: tunnel_core::policy::Principal::Agent(agent_id),
        tenant_id,
        owner_user_id,
        client_version: agent_version.clone(),
        client_os: os,
        outbound_tx: registered_tx.clone(),
        dialed_tid,
        conn_established_ms,
    };
    let mut tunnel_orig_sessions: std::collections::HashMap<
        ObjectId,
        crate::ws::tunnel::TunnelSession,
    > = std::collections::HashMap::new();
    let mut tunnel_orig_transports: Vec<String> = Vec::new();
    // Multi-region relay PoPs: per-connection persist rate limiter for probe
    // reports (the Hub's live copy refreshes on every report; Mongo doesn't
    // need to).
    let mut last_probe_persist: Option<std::time::Instant> = None;

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
                            // P3b-2: originator-side interception FIRST — if
                            // this agent is driving the tunnel-client role,
                            // consume its open/forward/relay/terminate here
                            // (Principal::Agent). Returns the value unchanged
                            // when this agent is the tunnel TARGET (or the
                            // message is non-tunnel), so it falls through below.
                            let Some(parsed) = crate::ws::tunnel::relay_tunnel_client_msg_from_agent(
                                &state,
                                &tunnel_orig,
                                &mut tunnel_orig_sessions,
                                &mut tunnel_orig_transports,
                                parsed,
                            )
                            .await
                            else {
                                continue;
                            };
                            // Tunnel-flow variants intercept next — Hub doesn't
                            // know about tunnel-clients, so we route directly
                            // through `AppState::tunnel_clients_by_session`. If
                            // it's not a tunnel-flow variant the helper returns
                            // the value unchanged for the Hub to handle.
                            let Some(parsed) = relay_tunnel_msg_from_agent(&state, parsed).await else {
                                continue;
                            };
                            // Overlay `rc:overlay.*` variants are brokered
                            // here too (the Hub doesn't know about the
                            // overlay). Consumed messages return None.
                            let Some(parsed) = crate::ws::overlay::relay_overlay_msg_from_node(
                                &state,
                                crate::ws::overlay::NodeIdentity::Agent(agent_id),
                                parsed,
                            )
                            .await
                            else {
                                continue;
                            };
                            // Multi-region relay PoPs: probe reports are
                            // consumed HERE (they need AppState + the per-
                            // connection persist rate limiter, which the Hub
                            // has no business holding).
                            if let ClientMsg::RelayProbeReport { results } = &parsed {
                                handle_relay_probe_report(
                                    &state,
                                    agent_id,
                                    results,
                                    &mut last_probe_persist,
                                )
                                .await;
                                continue;
                            }
                            // Multi-region DERP: mint an admission ticket for
                            // this agent's overlay node. Unanswerable (no
                            // signer / no overlay node) = silently unanswered;
                            // the agent keeps using the central /derp.
                            // Wave 2 — live session telemetry. Consumed
                            // HERE (needs Mongo, which the Hub doesn't
                            // hold) and scoped to THIS agent, so one
                            // agent can never write another's session.
                            if let ClientMsg::SessionStats {
                                session_id,
                                bytes_sent,
                                bytes_recv,
                                fps,
                                rtt_ms,
                                keyframe_requests,
                                input_events,
                                shared_seconds,
                                mixed_dial_seconds,
                            } = &parsed
                            {
                                if state.settings.stats.enabled
                                    && let Ok(sid) = ObjectId::parse_str(session_id)
                                {
                                    let stats =
                                        roomler_ai_remote_control::models::SessionStats {
                                            bytes_sent: *bytes_sent,
                                            bytes_recv: *bytes_recv,
                                            peak_fps: *fps,
                                            avg_rtt_ms: *rtt_ms,
                                            keyframe_requests: *keyframe_requests,
                                            input_events: *input_events,
                                            shared_seconds: *shared_seconds,
                                            mixed_dial_seconds: *mixed_dial_seconds,
                                        };
                                    if let Err(e) = state
                                        .remote_sessions
                                        .merge_live_stats(sid, agent_id, &stats)
                                        .await
                                    {
                                        debug!(%agent_id, %e, "session stats merge failed");
                                    }
                                }
                                continue;
                            }
                            if matches!(parsed, ClientMsg::DerpTicketRequest {}) {
                                handle_derp_ticket_request(&state, agent_id, &registered_tx).await;
                                continue;
                            }
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
                                // A device asking for an SSH session on
                                // ANOTHER device (`roomler ssh`). Spawned for
                                // the same reason as the exec leg: this is the
                                // REQUESTING agent's read loop, and gate
                                // evaluation touches the database.
                                ClientMsg::SshRequest {
                                    request_id,
                                    target,
                                    public_key,
                                    session_secs,
                                } => {
                                    let state = state.clone();
                                    let reply_tx = registered_tx.clone();
                                    tokio::spawn(async move {
                                        handle_agent_ssh_request(
                                            &state,
                                            tenant_id,
                                            agent_id,
                                            request_id,
                                            target,
                                            public_key,
                                            session_secs,
                                            reply_tx,
                                        )
                                        .await;
                                    });
                                    continue;
                                }
                                // P8: a device reporting what happened inside
                                // an SSH session. Fire-and-forget — nothing
                                // awaits it, so a failed insert must never
                                // disturb the connection.
                                //
                                // `tenant_id` / `agent_id` come from THIS
                                // authenticated connection and are never read
                                // off the frame, so a device can only write
                                // rows about itself. Everything else in the
                                // row is the device's unverified claim, which
                                // is why it lands in `ssh_activity` and not in
                                // the server-authored `ssh_audit`.
                                ClientMsg::SshActivity {
                                    grant_id,
                                    caller,
                                    kind,
                                    detail,
                                    exit_code,
                                    allowed,
                                } => {
                                    let state = state.clone();
                                    tokio::spawn(async move {
                                        record_ssh_activity(
                                            &state, tenant_id, agent_id, grant_id, caller, kind,
                                            detail, exit_code, allowed,
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
                                // FR-40 — the device's answer to a rotation
                                // order. A CLAIM, stored on the row like the
                                // config report; the join that follows is what
                                // the server can verify (`overlay_identity`).
                                ClientMsg::KeyRotated {
                                    request_id,
                                    outcome,
                                    old_public_key,
                                    new_public_key,
                                    key_epoch,
                                    detail,
                                } => {
                                    let state = state.clone();
                                    tokio::spawn(async move {
                                        record_key_rotation_report(
                                            &state,
                                            tenant_id,
                                            agent_id,
                                            request_id,
                                            outcome,
                                            old_public_key,
                                            new_public_key,
                                            key_epoch,
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
                                ..
                            } = &parsed
                            {
                                Some((
                                    *active_sessions,
                                    sys.clone(),
                                    *srflx_count,
                                    warm_relay.clone(),
                                    companion_version.clone(),
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
                            )) = heartbeat_sessions
                            {
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
                                // C4 stage 2 (PR-B) — mirror the warm leg onto
                                // the overlay-node row: the netmap is built
                                // from overlay_nodes, so only THIS write puts
                                // the pair-less dial target in front of peers.
                                if let Err(e) = state
                                    .overlay_nodes
                                    .set_warm_relay_for_agent(agent_id, warm_relay.as_deref())
                                    .await
                                {
                                    warn!(%agent_id, %e, "overlay-node warm-relay mirror failed");
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
    // P3b-2: tear down any tunnel-client sessions THIS agent originated —
    // tell each target agent, drop the reply-channel registration, and
    // audit the close (consumes the session map).
    crate::ws::tunnel::teardown_agent_originated_sessions(
        &state,
        &tunnel_orig,
        tunnel_orig_sessions,
    )
    .await;
    // P7 flap resilience: also terminate every tunnel session TARGETING this
    // agent — its per-connection tunnel peers died with this socket, so a
    // reconnected agent instance rejects every forward on them forever.
    // Terminating pushes the clients to re-open with fresh state. (A session
    // opened against a REPLACING connection during the handoff window may be
    // killed too; its flow supervisor re-opens it within seconds.)
    crate::ws::tunnel::terminate_sessions_targeting_agent(&state, agent_id).await;

    // Phase A-1 — the Mongo Offline write + presence release are GATED on
    // the hub removal being OURS: a displaced handler's late teardown must
    // not clobber the displacing connection's Online status or its fresh
    // presence claim (the status-race twin of the rc.53 registry fix).
    let removal_was_ours = state
        .rc_hub
        .unregister_agent(agent_id, Some(&registered_tx));
    pump.abort();
    if removal_was_ours {
        // If this agent was an overlay node, mark it offline + drop it from
        // peers' netmaps (best-effort; it re-syncs on its next join).
        //
        // rc.307 (B): gated on `removal_was_ours` — previously unconditional,
        // so a displaced/draining old connection's late teardown could mark
        // the overlay row Offline AFTER the replacing connection's re-join
        // set it Online, ghosting the node fleet-wide (`reachability()`
        // requires status==Online). Today's client rebuilt carriers every
        // reconnect and the resulting endpoint trickles healed the mis-mark
        // within seconds; the rc.307 persistent runtime's clean reattach
        // produces no such trickles, so the mis-mark would stick. Same
        // pattern as the A-1 status gate directly below.
        crate::ws::overlay::handle_overlay_leave(
            &state,
            crate::ws::overlay::NodeIdentity::Agent(agent_id),
        )
        .await;
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
            crate::ws::device_presence::note_transition(
                state.fleet(),
                tenant_id,
                agent_id,
                &machine_name,
                crate::ws::device_presence::OFFLINE,
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
///   [`roomler_ai_mod_fleet::agent_exec::authorize`]). Without it, one compromised laptop
///   would inherit its owner's exec rights across the whole fleet.
///
/// Cross-tenant is impossible by construction: the target is resolved WITHIN
/// `tenant_id`, which is this socket's own enrollment.
#[allow(clippy::too_many_arguments)]
async fn handle_agent_exec_request(
    state: &AppState,
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
    let caller = roomler_ai_mod_fleet::agent_exec::Caller {
        user_id: origin.owner_user_id,
        display: format!("{who} (via {})", origin.name),
        origin_agent_id: Some(origin_agent_id),
        source: "cli",
    };
    let body = roomler_ai_mod_fleet::agent_exec::ExecRequestBody {
        shell,
        command,
        timeout_ms,
        max_output_bytes: 0,
        cwd: None,
    };
    let res = roomler_ai_mod_fleet::agent_exec::dispatch(
        state.fleet(),
        tenant_id,
        &agent,
        &caller,
        &body,
    )
    .await;
    if reply_tx.try_send(reply(res.outcome)).is_err() {
        warn!(%origin_agent_id, %request_id, "rc:rpc.response undeliverable — origin WS gone");
    }
}

/// Persist one device-reported SSH activity row (P8).
///
/// Everything trustworthy in the row — which tenant, which device — is taken
/// from the authenticated connection by the caller and passed in here;
/// everything else is the device's claim. Best-effort by design: a log line
/// must never be able to disturb a live session, so a failed insert is warned
/// and dropped.
///
/// `detail` is re-clamped server-side. The device already caps it, but a
/// length bound that only exists on the reporting side is not a bound.
#[allow(clippy::too_many_arguments)]
async fn record_ssh_activity(
    state: &AppState,
    tenant_id: ObjectId,
    agent_id: ObjectId,
    grant_id: Option<String>,
    caller: String,
    kind: roomler_ai_remote_control::models::SshActivityKind,
    detail: Option<String>,
    exit_code: Option<i32>,
    allowed: bool,
) {
    use roomler_ai_remote_control::models::SshActivityEvent;

    let detail = detail.map(|mut d| {
        if d.chars().count() > SshActivityEvent::MAX_DETAIL {
            d = d.chars().take(SshActivityEvent::MAX_DETAIL).collect();
            d.push('…');
        }
        d
    });
    let event = SshActivityEvent {
        id: None,
        tenant_id,
        agent_id,
        grant_id,
        caller,
        kind,
        detail,
        exit_code,
        allowed,
        at: bson::DateTime::now(),
    };
    if let Err(e) = state.ssh_activity.record(event).await {
        warn!(%agent_id, %e, "ssh_activity insert failed");
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
    state: &AppState,
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

/// FR-40 — persist a device's [`ClientMsg::KeyRotated`] onto its agent row.
/// Same rules as [`record_config_report`]: last report wins, `reported_at`
/// is stamped here, `detail` is re-clamped on receipt. Public keys only —
/// the frame has no field for anything else, by construction.
#[allow(clippy::too_many_arguments)]
async fn record_key_rotation_report(
    state: &AppState,
    tenant_id: ObjectId,
    agent_id: ObjectId,
    request_id: String,
    outcome: roomler_ai_remote_control::models::KeyRotationOutcome,
    old_public_key: Option<String>,
    new_public_key: Option<String>,
    key_epoch: u32,
    detail: Option<String>,
) {
    use roomler_ai_remote_control::models::KeyRotationReport;

    let clamp = |s: String, max: usize| -> String {
        if s.chars().count() > max {
            let mut t: String = s.chars().take(max).collect();
            t.push('…');
            t
        } else {
            s
        }
    };
    // A WireGuard public key is 44 base64 chars; anything longer is not one.
    const MAX_KEY: usize = 64;
    let report = KeyRotationReport {
        request_id: clamp(request_id, 64),
        outcome,
        old_public_key: old_public_key.map(|k| clamp(k, MAX_KEY)),
        new_public_key: new_public_key.map(|k| clamp(k, MAX_KEY)),
        key_epoch,
        detail: detail.map(|d| clamp(d, KeyRotationReport::MAX_DETAIL)),
        reported_at: bson::DateTime::now(),
    };
    info!(
        %agent_id, request_id = %report.request_id, outcome = ?report.outcome,
        key_epoch, "overlay-key rotation reported by the device"
    );
    if let Err(e) = state
        .agents
        .record_key_rotation_report(tenant_id, agent_id, &report)
        .await
    {
        warn!(%agent_id, %e, "key rotation report write failed");
    }
}

/// The device-originated SSH leg (`roomler ssh <device>`).
///
/// Mirrors [`handle_agent_exec_request`] and goes through the SAME
/// [`agent_ssh::dispatch`] the HTTP route uses, so there is exactly one place
/// where the gates are evaluated regardless of how the request arrived.
///
/// [`agent_ssh::dispatch`]: crate::routes::agent_ssh::dispatch
#[allow(clippy::too_many_arguments)]
async fn handle_agent_ssh_request(
    state: &AppState,
    tenant_id: bson::oid::ObjectId,
    origin_agent_id: bson::oid::ObjectId,
    request_id: String,
    target: String,
    public_key: String,
    session_secs: u64,
    reply_tx: roomler_ai_remote_control::session::ClientTx,
) {
    // A refusal carries no address, so it carries no host key either — the two
    // are only ever meaningful together.
    let fail = |msg: String| ServerMsg::SshResponse {
        request_id: request_id.clone(),
        address: None,
        port: None,
        grant_id: None,
        host_pubkey: None,
        expires_at_ms: None,
        error: Some(msg),
    };

    // The origin's owner is the person whose permissions this runs under. A
    // device whose row vanished mid-flight has no principal, so it gets
    // nothing.
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

    let agent = match resolve_exec_target(state, tenant_id, &target).await {
        Some(a) => a,
        None => {
            let _ = reply_tx.try_send(fail(format!("no device named {target:?} in this org")));
            return;
        }
    };

    // "<person> (via <device>)" — the target's log and consent prompt name
    // both the human accountable and the box the request came from.
    let who = state
        .users
        .base
        .find_by_id(origin.owner_user_id)
        .await
        .map(|u| u.display_name)
        .unwrap_or_else(|_| origin.owner_user_id.to_hex());
    let caller = crate::routes::agent_ssh::Caller {
        user_id: origin.owner_user_id,
        display: format!("{who} (via {})", origin.name),
        origin_agent_id: Some(origin_agent_id),
    };

    let res = crate::routes::agent_ssh::dispatch(
        state,
        tenant_id,
        &agent,
        &caller,
        &public_key,
        session_secs,
    )
    .await;

    // EXHAUSTIVE — this hand-written mapping is how `host_pubkey` went missing
    // on this leg in the first place: the HTTP response gained the field and a
    // field-by-field literal silently kept sending the old shape, which
    // compiles perfectly. Binding every field means the next addition has to be
    // decided about rather than forgotten.
    let crate::routes::agent_ssh::SshResponseBody {
        address,
        port,
        // Dropped on purpose: this leg answers a device that named its own
        // target, so echoing the MagicDNS name back tells it nothing it did
        // not just say. The HTTP leg keeps it for display.
        name: _,
        grant_id,
        host_pubkey,
        expires_at_ms,
        error,
    } = res;
    let msg = ServerMsg::SshResponse {
        request_id: request_id.clone(),
        address,
        port,
        grant_id,
        host_pubkey,
        expires_at_ms,
        error,
    };
    if reply_tx.try_send(msg).is_err() {
        warn!(%origin_agent_id, %request_id, "rc:ssh.response undeliverable — origin WS gone");
    }
}

/// Resolve an exec target within one org: hex agent id first, then an
/// exact-then-case-insensitive device name.
async fn resolve_exec_target(
    state: &AppState,
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

/// Multi-region DERP: answer a ticket request. The ticket binds the agent's
/// overlay `(network_id, wg_public_key)` — exactly the invariants the central
/// `/derp` enforces from Mongo, so a PoP relay can enforce them with the
/// PUBLIC key alone.
async fn handle_derp_ticket_request(
    state: &AppState,
    agent_id: ObjectId,
    tx: &tokio::sync::mpsc::Sender<ServerMsg>,
) {
    let Some(signer) = &state.derp_ticket else {
        debug!(%agent_id, "derp ticket requested but no signer configured");
        return;
    };
    let Some(node) =
        crate::ws::overlay::current_node(state, crate::ws::overlay::NodeIdentity::Agent(agent_id))
            .await
    else {
        debug!(%agent_id, "derp ticket requested but agent has no overlay node");
        return;
    };
    match signer.mint(&node.network_id.to_hex(), &node.wg_public_key) {
        Ok((ticket, exp)) => {
            let _ = tx.try_send(ServerMsg::DerpTicket { ticket, exp });
        }
        Err(e) => warn!(%agent_id, %e, "derp ticket mint failed"),
    }
}

/// The agent's measured regions ordered by RTT (nearest first) — the
/// load-aware fallback ladder handed to the Hub alongside `relay_home`.
fn prefs_from_rtt(results: &[RelayRegionRtt]) -> Vec<String> {
    let mut measured: Vec<(u32, &str)> = results
        .iter()
        .filter_map(|r| r.rtt_ms.map(|ms| (ms, r.region.as_str())))
        .collect();
    measured.sort_unstable();
    measured.into_iter().map(|(_, r)| r.to_string()).collect()
}

/// Minimum spacing between Mongo persists of an agent's probe table. The
/// Hub's live copy refreshes on EVERY report regardless.
const PROBE_PERSIST_MIN_INTERVAL: std::time::Duration = std::time::Duration::from_secs(300);

/// Multi-region relay PoPs: derive the agent's `relay_home` from a probe
/// report and fan it out (Hub live copy always; Mongo rate-limited).
///
/// Hysteresis: the home only MOVES when the best region improves on the
/// current home's measured RTT by >20%, or the current home stopped being
/// measurable (dropped from the region set, or all its samples timed out).
/// This keeps a border-line agent from flapping between two near-equal PoPs —
/// the sticky pair cache protects live pairs, this protects everything else.
async fn handle_relay_probe_report(
    state: &AppState,
    agent_id: ObjectId,
    results: &[RelayRegionRtt],
    last_persist: &mut Option<std::time::Instant>,
) {
    let map = &state.turn_map;
    if !map.enabled || map.regions.is_empty() {
        return;
    }
    let known: Vec<&RelayRegionRtt> = results
        .iter()
        .filter(|r| map.regions.contains_key(&r.region))
        .collect();
    let best = known
        .iter()
        .filter_map(|r| r.rtt_ms.map(|ms| (ms, r.region.as_str())))
        .min();
    let current = state.rc_hub.agent_relay_home(agent_id);
    let new_home: Option<String> = match (best, current.as_deref()) {
        // Nothing measurable (full-UDP-block / dead PoPs) → default region.
        (None, _) => None,
        (Some((_, b)), None) => Some(b.to_string()),
        (Some((best_ms, b)), Some(cur)) => {
            let cur_ms = known
                .iter()
                .find(|r| r.region == cur)
                .and_then(|r| r.rtt_ms);
            match cur_ms {
                None => Some(b.to_string()),
                Some(c) if f64::from(best_ms) < f64::from(c) * 0.8 => Some(b.to_string()),
                Some(_) => Some(cur.to_string()),
            }
        }
    };
    state
        .rc_hub
        .set_agent_relay_home(agent_id, new_home.clone(), prefs_from_rtt(results));
    let due = last_persist
        .map(|t| t.elapsed() >= PROBE_PERSIST_MIN_INTERVAL)
        .unwrap_or(true);
    if !due {
        return;
    }
    *last_persist = Some(std::time::Instant::now());
    if let Err(e) = state
        .agents
        .set_relay_home(agent_id, new_home.as_deref(), results)
        .await
    {
        warn!(%agent_id, %e, "set_relay_home (agents) failed");
    }
    if let Err(e) = state
        .overlay_nodes
        .set_relay_home_for_agent(agent_id, new_home.as_deref())
        .await
    {
        warn!(%agent_id, %e, "set_relay_home (overlay_nodes) failed");
    }
    debug!(%agent_id, home = ?new_home, "relay probe report processed");
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

/// Route a parsed `rc:*` message coming from a controller browser tab.
/// Returns `true` if the message was handled, `false` if it wasn't rc:*.
#[allow(clippy::too_many_arguments)]
pub async fn dispatch_controller_rc(
    state: &AppState,
    user_id: ObjectId,
    controller_name: &str,
    controller_tx: &roomler_ai_remote_control::session::ClientTx,
    text: &str,
    consent_mode: ConsentMode,
    override_reason: Option<String>,
    // P6 — the device's `AccessPolicy.input_mode` (resolved by the authz
    // gate alongside `consent_mode`); forwarded to the agent's arbiter.
    input_mode: Option<roomler_ai_remote_control::models::InputMode>,
    // Multi-org — the org name the host's consent prompt should name
    // (resolved by the same gate).
    tenant_name: Option<String>,
    // PR-1 rehome direction inputs: the affinity key this conn DIALED
    // with (None = key-less legacy/racy dial) and when it established.
    dialed_tid: Option<&str>,
    conn_established_ms: i64,
    // PR-2 relay: this browser socket's connection id - the address the
    // owner pod's proxy pump routes replies back to.
    connection_id: &str,
) -> bool {
    let hub = &state.rc_hub;
    let Ok(parsed) = serde_json::from_str::<ClientMsg>(text) else {
        return false;
    };
    let is_session_request = matches!(&parsed, ClientMsg::SessionRequest { .. });
    let ctx = DispatchCtx {
        role: Role::Controller,
        user_id: Some(user_id),
        agent_id: None,
        controller_name: Some(controller_name.to_string()),
        controller_tx: Some(controller_tx.clone()),
        consent_mode,
        // PR-2: the relay needs its own copy after ctx takes this one.
        override_reason: override_reason.clone(),
        input_mode,
        tenant_name: tenant_name.clone(),
    };
    // …and so does the cross-pod relay, for the same reason.
    let relay_tenant_name = tenant_name;
    if let Err(e) = hub.dispatch(&ctx, parsed) {
        warn!(%user_id, %e, "rc:* dispatch failed (controller)");
        // C-2 rehome: a SessionRequest that missed the LOCAL hub while a
        // FOREIGN pod holds a fresh presence record is a cross-pod split,
        // not a real offline. Tell the controller which state it's in
        // (`agent_on_other_pod` → the UI force-redials its WS and retries
        // once) and nudge the owning pod to cycle the agent's WS if idle —
        // both ends then re-land at the current LB hash. Probe budget
        // 250 ms; on any probe failure fall through to the honest
        // `agent_offline`.
        if is_session_request
            && let roomler_ai_remote_control::error::Error::AgentOffline(agent_hex) = &e
            && let Some(redis) = &state.redis_pubsub
            && let Ok(Ok(Some(owner))) = tokio::time::timeout(
                std::time::Duration::from_millis(250),
                redis.agent_presence_foreign(agent_hex),
            )
            .await
        {
            note_agent_offline_evidence(state, agent_hex.clone(), "controller_session");
            crate::cluster::metrics::bump(&crate::cluster::metrics::RC_REHOME_TOTAL);
            let record = crate::cluster::directory::OwnerRecord::parse(&owner);
            let owner_pod = record
                .as_ref()
                .map(|r| r.pod_id.clone())
                .unwrap_or_default();
            let agent_since_ms = record.map(|r| r.since_ms).unwrap_or(0);
            // PR-2 relay FIRST: make the cross-pod session WORK now;
            // convergence (controller re-key, or the agent's own next
            // natural reconnect) proceeds without user-visible errors.
            // No nudge on the relay path - an idle-nudge racing the
            // relayed create would tear the session it just built.
            if let Ok(frame_val) = serde_json::from_str::<serde_json::Value>(text) {
                match crate::ws::rc_relay::relay_rc_frame(
                    state,
                    &owner_pod,
                    connection_id,
                    user_id,
                    controller_name,
                    consent_mode,
                    &override_reason,
                    input_mode,
                    &relay_tenant_name,
                    &frame_val,
                )
                .await
                {
                    Ok(None) => {
                        info!(
                            %user_id,
                            agent = %agent_hex,
                            %owner_pod,
                            "cross-pod rc miss: relayed to owner pod"
                        );
                        return true;
                    }
                    Ok(Some((code, message))) => {
                        // The owner's Hub answered authoritatively
                        // (agent_busy, permission_denied, ...) - same
                        // surface as a local dispatch failure.
                        let _ = controller_tx.try_send(ServerMsg::Error {
                            session_id: None,
                            code,
                            message,
                            open_nonce: None,
                        });
                        return true;
                    }
                    Err(()) => { /* relay unavailable - PR-1 path below */ }
                }
            }
            // PR-1 direction rule: only a correctly-keyed conn that is
            // provably NEWER than the agent's registration justifies
            // nudging the agent; every other shape means the CONTROLLER
            // is the mis-placed party (the 2026-08-04 incident) and a
            // nudge would bounce a correctly-homed, possibly busy agent.
            // One agent lookup for its tenant; on failure the helper
            // falls back to controller-moves (never nudge on doubt).
            let agent_tenant_hex = match ObjectId::parse_str(agent_hex) {
                Ok(aid) => state
                    .agents
                    .base
                    .find_by_id(aid)
                    .await
                    .map(|a| a.tenant_id.to_hex())
                    .unwrap_or_default(),
                Err(_) => String::new(),
            };
            let direction = crate::ws::rc_cluster::rehome_direction(
                dialed_tid,
                &agent_tenant_hex,
                conn_established_ms,
                agent_since_ms,
                state.settings.rc.rehome_direction_guard_ms,
            );
            match direction {
                crate::ws::rc_cluster::RehomeDirection::ControllerMove { reason } => {
                    crate::cluster::metrics::bump(
                        &crate::cluster::metrics::RC_REHOME_CONTROLLER_TOTAL,
                    );
                    info!(
                        %user_id,
                        agent = %agent_hex,
                        %owner_pod,
                        ?dialed_tid,
                        reason,
                        "cross-pod rc miss: controller re-dials (nudge suppressed)"
                    );
                }
                crate::ws::rc_cluster::RehomeDirection::NudgeAgent => {
                    info!(
                        %user_id,
                        agent = %agent_hex,
                        %owner_pod,
                        "cross-pod rc miss: agent judged parked; nudging owner pod"
                    );
                    spawn_agent_nudge(state, owner_pod, agent_hex.clone());
                }
            }
            // Pod identity stays in server logs; the wire carries only
            // the actionable fact (the UI re-keys + redials on it).
            let _ = controller_tx.try_send(ServerMsg::Error {
                session_id: None,
                code: "agent_on_other_pod".to_string(),
                message: "agent is homed on another pod; re-dial and retry".to_string(),
                open_nonce: None,
            });
            return true;
        }
        // PR-2: a session-scoped frame (answer / ice / terminate) whose
        // session lives on another pod - this conn relayed its create
        // there. Forward it: the owner holding the session dispatches;
        // an owner that lost it answers session_not_found, which is the
        // honest surface either way.
        if matches!(
            &e,
            roomler_ai_remote_control::error::Error::SessionNotFound(_)
        ) {
            let owners: Vec<String> = state
                .remote_rc_conns
                .get(connection_id)
                .map(|s| s.iter().cloned().collect())
                .unwrap_or_default();
            if !owners.is_empty()
                && let Ok(frame_val) = serde_json::from_str::<serde_json::Value>(text)
            {
                let mut refusal: Option<(String, String)> = None;
                for owner in owners {
                    match crate::ws::rc_relay::relay_rc_frame(
                        state,
                        &owner,
                        connection_id,
                        user_id,
                        controller_name,
                        consent_mode,
                        &override_reason,
                        input_mode,
                        &relay_tenant_name,
                        &frame_val,
                    )
                    .await
                    {
                        Ok(None) => return true,
                        Ok(Some(cm)) => refusal = Some(cm),
                        Err(()) => {}
                    }
                }
                if let Some((code, message)) = refusal {
                    let _ = controller_tx.try_send(ServerMsg::Error {
                        session_id: error_session_id(&e),
                        code,
                        message,
                        open_nonce: None,
                    });
                    return true;
                }
            }
        }
        // Phase A-1 split-evidence probe for the remaining offline paths
        // (non-session-request messages, probe timeouts).
        if let roomler_ai_remote_control::error::Error::AgentOffline(agent_hex) = &e {
            note_agent_offline_evidence(state, agent_hex.clone(), "controller_session");
        }
        // Surface the failure to the controller so the UI can exit its
        // "Requesting session…" spinner instead of hanging. Best-effort —
        // the controller may already be closing.
        let _ = controller_tx.try_send(ServerMsg::Error {
            session_id: error_session_id(&e),
            code: error_code(&e).to_string(),
            message: e.to_string(),
            open_nonce: None,
        });
    }
    true
}

/// Result of the session authz gate: the effective consent mode plus a VALIDATED
/// admin break-glass reason (Phase 5). `override_reason` is `Some` only when an
/// `ADMINISTRATOR` force-started a device they don't own with a non-empty reason
/// — in which case `mode` is `Auto` (consent skipped) and the Hub records an
/// `AdminOverride` audit.
pub struct SessionAuthz {
    pub mode: ConsentMode,
    pub override_reason: Option<String>,
    /// P6 — the device's `AccessPolicy.input_mode` (None = free default).
    pub input_mode: Option<roomler_ai_remote_control::models::InputMode>,
    /// Multi-org — display name of the organization this session happens in,
    /// so the host's consent prompt can say WHICH org is asking. Resolved
    /// from the agent row the gate already loads. `None` on the early-out
    /// paths (non-session-request, unknown agent) where no prompt follows.
    pub tenant_name: Option<String>,
}

impl SessionAuthz {
    fn allow(mode: ConsentMode) -> Self {
        Self {
            mode,
            override_reason: None,
            input_mode: None,
            tenant_name: None,
        }
    }
    fn allow_with_input(
        mode: ConsentMode,
        input_mode: Option<roomler_ai_remote_control::models::InputMode>,
        tenant_name: Option<String>,
    ) -> Self {
        Self {
            mode,
            override_reason: None,
            input_mode,
            tenant_name,
        }
    }
}

/// Authorization + consent-mode gate for `rc:session.request` — the Hub can't
/// do this because the `remote_control` crate sits below `services` in the dep
/// graph and has no access to tenant roles. Returns `Ok(SessionAuthz)` for an
/// allowed request, or `Err(reason)` to DENY. A non-`SessionRequest` rc:* message
/// returns `Ok(allow(Prompt))` — those are intra-session and the mode is unused.
///
/// Layers (coarse→fine authz): quarantine → self-control → tenant capability
/// (`ADMINISTRATOR` / `REMOTE_CONTROL`) → per-agent allowlist (empty = no
/// per-device restriction). Consent mode: self-control → `Auto`; else the
/// device's `effective_consent_mode()`. **Break-glass (Phase 5):** an
/// `ADMINISTRATOR` who sends a non-empty `override_reason` for a device they
/// don't own gets `Auto` (consent skipped) + the reason carried through for the
/// `AdminOverride` audit.
pub async fn resolve_session_authz(
    state: &AppState,
    controller_user_id: ObjectId,
    text: &str,
) -> Result<SessionAuthz, String> {
    use roomler_ai_db::models::role::permissions;
    use roomler_ai_remote_control::models::AgentStatus;

    let (agent_id, override_reason) = match serde_json::from_str::<ClientMsg>(text) {
        Ok(ClientMsg::SessionRequest {
            agent_id,
            override_reason,
            ..
        }) => (agent_id, override_reason),
        // Not a session request → the mode is unused; allow through.
        _ => return Ok(SessionAuthz::allow(ConsentMode::Prompt)),
    };

    // Unknown / soft-deleted agent → let the Hub answer with a clean
    // AgentNotFound rather than surfacing a permission error (the mode is moot —
    // create_session will fail on the agent lookup).
    let agent = match state.agents.base.find_by_id(agent_id).await {
        Ok(a) if a.deleted_at.is_none() => a,
        _ => return Ok(SessionAuthz::allow(ConsentMode::Prompt)),
    };

    if agent.status == AgentStatus::Quarantined {
        return Err("device is quarantined; new sessions are blocked".to_string());
    }

    // P6 — the device's input arbitration mode rides every allowed outcome.
    let input_mode = agent.access_policy.input_mode;

    // Multi-org — the organization name the host's consent prompt will show.
    // One extra read, and only on a real session request (the early-outs above
    // never reach here). A failed lookup degrades the prompt to the agent's own
    // org label rather than failing the session.
    // The SAME read answers "is this org archived?", which must refuse the
    // session: an archived org stops acting (`routes::tenant::archive`).
    // A failed lookup degrades the prompt to the agent's own org label
    // rather than failing the session — it must not, however, be read as
    // "not archived" on a row we could not see, so only a successful read
    // can refuse.
    let tenant = state.tenants.base.find_by_id(agent.tenant_id).await.ok();
    if tenant.as_ref().is_some_and(|t| t.is_archived) {
        return Err("this organization is archived; new sessions are blocked".to_string());
    }
    let tenant_name = tenant.map(|t| t.name);

    // Controlling your OWN device is always allowed. Whether it also
    // auto-consents is now the DEVICE's call (FR-27): `prompt_owner` unset —
    // every pre-FR-27 row, and the default — keeps the historical shortcut,
    // because unattended access to your own headless boxes is the common case
    // and prompting them asks a machine nobody is sitting at.
    //
    // ⚠️ This shortcut is why the consent picker looked broken. It short-circuits
    // BEFORE `effective_consent_mode()` is ever read, so on a fleet where one
    // person owns every device the setting had no observable effect at all —
    // and nothing on screen said so. The UI now labels it, and this flag is how
    // an owner opts into being asked (a shared workstation, or field-testing the
    // attended modes without a second account).
    if agent.owner_user_id == controller_user_id {
        return Ok(SessionAuthz::allow_with_input(
            agent.access_policy.owner_consent_mode(),
            input_mode,
            tenant_name,
        ));
    }

    // The effective mode for an allowed non-owner controller (attended default).
    let mode = agent.access_policy.effective_consent_mode();

    let perms = state
        .tenants
        .get_member_permissions(agent.tenant_id, controller_user_id)
        .await
        .unwrap_or(0);
    if permissions::has(perms, permissions::ADMINISTRATOR) {
        // Phase 5 break-glass: an ADMINISTRATOR may SKIP consent, but only with a
        // non-empty reason. A blank/absent reason ⇒ the admin gets the device's
        // normal consent mode (no forced override).
        if let Some(reason) = override_reason.filter(|r| !r.trim().is_empty()) {
            return Ok(SessionAuthz {
                mode: ConsentMode::Auto,
                override_reason: Some(reason),
                input_mode,
                tenant_name,
            });
        }
        return Ok(SessionAuthz::allow_with_input(
            mode,
            input_mode,
            tenant_name.clone(),
        ));
    }
    if !permissions::has(perms, permissions::REMOTE_CONTROL) {
        return Err("you don't have permission to control others' devices".to_string());
    }

    // Per-agent allowlist. Empty ⇒ no per-device restriction (any operator may
    // request; consent is the real gate). Non-empty ⇒ user or a role must match.
    let policy = &agent.access_policy;
    if policy.allowed_user_ids.is_empty() && policy.allowed_role_ids.is_empty() {
        return Ok(SessionAuthz::allow_with_input(
            mode,
            input_mode,
            tenant_name.clone(),
        ));
    }
    if policy.allowed_user_ids.contains(&controller_user_id) {
        return Ok(SessionAuthz::allow_with_input(
            mode,
            input_mode,
            tenant_name.clone(),
        ));
    }
    let role_ids = state
        .tenants
        .member_role_ids(agent.tenant_id, controller_user_id)
        .await
        .unwrap_or_default();
    if policy.allowed_role_ids.iter().any(|r| role_ids.contains(r)) {
        return Ok(SessionAuthz::allow_with_input(
            mode,
            input_mode,
            tenant_name.clone(),
        ));
    }
    Err("you're not on this device's control allowlist".to_string())
}

/// C-2/C-3 — fire-and-forget idle-agent nudge at the pod owning an
/// agent's WS (read from its directory record): the owner cycles the
/// socket iff the agent is fully idle, so its reconnect re-hashes onto
/// the current LB map. Failure is harmless (the requester's own retry
/// path still works; the agent converges on its next natural reconnect).
pub(crate) fn spawn_agent_nudge(state: &AppState, owner_pod: String, agent_hex: String) {
    let Some(bus) = state.cluster_bus.clone() else {
        return;
    };
    if owner_pod.is_empty() {
        return;
    }
    // PR-1 requester-side throttle: a controller click storm + retry
    // ladder sent 11 nudge RPCs in 15 s at one refusing owner in the
    // 2026-08-04 incident. The owner has its own cooldown; this just
    // keeps the bus quiet.
    if let Ok(aid) = ObjectId::parse_str(&agent_hex)
        && !crate::ws::rc_cluster::nudge_request_allowed(
            &state.agent_nudge_throttle,
            aid,
            std::time::Duration::from_millis(state.settings.rc.nudge_requester_throttle_ms),
        )
    {
        debug!(agent = %agent_hex, "agent nudge RPC suppressed (requester throttle)");
        return;
    }
    tokio::spawn(async move {
        match bus
            .request(
                &owner_pod,
                "rc.agent_nudge",
                serde_json::json!({ "agent_id": agent_hex }),
            )
            .await
        {
            Ok(rep) => {
                let nudged = rep.get("nudged").and_then(|v| v.as_bool()).unwrap_or(false);
                // `reason` is absent from pre-PR-1 peers (mixed-version
                // roll) — tolerate.
                let reason = rep.get("reason").and_then(|v| v.as_str()).unwrap_or("");
                if nudged {
                    info!(agent = %agent_hex, %owner_pod, "agent rehome nudge fired on owner pod");
                } else {
                    info!(
                        agent = %agent_hex,
                        %owner_pod,
                        reason,
                        "agent rehome nudge refused by owner pod"
                    );
                }
            }
            Err(e) => debug!(agent = %agent_hex, %e, "agent rehome nudge failed"),
        }
    });
}

/// Phase A-1 split-evidence probe (A2b): fired on a LOCAL hub miss; if
/// another pod holds a FRESH presence record for the agent, that miss was
/// a cross-pod split, not a real offline. One warn + a process counter —
/// the permanent field instrument that gates the Phase A-2 rehome work
/// (steady-state nonzero = stable split; spikes only around rolls =
/// churn). Fire-and-forget: never blocks the caller.
pub(crate) fn note_agent_offline_evidence(
    state: &AppState,
    agent_hex: String,
    caller: &'static str,
) {
    // Probe throttle: the tunnel-ICE path can miss at candidate rate
    // (>10/s in the 2026-08-02 incident); one Redis GET per 5 s is
    // plenty for an existence instrument.
    static LAST_PROBE_MS: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
    let now_ms = bson::DateTime::now().timestamp_millis();
    let last = LAST_PROBE_MS.load(std::sync::atomic::Ordering::Relaxed);
    if now_ms - last < 5_000
        || LAST_PROBE_MS
            .compare_exchange(
                last,
                now_ms,
                std::sync::atomic::Ordering::Relaxed,
                std::sync::atomic::Ordering::Relaxed,
            )
            .is_err()
    {
        return;
    }
    let Some(redis) = state.redis_pubsub.clone() else {
        return;
    };
    tokio::spawn(async move {
        match redis.agent_presence_foreign(&agent_hex).await {
            Ok(Some(owner)) => {
                let total = crate::cluster::metrics::SPLIT_EVIDENCE_TOTAL
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                    + 1;
                warn!(
                    agent = %agent_hex, caller, owner = %owner, total,
                    "SPLIT EVIDENCE: local hub miss but another pod holds a fresh presence record"
                );
            }
            Ok(None) => {}
            Err(e) => debug!(%agent_hex, %e, "split-evidence probe failed"),
        }
    });
}

/// Stable short code for the wire. Exhaustive match so a new
/// `remote_control::Error` variant triggers a compile error here rather
/// than silently being reported as "internal".
pub(crate) fn error_code(e: &roomler_ai_remote_control::Error) -> &'static str {
    use roomler_ai_remote_control::Error::*;
    match e {
        AgentOffline(_) => "agent_offline",
        AgentNotFound(_) => "agent_not_found",
        AgentBusy => "agent_busy",
        SessionNotFound(_) => "session_not_found",
        BadPhase(_, _) => "bad_phase",
        ConsentDenied => "consent_denied",
        ConsentTimeout => "consent_timeout",
        PermissionDenied(_) => "permission_denied",
        BadMessage(_) => "bad_message",
        SendFailed => "send_failed",
        // Fleet RPC. Distinct codes because the operator actions differ:
        // "update the agent" vs "the device stopped answering".
        ExecUnsupported(_) => "exec_unsupported",
        ExecTimeout(_) => "exec_timeout",
        Mongo(_) => "internal",
        Bson(_) => "internal",
        Json(_) => "internal",
    }
}

/// If the underlying error references a specific session, extract its id
/// so the controller UI can route the error to the right spinner instead
/// of assuming it's about the most recently attempted session.
fn error_session_id(e: &roomler_ai_remote_control::Error) -> Option<bson::oid::ObjectId> {
    use roomler_ai_remote_control::Error::*;
    match e {
        SessionNotFound(hex) => bson::oid::ObjectId::parse_str(hex).ok(),
        BadPhase(hex, _) => bson::oid::ObjectId::parse_str(hex).ok(),
        _ => None,
    }
}

/// Intercept tunnel-flow `ClientMsg` variants from the agent and route
/// the corresponding `ServerMsg` to the registered tunnel-client (if
/// any) keyed by `session_id`. Non-tunnel variants are returned
/// unchanged so the caller can pass them to the Hub.
///
/// Returns `None` if the message was consumed by the tunnel relay
/// (don't dispatch to the Hub afterwards), or `Some(parsed)` if the
/// caller should continue with Hub dispatch.
async fn relay_tunnel_msg_from_agent(state: &AppState, parsed: ClientMsg) -> Option<ClientMsg> {
    match parsed {
        ClientMsg::TcpForwardAccept {
            session_id,
            flow_id,
            dc_index,
        } => {
            relay_to_client(
                state,
                session_id,
                ServerMsg::TcpForwardAccept {
                    session_id,
                    flow_id,
                    dc_index,
                },
            )
            .await;
            None
        }
        ClientMsg::TcpForwardReject {
            session_id,
            flow_id,
            kind,
            reason,
        } => {
            relay_to_client(
                state,
                session_id,
                ServerMsg::TcpForwardReject {
                    session_id,
                    flow_id,
                    kind,
                    reason,
                },
            )
            .await;
            None
        }
        ClientMsg::TcpHalfClose {
            session_id,
            flow_id,
            direction,
        } => {
            relay_to_client(
                state,
                session_id,
                ServerMsg::TcpHalfClose {
                    session_id,
                    flow_id,
                    direction,
                },
            )
            .await;
            None
        }
        // The AGENT end of a flow closing. Byte counts are deliberately
        // ignored here: the audit row is written once, from the
        // ORIGINATOR's close (see `ws::tunnel::audit_tcp_close`), and
        // booking both ends would double every flow's volume.
        ClientMsg::TcpClosed {
            session_id,
            flow_id,
            reason,
            ..
        } => {
            relay_to_client(
                state,
                session_id,
                ServerMsg::TcpClosed {
                    session_id,
                    flow_id,
                    reason,
                },
            )
            .await;
            None
        }
        // UDP ASSOCIATE relays — mirror the Tcp* variants above. The
        // agent bound a UDP socket (Accept) / rejected / closed a UDP
        // flow; relay each to the tunnel-client by session_id.
        ClientMsg::UdpForwardAccept {
            session_id,
            flow_id,
            dc_index,
        } => {
            relay_to_client(
                state,
                session_id,
                ServerMsg::UdpForwardAccept {
                    session_id,
                    flow_id,
                    dc_index,
                },
            )
            .await;
            None
        }
        ClientMsg::UdpForwardReject {
            session_id,
            flow_id,
            kind,
            reason,
        } => {
            relay_to_client(
                state,
                session_id,
                ServerMsg::UdpForwardReject {
                    session_id,
                    flow_id,
                    kind,
                    reason,
                },
            )
            .await;
            None
        }
        ClientMsg::UdpClosed {
            session_id,
            flow_id,
            reason,
        } => {
            relay_to_client(
                state,
                session_id,
                ServerMsg::UdpClosed {
                    session_id,
                    flow_id,
                    reason,
                },
            )
            .await;
            None
        }
        ClientMsg::TunnelTerminate { session_id, reason } => {
            relay_to_client(
                state,
                session_id,
                ServerMsg::TunnelTerminate { session_id, reason },
            )
            .await;
            None
        }
        ClientMsg::TunnelSdpAnswer { session_id, sdp } => {
            relay_to_client(
                state,
                session_id,
                ServerMsg::TunnelSdpAnswer { session_id, sdp },
            )
            .await;
            None
        }
        ClientMsg::TunnelIce {
            session_id,
            candidate,
        } => {
            relay_to_client(
                state,
                session_id,
                ServerMsg::TunnelIce {
                    session_id,
                    candidate,
                },
            )
            .await;
            None
        }
        // Phase 1c: the agent's QUIC endpoint is up — relay its cert
        // fingerprint (for the client to pin) + dialable addrs to the
        // tunnel-client so it can connect the direct P2P QUIC link.
        ClientMsg::TunnelQuicReady {
            session_id,
            cert_fingerprint,
            addrs,
            derp_pubkey,
        } => {
            relay_to_client(
                state,
                session_id,
                ServerMsg::TunnelQuicReady {
                    session_id,
                    cert_fingerprint,
                    addrs,
                    // R4 — relayed verbatim; the client needs the agent's
                    // DERP identity to dial the quic-derp-v1 leg.
                    derp_pubkey,
                },
            )
            .await;
            None
        }
        // `TunnelHello` / `TunnelOpen` / `TcpForwardRequest` /
        // `TunnelSdpOffer` are tunnel-client → server messages;
        // agents shouldn't emit them. Pass through to the Hub so a
        // misbehaving agent gets a `bad_message` rather than being
        // silently dropped.
        other => Some(other),
    }
}

/// Push a `ServerMsg` to the tunnel-client registered for
/// `session_id`. No-op when the client has gone away (peer torn
/// down between agent emit + relay).
async fn relay_to_client(state: &AppState, session_id: bson::oid::ObjectId, msg: ServerMsg) {
    let Some(tx) = state
        .tunnel_clients_by_session
        .get(&session_id)
        .map(|entry| entry.value().clone())
    else {
        debug!(%session_id, "agent → client relay: no registered tunnel-client; dropping");
        // C-3 split evidence: if ANOTHER pod owns this session's record,
        // the drop was a cross-pod split (the agent's WS re-homed away
        // from the client's pod), not a torn-down client. Counted like
        // the A2b agent probe; throttled to 1/5 s.
        if let Some(dir) = state.cluster_directory.clone() {
            static LAST_MS: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

            let now = bson::DateTime::now().timestamp_millis();
            let last = LAST_MS.load(std::sync::atomic::Ordering::Relaxed);
            if now - last >= 5_000
                && LAST_MS
                    .compare_exchange(
                        last,
                        now,
                        std::sync::atomic::Ordering::Relaxed,
                        std::sync::atomic::Ordering::Relaxed,
                    )
                    .is_ok()
            {
                tokio::spawn(async move {
                    let key = crate::cluster::directory::tunnel_key(&session_id.to_hex());
                    if let Ok(Some(owner)) = dir.get(&key).await
                        && dir.is_foreign(&owner)
                    {
                        let total = crate::cluster::metrics::SPLIT_EVIDENCE_TOTAL
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                            + 1;
                        warn!(
                            session = %session_id, owner = %owner, total,
                            "SPLIT EVIDENCE: tunnel relay dropped but another pod owns the session"
                        );
                    }
                });
            }
        }
        return;
    };
    if let Err(e) = tx.send(msg).await {
        debug!(%session_id, %e, "agent → client relay: channel closed");
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
