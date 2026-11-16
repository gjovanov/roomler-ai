// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-40 — `POST /api/tenant/{tid}/agent/{agent_id}/overlay-key/rotate`:
//! order a device to retire its overlay (WireGuard) key
//! (`docs/fr/FR-40-overlay-key-rotation.md`).
//!
//! The server ORDERS a re-mint it never sees. Nothing here touches a key:
//! the route writes a request onto the agent row (desired state, so an
//! offline device is ordered on its next connect), pushes
//! `rc:agent.key_rotate` if the device is live and understands it, and
//! audits the decision — both arms, from one call site.
//!
//! Gates: `MANAGE_AGENTS` (retiring a key grants nothing, so no
//! `EXEC_DEVICE` / `SSH_DEVICE` bit is involved), one order per device per
//! minute, and the capability verb — a pre-feature agent drops an unknown
//! frame silently, and a silently-evaporated SECURITY action shown as "in
//! flight" on a dashboard is worse than the config case this copies.

use axum::{
    Json,
    extract::{Path, State},
};
use bson::{DateTime, oid::ObjectId};
use roomler_ai_db::models::role::permissions;
use roomler_ai_remote_control::{
    models::{Agent, KeyRotationRequest, RetiredKeyMode, RpcCap},
    signaling::ServerMsg,
};
use roomler_ai_services::dao::base::DaoError;
use serde::{Deserialize, Serialize};

use roomler_core::{ApiError, extractors::auth::AuthUser};

use crate::NetworkState;
use roomler_core::guards::require_permission;

/// One order per device per minute. A rotation churns every peer's carrier
/// to this device; a second click inside the window is the same storm.
pub const RATE_LIMIT_PER_MINUTE: u32 = 1;

/// How an accepted order left the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dispatch {
    /// A live socket took it.
    Pushed,
    /// The device is offline (or the push raced a disconnect); it is ordered
    /// on its next connect.
    Queued,
}

impl Dispatch {
    pub fn wire(self) -> &'static str {
        match self {
            Self::Pushed => "pushed",
            Self::Queued => "queued",
        }
    }
}

/// Why an order was refused. Each carries the operator's next move.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyRotationDenyReason {
    RateLimited,
    /// The device is ONLINE and its agent does not advertise `key-rotate`.
    /// (An offline device is queued regardless — it may well update before
    /// it reconnects, and the connect-time reconcile gates again.)
    AgentUnsupported,
}

impl KeyRotationDenyReason {
    pub fn wire(self) -> &'static str {
        match self {
            Self::RateLimited => "rate_limited",
            Self::AgentUnsupported => "agent_unsupported",
        }
    }

    pub fn message(self) -> &'static str {
        match self {
            Self::RateLimited => {
                "a rotation was ordered for this device less than a minute ago — wait for it \
                 to land (or fail) before ordering another"
            }
            Self::AgentUnsupported => {
                "this device's agent predates overlay-key rotation (needs 0.4.25 or later) — \
                 push an update first, then rotate"
            }
        }
    }
}

/// The pure decision. Kept free of I/O so the four cells of its truth table
/// are unit-tested; the handler only gathers the inputs.
pub fn decide(
    rate_ok: bool,
    online: bool,
    supports_rotation: bool,
) -> Result<Dispatch, KeyRotationDenyReason> {
    if !rate_ok {
        return Err(KeyRotationDenyReason::RateLimited);
    }
    if online && !supports_rotation {
        return Err(KeyRotationDenyReason::AgentUnsupported);
    }
    Ok(if online {
        Dispatch::Pushed
    } else {
        Dispatch::Queued
    })
}

/// Who placed an order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderedBy {
    /// An operator, through the route (which checked `MANAGE_AGENTS`).
    Operator(ObjectId),
    /// FR-40 P2 — the server itself, for the reason named: the order's
    /// `trigger`, today only `key_retired`. No user asked, so the order and
    /// its audit row carry the zero id.
    Server(&'static str),
}

impl OrderedBy {
    /// The `user_id` the order and its audit row carry.
    pub fn user_id(self) -> ObjectId {
        match self {
            Self::Operator(user) => user,
            Self::Server(_) => ObjectId::from_bytes([0; 12]),
        }
    }

    pub fn trigger(self) -> Option<&'static str> {
        match self {
            Self::Operator(_) => None,
            Self::Server(why) => Some(why),
        }
    }
}

/// What [`place_order`] did: the order's id, and the decision.
#[derive(Debug)]
pub struct Placed {
    pub request_id: String,
    pub outcome: Result<Dispatch, KeyRotationDenyReason>,
}

/// THE path a rotation order takes, whoever places it — the route below for
/// an operator, the overlay join for a device that presented a retired key
/// (FR-40 P2): the gates, the desired state on the row, the push, and ONE
/// audit row for either arm. One function, so an order the server places
/// cannot skip a gate, or an audit, that an operator's goes through.
///
/// `agent` is the LIVE row, already resolved in the tenant;
/// `public_key_before` is the key the device holds as the order is placed
/// (P1c: a join under any other key afterwards is the proof it rotated).
pub async fn place_order(
    state: &NetworkState,
    tid: ObjectId,
    aid: ObjectId,
    agent: &Agent,
    by: OrderedBy,
    public_key_before: Option<String>,
) -> Result<Placed, DaoError> {
    let request_id = ObjectId::new().to_hex();
    let online = state.fleet.rc_hub.is_agent_online(aid);
    // The hello caps are persisted on the row at every connect, so for a
    // live device this is what it advertised THIS session.
    let supports_rotation = agent.capabilities.has_rpc(RpcCap::KeyRotate);
    // Keyed on the device in both slots: the ceiling is per device, not per
    // admin — see `AppState::key_rotation_rate_limiter` — and the server's
    // own orders count against it too. Checked AFTER the identity gates so a
    // refusal is attributable and audited.
    let rate_ok = state
        .key_rotation_rate_limiter
        .check(aid, aid, RATE_LIMIT_PER_MINUTE);

    let verdict = decide(rate_ok, online, supports_rotation);

    // Desired state FIRST, then the push: a report can only ever refer to an
    // order that already exists on the row.
    let outcome: Result<Dispatch, KeyRotationDenyReason> = match verdict {
        Ok(dispatch) => {
            let request = KeyRotationRequest {
                request_id: request_id.clone(),
                requested_by: by.user_id(),
                requested_at: DateTime::now(),
                trigger: by.trigger().map(str::to_string),
                delivered_at: None,
                public_key_before,
            };
            state
                .fleet
                .agents
                .record_key_rotation_request(tid, aid, &request)
                .await?;
            let pushed = dispatch == Dispatch::Pushed
                && state
                    .fleet
                    .rc_hub
                    .send_to_agent(
                        aid,
                        ServerMsg::KeyRotate {
                            request_id: request_id.clone(),
                        },
                    )
                    .is_ok();
            if pushed {
                if let Err(e) = state
                    .fleet
                    .agents
                    .mark_key_rotation_delivered(tid, aid, &request_id)
                    .await
                {
                    tracing::warn!(%e, "key_rotation delivered_at write failed");
                }
                Ok(Dispatch::Pushed)
            } else {
                // Offline, or the push raced a disconnect: the order stands
                // and the connect-time reconcile delivers it.
                Ok(Dispatch::Queued)
            }
        }
        Err(reason) => Err(reason),
    };

    // ONE call site records both arms (the `config_audit` / `ssh_audit`
    // shape), so a new refusal cannot forget to audit itself. Best-effort:
    // an audit insert must never be what stops a legitimate rotation.
    if let Err(e) = state
        .key_rotation_audit
        .record(
            tid,
            aid,
            by.user_id(),
            by.trigger(),
            &request_id,
            outcome.as_ref().ok().map(|d| d.wire()),
            outcome.as_ref().err().map(|r| r.wire()),
        )
        .await
    {
        tracing::warn!(%e, "key rotation audit write failed");
    }

    Ok(Placed {
        request_id,
        outcome,
    })
}

#[derive(Debug, Serialize)]
pub struct RotateKeyResult {
    pub agent_id: String,
    /// Echoed by the device in its report; the dashboard matches on it.
    pub request_id: String,
    /// `pushed` or `queued`.
    pub dispatch: &'static str,
    /// `true` when the order reached a live socket. `false` = queued for the
    /// device's next connect.
    pub delivered: bool,
}

pub async fn rotate_overlay_key(
    State(state): State<NetworkState>,
    auth: AuthUser,
    Path((tenant_id, agent_id)): Path<(String, String)>,
) -> Result<Json<RotateKeyResult>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;
    let aid = ObjectId::parse_str(&agent_id)
        .map_err(|_| ApiError::BadRequest("Invalid agent_id".to_string()))?;

    require_permission(
        &state,
        tid,
        auth.user_id,
        permissions::MANAGE_AGENTS,
        "MANAGE_AGENTS",
    )
    .await?;

    // Tenant-scoped: a foreign agent id is a 404, not a cross-tenant order —
    // and LIVE: a rotation order on a removed device would be desired state
    // written onto a tombstone, with nothing left to reconcile it (#1821).
    let agent = state.fleet.agents.find_live_in_tenant(tid, aid).await?;

    let Placed {
        request_id,
        outcome,
    } = place_order(
        &state,
        tid,
        aid,
        &agent,
        OrderedBy::Operator(auth.user_id),
        // P1c — what the device holds NOW; a join under another key is the
        // proof the rotation happened, report or no report.
        agent
            .overlay_identity
            .as_ref()
            .map(|i| i.public_key.clone()),
    )
    .await?;

    let dispatch = match outcome {
        Ok(d) => d,
        Err(reason) => {
            tracing::info!(
                admin = %auth.user_id, agent = %aid, reason = reason.wire(),
                "overlay-key rotation refused"
            );
            return Err(ApiError::Conflict(format!(
                "{}: {}",
                reason.wire(),
                reason.message()
            )));
        }
    };
    tracing::info!(
        admin = %auth.user_id, agent = %aid, %request_id, dispatch = dispatch.wire(),
        "overlay-key rotation ordered"
    );
    Ok(Json(RotateKeyResult {
        agent_id: aid.to_hex(),
        request_id,
        dispatch: dispatch.wire(),
        delivered: dispatch == Dispatch::Pushed,
    }))
}

/// FR-40 P2 — the org's retired-key posture, as the switch route answers it.
#[derive(Debug, Serialize)]
pub struct RetiredKeyModeView {
    pub mode: RetiredKeyMode,
}

/// The body of the switch's `PUT`. A string, parsed strictly by the handler:
/// the mode's own `Deserialize` is lenient (a stored value a rolled-back build
/// does not know reads as `off`), and an admin's typo must be a 400 instead.
#[derive(Debug, Deserialize)]
pub struct SetRetiredKeyModeBody {
    pub mode: String,
}

fn tenant_of(tenant_id: &str) -> Result<ObjectId, ApiError> {
    ObjectId::parse_str(tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))
}

/// `GET /api/tenant/{tenant_id}/overlay-retired-keys` — whether this org
/// refuses retired overlay keys (`off` | `warn` | `enforce`). `MANAGE_AGENTS`,
/// the permission that orders a rotation.
pub async fn get_retired_key_mode(
    State(state): State<NetworkState>,
    auth: AuthUser,
    Path(tenant_id): Path<String>,
) -> Result<Json<RetiredKeyModeView>, ApiError> {
    let tid = tenant_of(&tenant_id)?;
    require_permission(
        &state,
        tid,
        auth.user_id,
        permissions::MANAGE_AGENTS,
        "MANAGE_AGENTS",
    )
    .await?;
    let mode = state
        .overlay_networks
        .get_or_create(tid)
        .await?
        .retired_key_mode;
    Ok(Json(RetiredKeyModeView { mode }))
}

/// `PUT /api/tenant/{tenant_id}/overlay-retired-keys` `{ "mode": … }` — the
/// org's switch. It disturbs no live session: the mode is read at each join
/// and DERP registration, so it applies from the next one. `MANAGE_AGENTS`:
/// whoever may order a device's rotation may have the server order it when
/// the device presents a retired key.
pub async fn set_retired_key_mode(
    State(state): State<NetworkState>,
    auth: AuthUser,
    Path(tenant_id): Path<String>,
    Json(body): Json<SetRetiredKeyModeBody>,
) -> Result<Json<RetiredKeyModeView>, ApiError> {
    let tid = tenant_of(&tenant_id)?;
    require_permission(
        &state,
        tid,
        auth.user_id,
        permissions::MANAGE_AGENTS,
        "MANAGE_AGENTS",
    )
    .await?;
    let mode = RetiredKeyMode::from_wire(&body.mode).ok_or_else(|| {
        ApiError::BadRequest("mode must be one of: off, warn, enforce".to_string())
    })?;
    state
        .overlay_networks
        .set_retired_key_mode(tid, mode)
        .await?;
    tracing::warn!(
        tenant = %tid, admin = %auth.user_id, mode = mode.wire(),
        "overlay retired keys: org switch changed"
    );
    Ok(Json(RetiredKeyModeView { mode }))
}

// FR-69 P5c — the order predicates moved to the wire crate's models (the agent
// socket, the fleet module's now, evaluates them on connect); re-exported so
// this file's tests and any `routes::overlay_key::…` path read as before.
pub use roomler_ai_remote_control::models::{
    REDELIVER_AFTER_SECS, order_in_flight, order_is_satisfied, should_redeliver,
};

#[cfg(test)]
mod tests {
    use super::*;

    fn req(delivered_secs_ago: Option<i64>, now: DateTime) -> KeyRotationRequest {
        KeyRotationRequest {
            request_id: "r1".into(),
            requested_by: ObjectId::new(),
            requested_at: now,
            trigger: None,
            public_key_before: None,
            delivered_at: delivered_secs_ago
                .map(|s| DateTime::from_millis(now.timestamp_millis() - s * 1000)),
        }
    }

    fn report(request_id: &str) -> roomler_ai_remote_control::models::KeyRotationReport {
        roomler_ai_remote_control::models::KeyRotationReport {
            request_id: request_id.into(),
            outcome: roomler_ai_remote_control::models::KeyRotationOutcome::Rotated,
            old_public_key: None,
            new_public_key: None,
            key_epoch: 1,
            detail: None,
            reported_at: DateTime::now(),
        }
    }

    fn ident(key: &str, at: DateTime) -> roomler_ai_remote_control::models::OverlayIdentity {
        roomler_ai_remote_control::models::OverlayIdentity {
            public_key: key.into(),
            key_epoch: 1,
            joined_at: at,
        }
    }

    /// The third cycle: one click must never become three rotations.
    #[test]
    fn an_order_the_device_already_executed_is_satisfied_by_its_join_alone() {
        let now = DateTime::now();
        let later = DateTime::from_millis(now.timestamp_millis() + 5_000);
        let earlier = DateTime::from_millis(now.timestamp_millis() - 5_000);
        let mut r = req(Some(3600), now); // long past the P1b window, no report
        r.public_key_before = Some("OLD==".into());
        assert!(order_is_satisfied(&r, Some(&ident("NEW==", later))));
        // Same key after the order: nothing happened yet.
        assert!(!order_is_satisfied(&r, Some(&ident("OLD==", later))));
        // A different key but from a join BEFORE the order proves nothing.
        assert!(!order_is_satisfied(&r, Some(&ident("NEW==", earlier))));
        // No snapshot (pre-P1c order) or no identity: cannot be judged.
        assert!(!order_is_satisfied(
            &req(Some(3600), now),
            Some(&ident("NEW==", later))
        ));
        assert!(!order_is_satisfied(&r, None));
    }

    /// The duplicate-delivery race from the first field run.
    #[test]
    fn a_freshly_delivered_order_is_not_pushed_again_on_reconnect() {
        let now = DateTime::now();
        assert!(!should_redeliver(&req(Some(1), now), None, now));
        assert!(!should_redeliver(
            &req(Some(REDELIVER_AFTER_SECS - 1), now),
            None,
            now
        ));
    }

    #[test]
    fn an_undelivered_or_stale_unanswered_order_is_pushed() {
        let now = DateTime::now();
        assert!(should_redeliver(&req(None, now), None, now));
        assert!(should_redeliver(
            &req(Some(REDELIVER_AFTER_SECS), now),
            None,
            now
        ));
        assert!(should_redeliver(&req(Some(3600), now), None, now));
    }

    #[test]
    fn an_answered_order_is_never_pushed_and_an_old_answer_does_not_count() {
        let now = DateTime::now();
        assert!(!should_redeliver(&req(None, now), Some(&report("r1")), now));
        // A report about an EARLIER order says nothing about this one.
        assert!(should_redeliver(&req(None, now), Some(&report("r0")), now));
    }

    #[test]
    fn the_ceiling_refuses_before_anything_else() {
        assert_eq!(
            decide(false, true, true),
            Err(KeyRotationDenyReason::RateLimited)
        );
        assert_eq!(
            decide(false, false, false),
            Err(KeyRotationDenyReason::RateLimited)
        );
    }

    #[test]
    fn a_live_device_without_the_verb_is_refused_not_queued() {
        // The order would evaporate in its unknown-tag branch while the
        // dashboard showed a rotation in flight.
        assert_eq!(
            decide(true, true, false),
            Err(KeyRotationDenyReason::AgentUnsupported)
        );
    }

    #[test]
    fn a_live_capable_device_is_pushed_and_an_offline_one_is_queued() {
        assert_eq!(decide(true, true, true), Ok(Dispatch::Pushed));
        assert_eq!(decide(true, false, true), Ok(Dispatch::Queued));
        // Offline + no verb on record: queued anyway — the row's caps may be
        // stale (it can update before it reconnects) and the connect-time
        // reconcile gates on the live hello.
        assert_eq!(decide(true, false, false), Ok(Dispatch::Queued));
    }

    /// FR-40 P2 — the loop guard on the server's own order: a device that
    /// keeps presenting a retired key while its order is being executed gets
    /// no second one. Anything else may be ordered again, under the ceiling.
    #[test]
    fn only_an_order_being_executed_holds_off_another() {
        let now = DateTime::now();
        let later = DateTime::from_millis(now.timestamp_millis() + 5_000);
        let earlier = DateTime::from_millis(now.timestamp_millis() - 5_000);
        // The server's order snapshots the key the device PRESENTED (the
        // retired one); its last accepted join, under the row's key, predates
        // the order.
        let mut fresh = req(Some(5), now);
        fresh.public_key_before = Some("RETIRED==".into());
        // Delivered seconds ago, unanswered: in flight.
        assert!(order_in_flight(
            &fresh,
            None,
            Some(&ident("CURRENT==", earlier)),
            now
        ));
        // Answered (whatever the answer): not in flight, the device said.
        assert!(!order_in_flight(&fresh, Some(&report("r1")), None, now));
        // ...but an answer to an EARLIER order says nothing about this one.
        assert!(order_in_flight(&fresh, Some(&report("r0")), None, now));
        // Satisfied by a join under another key: done, not in flight.
        assert!(!order_in_flight(
            &fresh,
            None,
            Some(&ident("NEW==", later)),
            now
        ));
        // Queued (never pushed), or delivered past the P1b window: whatever
        // carried it is gone, so it is not in flight.
        assert!(!order_in_flight(&req(None, now), None, None, now));
        assert!(!order_in_flight(
            &req(Some(REDELIVER_AFTER_SECS), now),
            None,
            None,
            now
        ));
    }

    /// FR-40 P2 — an order the server placed names no user and says why;
    /// an operator's names them and says nothing.
    #[test]
    fn the_server_orders_as_nobody_with_a_reason() {
        let admin = ObjectId::new();
        assert_eq!(OrderedBy::Operator(admin).user_id(), admin);
        assert_eq!(OrderedBy::Operator(admin).trigger(), None);
        let by =
            OrderedBy::Server(roomler_ai_remote_control::models::KEY_ROTATION_TRIGGER_KEY_RETIRED);
        assert_eq!(by.user_id(), ObjectId::from_bytes([0; 12]));
        assert_eq!(by.trigger(), Some("key_retired"));
    }

    #[test]
    fn wire_strings_are_locked() {
        assert_eq!(Dispatch::Pushed.wire(), "pushed");
        assert_eq!(Dispatch::Queued.wire(), "queued");
        assert_eq!(KeyRotationDenyReason::RateLimited.wire(), "rate_limited");
        assert_eq!(
            KeyRotationDenyReason::AgentUnsupported.wire(),
            "agent_unsupported"
        );
    }
}
