// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
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

use bson::oid::ObjectId;
use dashmap::DashMap;
use roomler_ai_mod_fleet::hub::{DispatchCtx, OrgPolicy};
use roomler_ai_remote_control::models::{ConsentMode, InputMode, RcOverlayMode, SessionPolicy};
use roomler_ai_remote_control::signaling::{ClientMsg, Role, ServerMsg};
use roomler_core::cluster::bus::BusError;
use tracing::{debug, info, warn};

use crate::RemoteState;

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
/// from the module's init (needs the full state), the way the host wires
/// `wire_media_cluster` / `wire_derp_cluster`.
pub fn wire_rc_relay(state: &RemoteState) {
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
                // FR-85 P3 — resolved by the ORIGIN pod's authz gate. ⚠️ An
                // absent field (a pod still on an older build) reads as
                // FALSE: the hub then strips RECORD, which fails closed.
                let may_record = body
                    .get("may_record")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                // #1882 — the device's `AccessPolicy.rc_overlay`, resolved by
                // the same ORIGIN-pod gate. Absent (an older pod) reads OFF,
                // the policy's own default. FR-128 — and `auto`, with its
                // far-TURN hint.
                let rc_overlay = rc_overlay_from_relay(&body);
                // FR-101 P2 — the org's session policy, read by the ORIGIN
                // pod's gate. See `session_policy_from_relay` for absent.
                let session_policy = session_policy_from_relay(body.get("session_policy"));
                // FR-101 P4b — the org's maximum session length, in seconds.
                // Absent (an older origin pod) is no limit, what that pod
                // would have applied itself.
                let max_duration = body
                    .get("max_secs")
                    .and_then(|v| v.as_u64())
                    .map(std::time::Duration::from_secs);
                // P4b-2 — the idle limit, in seconds, resolved by the same
                // gate. Absent (an older origin pod) is no idle limit, what
                // that pod would have applied itself.
                let idle_secs = body
                    .get("idle_secs")
                    .and_then(|v| v.as_u64())
                    .and_then(|s| u32::try_from(s).ok());
                // FR-99 P3b — the device's lock ask, resolved by the same
                // gate. Absent or null (an older origin pod, or nothing
                // asked) is no ask, what that pod would have applied itself.
                let lock_on_end = lock_ask_from_relay(body.get("lock_on_end"));
                // FR-115 P3 — the device org's session cap, resolved by the
                // same gate. Absent (an older origin pod) or null is no cap,
                // what that pod would have applied itself.
                let org_session_cap = body
                    .get("org_session_cap")
                    .and_then(|v| v.as_u64())
                    .and_then(|n| u32::try_from(n).ok());
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
                            let (tx, mut rx) = state.fleet.rc_hub.register_controller(user_id);
                            let pump_state = state.clone();
                            let pump_conn = conn.clone();
                            let pump = tokio::spawn(async move {
                                while let Some(msg) = rx.recv().await {
                                    let Ok(val) = serde_json::to_value(&msg) else {
                                        continue;
                                    };
                                    roomler_core::ws::dispatcher::send_to_connection_routed(
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

                // FR-52 P3d — an external-access login the origin pod forwarded
                // because the device is homed HERE. Handled by the extauth relay,
                // not the Hub, and answered through this proxy sender — whose
                // pump already routes every reply back to the browser's socket.
                if let Some(handled) =
                    crate::extauth::on_relayed(&state, user_id, &controller_name, &tx, &frame)
                {
                    return Ok(serde_json::json!({ "dispatched": handled }));
                }

                // FR-138 P1b-2 — this pod holds the device, so the session a
                // relayed redial `replaces`, and what is held for this person
                // on the device, end here before the org's slot is claimed
                // below and before the hub's own capacity gate, as on the
                // local path. Inert at window 0.
                if let ClientMsg::SessionRequest {
                    agent_id, replaces, ..
                } = &frame
                {
                    crate::row_end::before_request(&state, user_id, *agent_id, *replaces).await;
                }
                // FR-115 P3b — this pod owns the device, so the org's slot is
                // claimed here, exactly as the local path claims it.
                let mut org_session_cap = org_session_cap;
                let mut slot = None;
                if let ClientMsg::SessionRequest { agent_id, .. } = &frame
                    && let Some(cap) = org_session_cap
                    && crate::slots::enabled(&state)
                    && let Some(org) = state.fleet.rc_hub.agent_org(*agent_id)
                {
                    use crate::slots::SlotDecision;
                    use roomler_core::cluster::session_slots::SlotScope;
                    match crate::slots::claim(&state, SlotScope::Org(org), user_id, *agent_id, cap)
                        .await
                    {
                        SlotDecision::Claimed(lease) => {
                            slot = Some(lease);
                            org_session_cap = None;
                        }
                        SlotDecision::Fallback => {}
                        SlotDecision::Full {
                            cap,
                            holder_user,
                            holder_agent,
                        } => {
                            let e = roomler_ai_remote_control::Error::OrgSessionLimit {
                                cap,
                                holder_user,
                                holder_agent,
                            };
                            return Ok(serde_json::json!({
                                "dispatched": false,
                                "code": crate::controller::error_code(&e),
                                "message": crate::controller::refusal_message(&state, &e).await,
                            }));
                        }
                    }
                }
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
                    may_record,
                    rc_overlay,
                    session_policy,
                    max_duration,
                    idle_secs,
                    lock_on_end,
                    org_session_cap,
                    slot: slot.clone(),
                };
                let dispatched = state.fleet.rc_hub.dispatch(&ctx, frame);
                if let Some(lease) = &slot {
                    let created = state
                        .fleet
                        .rc_hub
                        .session_snapshot(lease.session)
                        .map(|_| lease.session);
                    crate::slots::release_unused(&state, lease, created).await;
                }
                match dispatched {
                    Ok(()) => Ok(serde_json::json!({ "dispatched": true })),
                    Err(e) => Ok(serde_json::json!({
                        "dispatched": false,
                        "code": crate::controller::error_code(&e),
                        // FR-115 P3 — the same words the local path sends
                        // (who holds the slot, on which device).
                        "message": crate::controller::refusal_message(&state, &e).await,
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
                // #3840 — the decision is `probe_verdict`, the walk `sweep`.
                let unsettled = sweep(
                    quiet,
                    |conn, origin| {
                        let bus = bus.clone();
                        async move {
                            let body = serde_json::json!({ "conn": conn });
                            probe_verdict(&bus.request(&origin, "rc.conn_alive", body).await)
                        }
                    },
                    |conn, origin, verdict| {
                        if verdict == ProbeVerdict::Gone {
                            warn!(conn = %conn, %origin, "rc relay: origin conn gone; reaping proxy");
                            remove_proxy(&state, conn);
                        } else if let Some(p) = state.rc_proxy_controllers.get(conn) {
                            // Alive: push last_seen forward so the next
                            // sweep does not re-probe immediately.
                            *p.last_seen.lock().unwrap() = Instant::now();
                        }
                    },
                )
                .await;
                if unsettled > 0 {
                    warn!(unsettled, "{SWEEP_STOPPED_LINE}");
                }
            }
        });
    }
}

/// Tear down one proxy: Hub unregister (terminates its sessions +
/// notifies both sides) + pump abort. Returns whether it existed.
pub fn remove_proxy(state: &RemoteState, conn: &str) -> bool {
    let Some((_, proxy)) = state.rc_proxy_controllers.remove(conn) else {
        return false;
    };
    state
        .fleet
        .rc_hub
        .unregister_controller(proxy.user_id, &proxy.tx);
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
/// - `Err(failed)`: the relay itself failed (no bus / deadline / NACK /
///   Redis) - fall back to the PR-1 rehome path. [`RelayFailed`] says
///   whether the owner pod is gone (#3840).
#[allow(clippy::too_many_arguments)]
pub async fn relay_rc_frame(
    state: &RemoteState,
    owner_pod: &str,
    conn: &str,
    user_id: ObjectId,
    controller_name: &str,
    consent_mode: ConsentMode,
    override_reason: &Option<String>,
    input_mode: Option<InputMode>,
    tenant_name: &Option<String>,
    // FR-85 P3 — the origin pod ran the authz gate; the owner pod's hub
    // strips RECORD without it.
    may_record: bool,
    // #1882 — resolved by the same gate; the owner pod reads absent as off.
    // FR-128 — `auto` and its hint cross beside the bool.
    rc_overlay: RcOverlayMode,
    // FR-101 P2 — the org's session policy, read by the same gate. ⚠️ The
    // owner pod's hub applies it; without this a cross-pod session would
    // skip the org's switches.
    session_policy: OrgPolicy,
    // FR-101 P4b — the org's maximum session length; the owner pod's hub
    // arms it when the session goes live.
    max_duration: Option<std::time::Duration>,
    // P4b-2 — the idle limit; the owner pod's hub sends it to the agent.
    idle_secs: Option<u32>,
    // FR-99 P3b — the device's lock ask; the owner pod's hub sends it to an
    // agent that honours it.
    lock_on_end: Option<roomler_ai_remote_control::models::LockOnEnd>,
    // FR-115 P3 — the device org's concurrent-session cap; the owner pod's
    // hub counts the org's live sessions against it. ⚠️ Without it a
    // session landing on the other pod would skip the cap.
    org_session_cap: Option<u32>,
    raw_frame: &serde_json::Value,
) -> Result<Option<(String, String)>, RelayFailed> {
    let Some(bus) = state.cluster_bus.clone() else {
        return Err(RelayFailed::NotSent);
    };
    if owner_pod.is_empty() {
        return Err(RelayFailed::NotSent);
    }
    let (rc_overlay_on, rc_overlay_auto, rc_overlay_far_turn) = rc_overlay_to_relay(rc_overlay);
    let body = serde_json::json!({
        "conn": conn,
        "origin": state.pod.pod_id,
        "user_id": user_id.to_hex(),
        "controller_name": controller_name,
        "consent_mode": consent_mode,
        "override_reason": override_reason,
        "input_mode": input_mode,
        "tenant_name": tenant_name,
        "may_record": may_record,
        "rc_overlay": rc_overlay_on,
        // FR-128 — beside the bool, so an owner pod older than them reads
        // only the bool, and `auto` reaches it as `off`.
        "rc_overlay_auto": rc_overlay_auto,
        "rc_overlay_far_turn": rc_overlay_far_turn,
        "session_policy": session_policy_to_relay(session_policy),
        "max_secs": max_duration.map(|d| d.as_secs()),
        "idle_secs": idle_secs,
        "lock_on_end": lock_on_end,
        "org_session_cap": org_session_cap,
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
                roomler_core::cluster::metrics::bump(
                    &roomler_core::cluster::metrics::RC_RELAY_TOTAL,
                );
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
            let failed = settle_failed_relay(&state.remote_rc_conns, conn, owner_pod, &e);
            debug!(%owner_pod, %e, ?failed, "rc relay: rc.cmd failed; falling back to rehome");
            Err(failed)
        }
    }
}

/// Controller-pod side: the browser socket closed - notify every owner
/// pod that hosted proxied sessions for this conn (fire-and-forget;
/// the janitor sweep is the belt if the notice is lost).
pub fn forward_conn_closed(state: &RemoteState, conn: &str) {
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

/// FR-101 P2 — the gate's [`OrgPolicy`] in the relay body: the policy as an
/// object, or `"unavailable"` when the origin pod could not read it.
fn session_policy_to_relay(policy: OrgPolicy) -> serde_json::Value {
    match policy {
        OrgPolicy::Read(p) => serde_json::to_value(p).unwrap_or(serde_json::Value::Null),
        OrgPolicy::Unavailable => serde_json::Value::from("unavailable"),
    }
}

/// FR-101 P2 — the owner pod's reading of the relay body's `session_policy`.
///
/// ⚠️ ABSENT reads as unrestricted, unlike `may_record`'s absent-is-false:
/// it comes from an origin pod older than the feature, which applied no
/// policy and would have granted these features itself. Mid-roll, a pair of
/// pods on different builds cannot promise the policy in either direction
/// (an older OWNER pod ignores the field just as surely), and failing closed
/// here would take clipboard and files from every cross-pod session during
/// each roll, of orgs that set nothing. Anything present but unreadable is
/// our own pod's garbage and fails closed.
fn session_policy_from_relay(v: Option<&serde_json::Value>) -> OrgPolicy {
    match v {
        None | Some(serde_json::Value::Null) => OrgPolicy::Read(SessionPolicy::default()),
        Some(v @ serde_json::Value::Object(_)) => serde_json::from_value(v.clone())
            .map(OrgPolicy::Read)
            .unwrap_or(OrgPolicy::Unavailable),
        Some(_) => OrgPolicy::Unavailable,
    }
}

/// #1882 / FR-128 — the relay body's three fields for the device's mode:
/// `rc_overlay` (only `on` says yes), `rc_overlay_auto`, and the far-TURN
/// hint (`None` = not measured, and for every mode but `auto`).
fn rc_overlay_to_relay(mode: RcOverlayMode) -> (bool, bool, Option<bool>) {
    match mode {
        RcOverlayMode::Off => (false, false, None),
        RcOverlayMode::Auto { far_turn } => (false, true, far_turn),
        RcOverlayMode::On => (true, false, None),
    }
}

/// #1882 / FR-128 — the device's mode as the ORIGIN pod's gate resolved it.
/// Every field absent (an older origin pod) reads `off`, the policy's own
/// default. `rc_overlay` wins, as it does on the policy row. `auto` rides
/// beside a `false` bool, so an origin pod that sends only the bool never
/// sends it; its hint is `None` (not measured) when absent or null.
fn rc_overlay_from_relay(body: &serde_json::Value) -> RcOverlayMode {
    let flag = |k: &str| body.get(k).and_then(|v| v.as_bool());
    if flag("rc_overlay").unwrap_or(false) {
        RcOverlayMode::On
    } else if flag("rc_overlay_auto").unwrap_or(false) {
        RcOverlayMode::Auto {
            far_turn: flag("rc_overlay_far_turn"),
        }
    } else {
        RcOverlayMode::Off
    }
}

/// FR-99 P3b — the device's lock ask on the owner pod's side of the relay.
/// Absent or null is no ask: an origin pod older than the field asked
/// nothing of the device itself. `never` asks nothing either. A value this
/// build cannot read reads as `always`, the stricter side, as
/// `LockOnEnd` reads one everywhere.
fn lock_ask_from_relay(
    v: Option<&serde_json::Value>,
) -> Option<roomler_ai_remote_control::models::LockOnEnd> {
    use roomler_ai_remote_control::models::LockOnEnd;
    match v {
        None | Some(serde_json::Value::Null) => None,
        Some(v) => serde_json::from_value::<LockOnEnd>(v.clone())
            .ok()
            .filter(|l| *l > LockOnEnd::Never),
    }
}

// ── #3840 — what a failed relay says about the other pod ────────────────────
//
// A bus request fails three ways. A deadline (no answer from the other pod in
// time) and a NACK (it answered that it cannot take the request) are answers
// about that pod. A Redis error (the publish failed, or the shared
// connection's response timeout fired) is not: Redis paused or restarted, and
// the other pod may be fine. Before #3840 a Redis that stopped answering held
// the publish instead of failing it, so this error was rare; with the bound
// it comes with every Redis restart and every pause longer than the response
// timeout. So what tears something down acts on the first two only.

/// The one wording of a session frame dropped because Redis failed its
/// relay, with the frame type and a short session id. At most one line a
/// second per pod, with how many were held back.
pub const FRAME_DROPPED_LINE: &str = "rc relay: frame dropped, Redis failed its relay";

/// The one wording of a janitor sweep that a Redis failure stopped: the
/// proxies it could not settle are kept, and asked about at the next sweep.
pub const SWEEP_STOPPED_LINE: &str = "rc relay: sweep stopped, Redis failed a probe";

/// Why the relay of an rc frame to its owner pod failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayFailed {
    /// Nothing was sent: this pod has no bus, or no owner pod was named.
    NotSent,
    /// The owner pod gave no answer within the deadline, or NACKed the frame:
    /// it is gone or cannot take it, so the route to it is forgotten.
    OwnerGone,
    /// Redis failed the publish. That says nothing about the owner pod, so
    /// the route to it is kept.
    Unknown,
}

/// What a failed bus request says about the pod it was sent to.
pub fn relay_failed(e: &BusError) -> RelayFailed {
    // Before #3840: every failure read as the owner gone.
    let _ = e;
    RelayFailed::OwnerGone
}

/// The route bookkeeping after the relay of `conn`'s frame to `owner_pod`
/// failed with `e`: the route is forgotten only when the owner pod is gone.
fn settle_failed_relay(
    conns: &RemoteRcConns,
    conn: &str,
    owner_pod: &str,
    e: &BusError,
) -> RelayFailed {
    let failed = relay_failed(e);
    if failed == RelayFailed::OwnerGone {
        // The owner may have moved meanwhile - forget this route.
        if let Some(mut set) = conns.get_mut(conn) {
            set.remove(owner_pod);
        }
        conns.remove_if(conn, |_, s| s.is_empty());
    }
    failed
}

/// What one `rc.conn_alive` probe says about a quiet proxy's origin pod.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeVerdict {
    /// The origin pod still holds the browser socket: keep the proxy.
    Alive,
    /// The origin pod answered that it does not hold the socket, or NACKed
    /// the probe (it runs no relay, so it holds no relayed socket, and every
    /// later probe would NACK too), or gave no answer within the deadline
    /// (presumed dead, its sockets with it): reap the proxy.
    Gone,
    /// Redis failed the probe: keep the proxy and ask again at the next
    /// sweep.
    Unknown,
}

/// The verdict of one probe's answer.
pub fn probe_verdict(answer: &Result<serde_json::Value, BusError>) -> ProbeVerdict {
    // Before #3840: anything but a live answer reaped the proxy.
    match answer {
        Ok(rep) if rep.get("alive").and_then(|v| v.as_bool()) == Some(true) => ProbeVerdict::Alive,
        _ => ProbeVerdict::Gone,
    }
}

/// One janitor sweep over the quiet proxies, `(conn, origin pod)`: probe
/// each origin in turn and hand `act` every verdict that settles one (reap
/// it, or keep it and move its clock). The first probe Redis fails stops the
/// sweep: it settles nothing, and Redis is failing for the probes after it
/// too, each of which would wait out the response timeout. Returns how many
/// proxies the sweep left as they were, that one and every one after it; the
/// next sweep asks about them again, since their clocks did not move.
pub async fn sweep<P, F>(
    quiet: Vec<(String, String)>,
    mut probe: P,
    mut act: impl FnMut(&str, &str, ProbeVerdict),
) -> usize
where
    P: FnMut(String, String) -> F,
    F: std::future::Future<Output = ProbeVerdict>,
{
    // Before #3840: every quiet proxy was probed, and anything but a live
    // answer reaped it.
    for (conn, origin) in quiet {
        if origin.is_empty() {
            continue;
        }
        let verdict = match probe(conn.clone(), origin.clone()).await {
            ProbeVerdict::Alive => ProbeVerdict::Alive,
            _ => ProbeVerdict::Gone,
        };
        act(&conn, &origin, verdict);
    }
    0
}

/// The relay of one session frame (`rc:sdp.offer`, `rc:ice`) to every owner
/// pod its socket's route names, when none of them dispatched it.
#[derive(Debug, Default)]
pub struct Scoped {
    refusal: Option<(String, String)>,
    lost_to_redis: bool,
}

impl Scoped {
    /// An owner pod's hub refused the frame, in these words.
    pub fn refused(&mut self, said: (String, String)) {
        self.refusal = Some(said);
    }

    /// The relay to one owner pod failed.
    pub fn failed(&mut self, failed: RelayFailed) {
        // Before #3840: a failed relay fell through to the local answer.
        let _ = failed;
    }

    /// The `rc:error` for an owner's refusal: its words, as a local dispatch
    /// failure reads.
    pub fn refusal(&mut self, session_id: Option<ObjectId>) -> Option<ServerMsg> {
        self.refusal.take().map(|(code, message)| ServerMsg::Error {
            session_id,
            code,
            message,
            open_nonce: None,
        })
    }

    /// No owner refused the frame, and Redis failed its relay to one (q2 =
    /// A): the frame is dropped, as one lost in flight. The controller is told
    /// nothing (its own timers recover a setup) and the route is kept.
    pub fn dropped(&self, frame: &serde_json::Value) -> bool {
        // Before #3840: never.
        let _ = (frame, self.lost_to_redis);
        false
    }
}

/// The frame type and a short session id of a relayed frame, for its log
/// line: `t`, and the last 6 characters of `session_id`.
pub fn frame_tag(frame: &serde_json::Value) -> (&str, &str) {
    let kind = frame.get("t").and_then(|v| v.as_str()).unwrap_or("?");
    let sid = frame
        .get("session_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    (kind, sid.get(sid.len().saturating_sub(6)..).unwrap_or(""))
}

#[cfg(test)]
mod lock_ask_relay_tests {
    use super::*;
    use roomler_ai_remote_control::models::LockOnEnd;

    /// The ask crosses the relay as the origin's gate resolved it; an older
    /// origin pod's body (no field) asks nothing.
    #[test]
    fn the_lock_ask_crosses_the_relay_as_asked() {
        assert_eq!(lock_ask_from_relay(None), None);
        assert_eq!(lock_ask_from_relay(Some(&serde_json::Value::Null)), None);
        assert_eq!(lock_ask_from_relay(Some(&serde_json::json!("never"))), None);
        assert_eq!(
            lock_ask_from_relay(Some(&serde_json::json!("auto"))),
            Some(LockOnEnd::Auto)
        );
        assert_eq!(
            lock_ask_from_relay(Some(&serde_json::json!("always"))),
            Some(LockOnEnd::Always)
        );
        assert_eq!(
            lock_ask_from_relay(Some(&serde_json::json!("on_tuesdays"))),
            Some(LockOnEnd::Always)
        );
        // What `relay_rc_frame` writes reads back the same.
        let body = serde_json::json!({ "lock_on_end": Some(LockOnEnd::Auto) });
        assert_eq!(
            lock_ask_from_relay(body.get("lock_on_end")),
            Some(LockOnEnd::Auto)
        );
    }
}

#[cfg(test)]
mod rc_overlay_relay_tests {
    use super::*;

    /// The body `relay_rc_frame` writes for a mode, the fields alone.
    fn written(mode: RcOverlayMode) -> serde_json::Value {
        let (on, auto, far_turn) = rc_overlay_to_relay(mode);
        serde_json::json!({
            "rc_overlay": on,
            "rc_overlay_auto": auto,
            "rc_overlay_far_turn": far_turn,
        })
    }

    /// FR-128 — every mode, and every hint, crosses as the gate resolved it.
    #[test]
    fn every_mode_crosses_the_relay_as_resolved() {
        for mode in [
            RcOverlayMode::Off,
            RcOverlayMode::On,
            RcOverlayMode::Auto { far_turn: None },
            RcOverlayMode::Auto {
                far_turn: Some(true),
            },
            RcOverlayMode::Auto {
                far_turn: Some(false),
            },
        ] {
            assert_eq!(rc_overlay_from_relay(&written(mode)), mode, "{mode:?}");
        }
    }

    /// An older origin pod sends the bool alone, or nothing: `off`, or `on`
    /// as it said. And `auto` reaches an older OWNER pod as the bool it
    /// reads, which is `false`.
    #[test]
    fn an_older_pod_reads_and_sends_only_the_bool() {
        assert_eq!(
            rc_overlay_from_relay(&serde_json::json!({ "may_record": false })),
            RcOverlayMode::Off
        );
        assert_eq!(
            rc_overlay_from_relay(&serde_json::json!({ "rc_overlay": true })),
            RcOverlayMode::On
        );
        let auto = written(RcOverlayMode::Auto {
            far_turn: Some(true),
        });
        assert_eq!(auto["rc_overlay"], serde_json::json!(false));
    }
}

#[cfg(test)]
mod session_policy_relay_tests {
    use super::*;
    use roomler_ai_remote_control::models::FeaturePolicy;

    /// The origin pod's reading reaches the owner pod unchanged, both ways.
    #[test]
    fn the_policy_crosses_the_relay_as_read() {
        let p = SessionPolicy {
            clipboard: FeaturePolicy::Ask,
            files: FeaturePolicy::Off,
            audio: FeaturePolicy::On,
        };
        for policy in [
            OrgPolicy::Read(p),
            OrgPolicy::Read(SessionPolicy::default()),
            OrgPolicy::Unavailable,
        ] {
            let body = serde_json::json!({ "session_policy": session_policy_to_relay(policy) });
            assert_eq!(
                session_policy_from_relay(body.get("session_policy")),
                policy
            );
        }
    }

    /// An older origin pod sends no field: what it would have done itself.
    #[test]
    fn an_absent_policy_from_an_older_pod_is_unrestricted() {
        let body = serde_json::json!({ "may_record": false });
        assert_eq!(
            session_policy_from_relay(body.get("session_policy")),
            OrgPolicy::Read(SessionPolicy::default())
        );
    }

    /// Present but unreadable fails closed: a number, or a string other than
    /// `unavailable`. Inside an object, a value of the wrong type refuses that
    /// one feature, as `FeaturePolicy` reads it everywhere.
    #[test]
    fn a_present_but_unreadable_policy_refuses() {
        for bad in [serde_json::json!(7), serde_json::json!("on")] {
            assert_eq!(
                session_policy_from_relay(Some(&bad)),
                OrgPolicy::Unavailable,
                "{bad}"
            );
        }
        assert_eq!(
            session_policy_from_relay(Some(&serde_json::json!({ "files": 3 }))),
            OrgPolicy::Read(SessionPolicy {
                files: FeaturePolicy::Off,
                ..SessionPolicy::default()
            })
        );
    }
}

/// #3840 — the relay's reading of a failed bus request. A Redis error says
/// nothing about the other pod; a deadline or a NACK does.
#[cfg(test)]
mod relay_failure_tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use super::*;

    fn redis_error() -> BusError {
        BusError::Redis(redis::RedisError::from(std::io::Error::from(
            std::io::ErrorKind::TimedOut,
        )))
    }

    fn deadline() -> BusError {
        BusError::Deadline(roomler_core::cluster::bus::RPC_DEADLINE)
    }

    fn nack() -> BusError {
        BusError::Nack("no handler for class rc.cmd".to_string())
    }

    fn routes(conns: &RemoteRcConns, conn: &str) -> Vec<String> {
        let mut owners: Vec<String> = conns
            .get(conn)
            .map(|s| s.iter().cloned().collect())
            .unwrap_or_default();
        owners.sort();
        owners
    }

    #[test]
    fn only_a_deadline_or_a_nack_says_the_owner_pod_is_gone() {
        assert_eq!(relay_failed(&redis_error()), RelayFailed::Unknown);
        assert_eq!(relay_failed(&deadline()), RelayFailed::OwnerGone);
        assert_eq!(relay_failed(&nack()), RelayFailed::OwnerGone);
    }

    /// A relay Redis failed keeps the route to the owner pod, so the next
    /// frame goes there again; one the owner pod did not answer, or NACKed,
    /// forgets that owner and only that one.
    #[test]
    fn a_relay_redis_failed_keeps_the_route_and_a_gone_owner_is_forgotten() {
        for (e, kept) in [(redis_error(), true), (deadline(), false), (nack(), false)] {
            let conns = RemoteRcConns::new();
            conns
                .entry("c1".to_string())
                .or_default()
                .insert("pod-b".to_string());
            let failed = settle_failed_relay(&conns, "c1", "pod-b", &e);
            assert_eq!(
                failed == RelayFailed::Unknown,
                kept,
                "{e}: read as {failed:?}"
            );
            let expect: Vec<String> = if kept {
                vec!["pod-b".to_string()]
            } else {
                Vec::new()
            };
            assert_eq!(routes(&conns, "c1"), expect, "{e}");
            assert_eq!(
                conns.contains_key("c1"),
                kept,
                "{e}: an empty route is removed"
            );
        }
        // The socket's other owner keeps its route when one owner is gone.
        let conns = RemoteRcConns::new();
        for pod in ["pod-b", "pod-c"] {
            conns
                .entry("c1".to_string())
                .or_default()
                .insert(pod.to_string());
        }
        settle_failed_relay(&conns, "c1", "pod-b", &deadline());
        assert_eq!(routes(&conns, "c1"), vec!["pod-c".to_string()]);
    }

    /// The janitor's probe: a live answer keeps the proxy; a negative answer,
    /// an answer that says nothing, a NACK or a deadline reaps it; a probe
    /// Redis failed settles nothing.
    #[test]
    fn a_probe_redis_failed_is_unknown_and_every_answer_is_a_verdict() {
        use serde_json::json;
        assert_eq!(
            probe_verdict(&Ok(json!({ "alive": true }))),
            ProbeVerdict::Alive
        );
        assert_eq!(
            probe_verdict(&Ok(json!({ "alive": false }))),
            ProbeVerdict::Gone
        );
        assert_eq!(probe_verdict(&Ok(json!({}))), ProbeVerdict::Gone);
        assert_eq!(probe_verdict(&Err(deadline())), ProbeVerdict::Gone);
        assert_eq!(probe_verdict(&Err(nack())), ProbeVerdict::Gone);
        assert_eq!(probe_verdict(&Err(redis_error())), ProbeVerdict::Unknown);
    }

    fn quiet(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(c, o)| (c.to_string(), o.to_string()))
            .collect()
    }

    /// Runs `sweep` with scripted verdicts: which conns it probed, what it
    /// handed `act`, and how many it left unsettled.
    async fn run_sweep(
        quiet: Vec<(String, String)>,
        verdicts: &[ProbeVerdict],
    ) -> (Vec<String>, Vec<(String, ProbeVerdict)>, usize) {
        let script = Mutex::new(verdicts.iter().copied().collect::<VecDeque<_>>());
        let probed = Mutex::new(Vec::new());
        let mut acted = Vec::new();
        let unsettled = sweep(
            quiet,
            |conn, _origin| {
                probed.lock().unwrap().push(conn);
                let verdict = script
                    .lock()
                    .unwrap()
                    .pop_front()
                    .expect("a scripted verdict for every probe");
                async move { verdict }
            },
            |conn, _origin, verdict| acted.push((conn.to_string(), verdict)),
        )
        .await;
        (probed.into_inner().unwrap(), acted, unsettled)
    }

    /// A sweep reaps no proxy and moves no clock on a probe Redis failed, and
    /// stops there: the probes after it would wait out the bound on a failing
    /// Redis. Those proxies are left for the next sweep.
    #[tokio::test]
    async fn a_sweep_stops_at_a_probe_redis_failed_and_keeps_every_proxy_left() {
        let (probed, acted, unsettled) = run_sweep(
            quiet(&[("c1", "pod-a"), ("c2", "pod-b"), ("c3", "pod-c")]),
            &[
                ProbeVerdict::Alive,
                ProbeVerdict::Unknown,
                ProbeVerdict::Gone,
            ],
        )
        .await;
        assert_eq!(
            probed,
            ["c1", "c2"],
            "the sweep stops at the probe Redis failed"
        );
        assert_eq!(
            acted,
            [("c1".to_string(), ProbeVerdict::Alive)],
            "c2 and c3 are neither reaped nor refreshed"
        );
        assert_eq!(unsettled, 2, "c2 and c3 are left for the next sweep");
    }

    /// Control: with no Redis failure a sweep settles every quiet proxy, and
    /// skips one with no origin pod, as before.
    #[tokio::test]
    async fn a_sweep_without_a_redis_failure_settles_every_proxy() {
        let (probed, acted, unsettled) = run_sweep(
            quiet(&[
                ("c1", "pod-a"),
                ("c2", ""),
                ("c3", "pod-c"),
                ("c4", "pod-d"),
            ]),
            &[ProbeVerdict::Gone, ProbeVerdict::Alive, ProbeVerdict::Gone],
        )
        .await;
        assert_eq!(probed, ["c1", "c3", "c4"]);
        assert_eq!(
            acted,
            [
                ("c1".to_string(), ProbeVerdict::Gone),
                ("c3".to_string(), ProbeVerdict::Alive),
                ("c4".to_string(), ProbeVerdict::Gone),
            ]
        );
        assert_eq!(unsettled, 0);
    }

    const SID: &str = "6738f0a1b2c3d4e5f6a7b8c9";

    /// The two session frames the relay carries for a controller. Its
    /// `rc:terminate` is never relayed: the hub of the controller's pod
    /// answers one for a session it does not hold with Ok (`Hub::dispatch`).
    fn relayed_frames() -> [serde_json::Value; 2] {
        [
            serde_json::json!({ "t": "rc:sdp.offer", "session_id": SID, "sdp": "v=0" }),
            serde_json::json!({ "t": "rc:ice", "session_id": SID, "candidate": {} }),
        ]
    }

    /// q2 = A — Redis lost the frame on its way to the owner pod, and no
    /// owner refused it: the frame is dropped and the controller is told
    /// nothing.
    #[test]
    fn a_frame_redis_lost_is_dropped_and_the_controller_is_told_nothing() {
        for frame in relayed_frames() {
            let mut said = Scoped::default();
            said.failed(RelayFailed::Unknown);
            assert!(said.refusal(None).is_none(), "{frame}: no refusal");
            assert!(said.dropped(&frame), "{frame}: dropped, nothing told");
        }
        // One owner gone and another lost to Redis: still dropped.
        let mut said = Scoped::default();
        said.failed(RelayFailed::OwnerGone);
        said.failed(RelayFailed::Unknown);
        assert!(said.refusal(None).is_none());
        assert!(said.dropped(&relayed_frames()[1]));
    }

    /// A deadline or a NACK from every owner keeps today's answer: the frame
    /// is not dropped, and the caller answers `session_not_found` as before.
    /// So does a relay that sent nothing.
    #[test]
    fn a_frame_whose_owner_is_gone_falls_through_to_session_not_found() {
        for failed in [RelayFailed::OwnerGone, RelayFailed::NotSent] {
            for frame in relayed_frames() {
                let mut said = Scoped::default();
                said.failed(failed);
                assert!(said.refusal(None).is_none());
                assert!(!said.dropped(&frame), "{failed:?} {frame}: falls through");
            }
        }
        assert!(
            !Scoped::default().dropped(&relayed_frames()[0]),
            "no owner tried: falls through"
        );
    }

    /// An owner's refusal is told in its words, whatever the relay to another
    /// owner did.
    #[test]
    fn an_owners_refusal_is_told_whatever_another_relay_did() {
        let sid = ObjectId::parse_str(SID).unwrap();
        let mut said = Scoped::default();
        said.failed(RelayFailed::Unknown);
        said.refused(("agent_busy".to_string(), "busy".to_string()));
        match said.refusal(Some(sid)) {
            Some(ServerMsg::Error {
                session_id,
                code,
                message,
                open_nonce,
            }) => {
                assert_eq!(session_id, Some(sid));
                assert_eq!((code.as_str(), message.as_str()), ("agent_busy", "busy"));
                assert_eq!(open_nonce, None);
            }
            other => panic!("expected the owner's refusal, got {other:?}"),
        }
    }

    /// The dropped frame's line names its type and the last 6 characters of
    /// its session id.
    #[test]
    fn a_dropped_frame_is_named_by_its_type_and_a_short_session_id() {
        use serde_json::json;
        assert_eq!(
            frame_tag(&json!({ "t": "rc:ice", "session_id": SID })),
            ("rc:ice", "a7b8c9")
        );
        assert_eq!(
            frame_tag(&json!({ "t": "rc:sdp.offer" })),
            ("rc:sdp.offer", "")
        );
        assert_eq!(
            frame_tag(&json!({ "t": "rc:ice", "session_id": "abc" })),
            ("rc:ice", "abc")
        );
        assert_eq!(frame_tag(&json!({ "session_id": 7 })), ("?", ""));
    }
}
