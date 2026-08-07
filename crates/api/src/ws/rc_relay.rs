//! PR-2 — cross-pod rc signalling relay: `rc.cmd` / `rc.conn_closed` /
//! `rc.conn_alive`.
//!
//! PR-1 made cross-pod rc misses CONVERGE (re-key the controller, nudge
//! a provably-parked idle agent). This module makes them WORK in the
//! meantime: a controller whose WS sits on pod A can run a session with
//! an agent homed on pod B by forwarding its raw `rc:*` frames over the
//! per-pod bus; every server->controller message routes back over the
//! C-4 conn-addressed lane (`send_to_connection_routed` is
//! payload-agnostic). Co-location then becomes a latency optimization,
//! not a correctness requirement: the relay adds one bus hop to
//! SIGNALLING only — media and input always ride the P2P WebRTC planes.
//!
//! Topology per relayed controller connection:
//!
//! ```text
//! browser ==ws== pod A                         pod B ==ws== agent
//!    rc:* frame --> rc.cmd{conn,frame,...} --> proxy DispatchCtx
//!                                              -> Hub::dispatch
//!    <-- global channel conn-envelope <-- proxy pump <- ClientTx
//! ```
//!
//! Lifecycle: the owner pod keeps ONE proxy controller (Hub
//! registration + pump task) per origin `conn`, created lazily on the
//! first `rc.cmd`. It dies on: the origin pod's WS-close notice
//! (`rc.conn_closed` - the primary path, mirroring C-4's
//! `remote_media_conns` forwarding), or the janitor sweep discovering
//! the origin conn gone / origin pod dead (`rc.conn_alive` probe, the
//! belt for a crashed origin pod that could never send the notice).
//! The rc.185 orphan reap in `create_session` remains the last-resort
//! braces.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use mongodb::bson::oid::ObjectId;
use roomler_ai_remote_control::hub::DispatchCtx;
use roomler_ai_remote_control::models::{ConsentMode, InputMode};
use roomler_ai_remote_control::signaling::{ClientMsg, Role};
use tracing::{debug, info, warn};

use crate::state::AppState;

/// One live proxy controller on the OWNER pod, keyed by the origin
/// connection id.
pub struct ProxyController {
    pub user_id: ObjectId,
    /// The Hub-registered ClientTx (identity key for unregister).
    pub tx: roomler_ai_remote_control::session::ClientTx,
    /// Pod that holds the real browser socket (sweep probe target).
    pub origin_pod: String,
    /// Refreshed on every forwarded frame; the sweep only probes
    /// entries that went quiet (a HEALTHY session is signalling-quiet,
    /// so quiet alone is never grounds for teardown).
    pub last_seen: std::sync::Mutex<Instant>,
    pump: tokio::task::JoinHandle<()>,
}

/// origin conn id -> proxy.
pub type ProxyControllers = DashMap<String, ProxyController>;

/// Sweep cadence + probe threshold. Quiet entries get an
/// `rc.conn_alive` probe at most once per sweep; only a NEGATIVE or
/// dead-pod answer tears down.
const SWEEP_EVERY: Duration = Duration::from_secs(60);
const PROBE_WHEN_QUIET_FOR: Duration = Duration::from_secs(120);

/// Register the owner-side bus verbs + the janitor sweep. Called once
/// after AppState is built (needs the full state), next to
/// `wire_media_cluster` / `wire_derp_cluster`.
pub fn wire_rc_relay(state: &AppState) {
    let Some(bus) = state.cluster_bus.clone() else {
        return;
    };

    // rc.cmd - dispatch one forwarded controller frame into the local
    // Hub under a proxy identity. Reply: {dispatched: bool, code?,
    // message?} - a dispatch error is a NORMAL reply (the caller
    // forwards it to its browser as rc:error); NACK/deadline mean the
    // relay itself failed and the caller falls back to the PR-1 rehome
    // answer.
    {
        let state = state.clone();
        bus.register("rc.cmd", move |body| {
            let state = state.clone();
            Box::pin(async move {
                let conn = body
                    .get("conn")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| "missing conn".to_string())?
                    .to_string();
                let user_id = body
                    .get("user_id")
                    .and_then(|v| v.as_str())
                    .and_then(|s| ObjectId::parse_str(s).ok())
                    .ok_or_else(|| "bad user_id".to_string())?;
                let origin_pod = body
                    .get("origin")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                let controller_name = body
                    .get("controller_name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("remote user")
                    .to_string();
                let consent_mode: ConsentMode = body
                    .get("consent_mode")
                    .cloned()
                    .and_then(|v| serde_json::from_value(v).ok())
                    .unwrap_or_default();
                let override_reason = body
                    .get("override_reason")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let input_mode: Option<InputMode> = body
                    .get("input_mode")
                    .cloned()
                    .and_then(|v| serde_json::from_value(v).ok());
                // Multi-org — the asking org's name rides the relayed frame so
                // a CROSS-POD session request still names it in the host's
                // consent prompt. Resolved on the ORIGIN pod (it ran the authz
                // gate); the owner pod only forwards what it was told.
                let tenant_name = body
                    .get("tenant_name")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let frame: ClientMsg = serde_json::from_value(
                    body.get("frame")
                        .cloned()
                        .ok_or_else(|| "missing frame".to_string())?,
                )
                .map_err(|e| format!("bad frame: {e}"))?;

                // Get-or-create the proxy for this origin conn.
                let tx = {
                    let entry = state.rc_proxy_controllers.entry(conn.clone());
                    match entry {
                        dashmap::mapref::entry::Entry::Occupied(o) => {
                            *o.get().last_seen.lock().unwrap() = Instant::now();
                            o.get().tx.clone()
                        }
                        dashmap::mapref::entry::Entry::Vacant(v) => {
                            let (tx, mut rx) = state.rc_hub.register_controller(user_id);
                            let pump_state = state.clone();
                            let pump_conn = conn.clone();
                            let pump = tokio::spawn(async move {
                                while let Some(msg) = rx.recv().await {
                                    let Ok(val) = serde_json::to_value(&msg) else {
                                        continue;
                                    };
                                    crate::ws::dispatcher::send_to_connection_routed(
                                        &pump_state.ws_storage,
                                        &pump_state.redis_pubsub,
                                        &pump_conn,
                                        &val,
                                    )
                                    .await;
                                }
                            });
                            info!(%user_id, conn = %conn, "rc relay: proxy controller created");
                            v.insert(ProxyController {
                                user_id,
                                tx: tx.clone(),
                                origin_pod,
                                last_seen: std::sync::Mutex::new(Instant::now()),
                                pump,
                            });
                            tx
                        }
                    }
                };

                let ctx = DispatchCtx {
                    role: Role::Controller,
                    user_id: Some(user_id),
                    agent_id: None,
                    controller_name: Some(controller_name),
                    controller_tx: Some(tx),
                    consent_mode,
                    override_reason,
                    input_mode,
                    tenant_name,
                };
                match state.rc_hub.dispatch(&ctx, frame) {
                    Ok(()) => Ok(serde_json::json!({ "dispatched": true })),
                    Err(e) => Ok(serde_json::json!({
                        "dispatched": false,
                        "code": crate::ws::remote_control::error_code(&e),
                        "message": e.to_string(),
                    })),
                }
            })
        });
    }

    // rc.conn_closed - the origin pod's browser socket died; tear the
    // proxy down (unregister terminates the conn's sessions, freeing
    // the agent's slot, exactly like a local tab close).
    {
        let state = state.clone();
        bus.register("rc.conn_closed", move |body| {
            let state = state.clone();
            Box::pin(async move {
                let conn = body
                    .get("conn")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| "missing conn".to_string())?;
                let removed = remove_proxy(&state, conn);
                Ok(serde_json::json!({ "removed": removed }))
            })
        });
    }

    // rc.conn_alive - "does this connection id still have a live
    // socket here?" (sweep probe; answered by the ORIGIN pod).
    {
        let state = state.clone();
        bus.register("rc.conn_alive", move |body| {
            let state = state.clone();
            Box::pin(async move {
                let conn = body
                    .get("conn")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| "missing conn".to_string())?;
                let alive = state.ws_storage.get_sender_by_connection(conn).is_some();
                Ok(serde_json::json!({ "alive": alive }))
            })
        });
    }

    // Janitor sweep: probe QUIET proxies' origin pods; tear down on a
    // negative answer or a dead origin (deadline). A healthy quiet
    // session keeps its proxy as long as the origin conn answers alive.
    {
        let state = state.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(SWEEP_EVERY).await;
                let Some(bus) = state.cluster_bus.clone() else {
                    continue;
                };
                let quiet: Vec<(String, String)> = state
                    .rc_proxy_controllers
                    .iter()
                    .filter(|e| {
                        e.value().last_seen.lock().unwrap().elapsed() > PROBE_WHEN_QUIET_FOR
                    })
                    .map(|e| (e.key().clone(), e.value().origin_pod.clone()))
                    .collect();
                for (conn, origin) in quiet {
                    if origin.is_empty() {
                        continue;
                    }
                    let alive = match bus
                        .request(
                            &origin,
                            "rc.conn_alive",
                            serde_json::json!({ "conn": conn }),
                        )
                        .await
                    {
                        Ok(rep) => rep.get("alive").and_then(|v| v.as_bool()).unwrap_or(false),
                        // Deadline = origin pod presumed dead; its
                        // sockets died with it.
                        Err(_) => false,
                    };
                    if !alive {
                        warn!(conn = %conn, %origin, "rc relay: origin conn gone; reaping proxy");
                        remove_proxy(&state, &conn);
                    } else {
                        // Alive: push last_seen forward so the next
                        // sweep does not re-probe immediately.
                        if let Some(p) = state.rc_proxy_controllers.get(&conn) {
                            *p.last_seen.lock().unwrap() = Instant::now();
                        }
                    }
                }
            }
        });
    }
}

/// Tear down one proxy: Hub unregister (terminates its sessions +
/// notifies both sides) + pump abort. Returns whether it existed.
pub fn remove_proxy(state: &AppState, conn: &str) -> bool {
    let Some((_, proxy)) = state.rc_proxy_controllers.remove(conn) else {
        return false;
    };
    state.rc_hub.unregister_controller(proxy.user_id, &proxy.tx);
    proxy.pump.abort();
    debug!(conn = %conn, "rc relay: proxy controller removed");
    true
}

/// Controller-pod side: forward one raw rc frame to `owner_pod`.
/// Returns:
/// - `Ok(None)`: dispatched remotely - suppress local error handling
///   (replies stream back conn-addressed).
/// - `Ok(Some((code, message)))`: the owner's Hub refused - surface it
///   locally as `rc:error` (same as a local dispatch failure).
/// - `Err(())`: the relay itself failed (no bus / deadline / NACK) -
///   fall back to the PR-1 rehome path.
#[allow(clippy::too_many_arguments)]
pub async fn relay_rc_frame(
    state: &AppState,
    owner_pod: &str,
    conn: &str,
    user_id: ObjectId,
    controller_name: &str,
    consent_mode: ConsentMode,
    override_reason: &Option<String>,
    input_mode: Option<InputMode>,
    tenant_name: &Option<String>,
    raw_frame: &serde_json::Value,
) -> Result<Option<(String, String)>, ()> {
    let Some(bus) = state.cluster_bus.clone() else {
        return Err(());
    };
    if owner_pod.is_empty() {
        return Err(());
    }
    let body = serde_json::json!({
        "conn": conn,
        "origin": state.pod.pod_id,
        "user_id": user_id.to_hex(),
        "controller_name": controller_name,
        "consent_mode": consent_mode,
        "override_reason": override_reason,
        "input_mode": input_mode,
        "tenant_name": tenant_name,
        "frame": raw_frame,
    });
    match bus.request(owner_pod, "rc.cmd", body).await {
        Ok(rep) => {
            if rep
                .get("dispatched")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                // Remember the remote home so (a) later session-scoped
                // frames route there and (b) the WS close forwards the
                // teardown notice.
                state
                    .remote_rc_conns
                    .entry(conn.to_string())
                    .or_default()
                    .insert(owner_pod.to_string());
                crate::cluster::metrics::bump(&crate::cluster::metrics::RC_RELAY_TOTAL);
                Ok(None)
            } else {
                let code = rep
                    .get("code")
                    .and_then(|v| v.as_str())
                    .unwrap_or("relay_error")
                    .to_string();
                let message = rep
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("remote dispatch failed")
                    .to_string();
                Ok(Some((code, message)))
            }
        }
        Err(e) => {
            debug!(%owner_pod, %e, "rc relay: rc.cmd failed; falling back to rehome");
            // The owner may have moved meanwhile - forget this route.
            if let Some(mut set) = state.remote_rc_conns.get_mut(conn) {
                set.remove(owner_pod);
            }
            state.remote_rc_conns.remove_if(conn, |_, s| s.is_empty());
            Err(())
        }
    }
}

/// Controller-pod side: the browser socket closed - notify every owner
/// pod that hosted proxied sessions for this conn (fire-and-forget;
/// the janitor sweep is the belt if the notice is lost).
pub fn forward_conn_closed(state: &AppState, conn: &str) {
    let Some((_, owners)) = state.remote_rc_conns.remove(conn) else {
        return;
    };
    let Some(bus) = state.cluster_bus.clone() else {
        return;
    };
    for owner in owners {
        let bus = bus.clone();
        let conn = conn.to_string();
        tokio::spawn(async move {
            let _ = bus
                .request(
                    &owner,
                    "rc.conn_closed",
                    serde_json::json!({ "conn": conn }),
                )
                .await;
        });
    }
}

/// conn id -> owner pods hosting proxied rc sessions for it.
pub type RemoteRcConns = DashMap<String, HashSet<String>>;
