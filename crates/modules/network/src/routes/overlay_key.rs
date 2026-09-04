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
    models::{KeyRotationRequest, RpcCap},
    signaling::ServerMsg,
};
use serde::Serialize;

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

    // Tenant-scoped: a foreign agent id is a 404, not a cross-tenant order.
    let agent = state.fleet.agents.find_in_tenant(tid, aid).await?;

    let request_id = ObjectId::new().to_hex();
    let online = state.fleet.rc_hub.is_agent_online(aid);
    // The hello caps are persisted on the row at every connect, so for a
    // live device this is what it advertised THIS session.
    let supports_rotation = agent.capabilities.has_rpc(RpcCap::KeyRotate);
    // Keyed on the device in both slots: the ceiling is per device, not per
    // admin — see `AppState::key_rotation_rate_limiter`. Checked AFTER the
    // identity gates so a refusal is attributable and audited.
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
                requested_by: auth.user_id,
                requested_at: DateTime::now(),
                delivered_at: None,
                // P1c — what the device holds NOW; a join under another key
                // is the proof the rotation happened, report or no report.
                public_key_before: agent
                    .overlay_identity
                    .as_ref()
                    .map(|i| i.public_key.clone()),
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
            auth.user_id,
            &request_id,
            outcome.as_ref().ok().map(|d| d.wire()),
            outcome.as_ref().err().map(|r| r.wire()),
        )
        .await
    {
        tracing::warn!(%e, "key rotation audit write failed");
    }

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

// FR-69 P5c — the order predicates moved to the wire crate's models (the agent
// socket, the fleet module's now, evaluates them on connect); re-exported so
// this file's tests and any `routes::overlay_key::…` path read as before.
pub use roomler_ai_remote_control::models::{
    REDELIVER_AFTER_SECS, order_is_satisfied, should_redeliver,
};

#[cfg(test)]
mod tests {
    use super::*;

    fn req(delivered_secs_ago: Option<i64>, now: DateTime) -> KeyRotationRequest {
        KeyRotationRequest {
            request_id: "r1".into(),
            requested_by: ObjectId::new(),
            requested_at: now,
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
