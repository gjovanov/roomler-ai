// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! C-2 — rc control events on the global channel: IDEMPOTENT hub operations
//! every pod applies locally (the one holding the session/agent acts; the
//! rest no-op), which makes consent delivery and admin kicks
//! location-transparent. FR-69 P5a — moved from the host's `ws/remote_control.rs`:
//! the publisher needs only the core (Redis), the applier only the Hub.

use bson::oid::ObjectId;
use roomler_core::Core;
use tracing::{debug, info};

use crate::Hub;

/// C-2 — publish an rc control event on the global channel. Control
/// events are IDEMPOTENT hub operations every pod applies locally (the
/// one holding the session/agent acts; the rest no-op), which makes
/// consent delivery and admin kicks location-transparent: the HTTP
/// request can land on any pod. The publisher already applied locally —
/// the self-echo guard in the forwarder skips its own envelope.
pub async fn publish_rc_ctrl(state: &Core, evt: &str, mut fields: serde_json::Value) {
    let Some(redis) = &state.redis_pubsub else {
        return;
    };
    if let Some(obj) = fields.as_object_mut() {
        obj.insert("class".into(), serde_json::json!("rc"));
        obj.insert("evt".into(), serde_json::json!(evt));
    }
    let env = serde_json::json!({ "origin": redis.instance_id(), "ctrl": fields });
    if let Err(e) = redis.publish(&env.to_string()).await {
        debug!(%evt, %e, "rc ctrl publish failed (cross-pod delivery degraded)");
    }
}

/// C-2 — apply a received rc control event to THIS pod's hub. Idempotent:
/// misses are no-ops (the entity lives elsewhere or is already gone).
pub fn apply_rc_ctrl(hub: &Hub, ctrl: &serde_json::Value) {
    match ctrl.get("evt").and_then(|v| v.as_str()) {
        Some("consent") => {
            let (Some(sid), Some(granted)) = (
                ctrl.get("session_id")
                    .and_then(|v| v.as_str())
                    .and_then(|s| ObjectId::parse_str(s).ok()),
                ctrl.get("granted").and_then(|v| v.as_bool()),
            ) else {
                return;
            };
            // Cross-pod replay of an approve-link decision: a human clicked a
            // button, so there is no deny REASON to carry (FR-27).
            if hub.deliver_consent(sid, granted, None).is_ok() {
                info!(session = %sid, granted, "rc ctrl: consent applied from another pod");
            }
        }
        Some("kick") => {
            let Some(aid) = ctrl
                .get("agent_id")
                .and_then(|v| v.as_str())
                .and_then(|s| ObjectId::parse_str(s).ok())
            else {
                return;
            };
            if hub.unregister_agent(aid, None) {
                info!(agent = %aid, "rc ctrl: kick applied from another pod");
            }
        }
        // P3 — admin/controller force-close for a session homed on another
        // pod (the HTTP terminate route already authz'd against the named
        // tenant; the local terminate there was a silent no-op cross-pod).
        // The tenant re-check here stops a forged/mismatched envelope from
        // killing an identically-numbered session in a different tenant.
        Some("terminate") => {
            let (Some(sid), Some(tid)) = (
                ctrl.get("session_id")
                    .and_then(|v| v.as_str())
                    .and_then(|s| ObjectId::parse_str(s).ok()),
                ctrl.get("tenant_id")
                    .and_then(|v| v.as_str())
                    .and_then(|s| ObjectId::parse_str(s).ok()),
            ) else {
                return;
            };
            if hub.session_snapshot(sid).is_some_and(|(t, _)| t == tid)
                && hub
                    .terminate(
                        sid,
                        roomler_ai_remote_control::models::EndReason::AdminTerminated,
                    )
                    .is_ok()
            {
                info!(session = %sid, "rc ctrl: terminate applied from another pod");
            }
        }
        // P2b — cycle an agent's WS so it re-establishes its overlay session
        // on the tenant's NEW block (self_ip binds once, at establish). Rides
        // the ctrl lane rather than a directed RPC because the renumber
        // touches a whole tenant at once and the HTTP call can land on any
        // pod: every pod applies, the one holding the socket acts.
        Some("overlay_cycle") => {
            let Some(aid) = ctrl
                .get("agent_id")
                .and_then(|v| v.as_str())
                .and_then(|s| ObjectId::parse_str(s).ok())
            else {
                return;
            };
            if hub.cycle_agent_ws(aid) {
                info!(agent = %aid, "rc ctrl: overlay renumber cycle applied from another pod");
            }
        }
        // Multi-org — deliver a cross-org join push to an agent homed on
        // whichever pod holds its socket. `send_to_agent_in_tenant` re-checks
        // the tenant: this envelope carries a live enrollment token and every
        // pod applies it, so a mismatched one must not be able to hand a
        // different tenant's device a token for an org it never asked to join.
        Some("agent_join_org") => {
            let (Some(aid), Some(tid), Some(token)) = (
                ctrl.get("agent_id")
                    .and_then(|v| v.as_str())
                    .and_then(|s| ObjectId::parse_str(s).ok()),
                ctrl.get("tenant_id")
                    .and_then(|v| v.as_str())
                    .and_then(|s| ObjectId::parse_str(s).ok()),
                ctrl.get("enrollment_token")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
            ) else {
                return;
            };
            let msg = roomler_ai_remote_control::signaling::ServerMsg::JoinOrg {
                enrollment_token: token,
                label: ctrl
                    .get("label")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                overlay_mode: ctrl
                    .get("overlay_mode")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
            };
            if hub.send_to_agent_in_tenant(aid, tid, msg).is_ok() {
                info!(agent = %aid, "rc ctrl: cross-org join delivered from another pod");
            }
        }
        _ => {}
    }
}
