// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-129 A1 — the "device paired" notice: when a person approves a pairing
//! into an org, the org's OTHER `MANAGE_AGENTS` holders hear of it by web
//! push and by email (operator decision 3, 2026-10-28) — "<approver> paired
//! <machine> (<os>) into <org> from <country or 'an unknown location'>".
//!
//! Spawned from `approve` and best-effort end to end, like the owner-consent
//! consumer it is modelled on (`consent_consumer.rs`): a notice that could
//! not be sent is logged and never costs the approval its answer. The
//! approver is never a recipient — they know.

use bson::oid::ObjectId;
use roomler_ai_db::models::role::permissions;
use roomler_ai_services::email::DevicePairedNotice;
use tracing::{debug, info, warn};

use crate::{FleetState, pairing::PairingRole};

/// The human label for an `OsKind` wire spelling, for the notice's text.
fn os_label(os: &str) -> &str {
    match os {
        "linux" => "Linux",
        "macos" => "macOS",
        "windows" => "Windows",
        other => other,
    }
}

/// What the pairing enrolls, so an admin reading the notice can tell a
/// tunnel client — paired by any member — from the full daemon.
pub fn role_label(role: PairingRole) -> &'static str {
    match role {
        PairingRole::Daemon => {
            "remote desktop and the private network (roomlerd — runs as SYSTEM/root when installed system-wide)"
        }
        PairingRole::Tunnel => "a tunnel client (the roomler CLI)",
    }
}

/// Spawn the notice for one approval. Returns at once.
pub fn spawn_paired_notice(
    state: FleetState,
    tenant_id: ObjectId,
    approver_id: ObjectId,
    role: PairingRole,
    machine_name: String,
    os: String,
    country: Option<String>,
) {
    tokio::spawn(async move {
        if let Err(e) = send_paired_notice(
            &state,
            tenant_id,
            approver_id,
            role,
            &machine_name,
            &os,
            country,
        )
        .await
        {
            warn!(tenant = %tenant_id, error = %e, "pairing: the device-paired notice failed");
        }
    });
}

/// Resolve the org, the approver and the recipients, then send the two
/// legs per recipient. A failure on one recipient or one leg does not stop
/// the rest.
pub async fn send_paired_notice(
    state: &FleetState,
    tenant_id: ObjectId,
    approver_id: ObjectId,
    role: PairingRole,
    machine_name: &str,
    os: &str,
    country: Option<String>,
) -> anyhow::Result<()> {
    let tenant = state.tenants.base.find_by_id(tenant_id).await?;
    let approver = state.users.base.find_by_id(approver_id).await?;
    let recipients: Vec<ObjectId> = state
        .tenants
        .members_with_permission(tenant_id, permissions::MANAGE_AGENTS)
        .await?
        .into_iter()
        .filter(|id| *id != approver_id)
        .collect();
    if recipients.is_empty() {
        debug!(tenant = %tenant_id, "pairing: no other admin to notify");
        return Ok(());
    }
    let location = country
        .as_deref()
        .map(str::to_string)
        .unwrap_or_else(|| "an unknown location".to_string());
    let os_label = os_label(os);
    let title = "A device was paired into your organization";
    // The push body is plain text (the browser renders a notification's
    // text as text), with the kind named so a tunnel client reads as one.
    let body = format!(
        "{} paired {} ({}) into {} from {} — {}",
        approver.display_name,
        machine_name,
        os_label,
        tenant.name,
        location,
        role_label(role)
    );
    let devices_path = format!("/tenant/{}/devices", tenant_id.to_hex());
    let devices_url = format!(
        "{}{}",
        state.settings.app.frontend_url.trim_end_matches('/'),
        devices_path
    );
    let notice = DevicePairedNotice {
        approver_name: approver.display_name.clone(),
        machine_name: machine_name.to_string(),
        os_label: os_label.to_string(),
        role_label: role_label(role).to_string(),
        org_name: tenant.name.clone(),
        location: location.clone(),
        devices_url: devices_url.clone(),
    };
    let mut emails = 0usize;
    let mut pushes = 0usize;
    for admin_id in recipients {
        if let Some(email) = &state.email {
            match state.users.base.find_by_id(admin_id).await {
                Ok(admin) => {
                    if let Err(e) = email.send_device_paired_notice(&admin.email, &notice).await {
                        debug!(user = %admin_id, error = %e, "pairing: notice email failed");
                    } else {
                        emails += 1;
                    }
                }
                Err(e) => {
                    debug!(user = %admin_id, error = %e, "pairing: notice recipient not read")
                }
            }
        }
        if let Some(push) = &state.push {
            let subs = state
                .push_subscriptions
                .find_by_user(admin_id)
                .await
                .unwrap_or_default();
            for sub in subs {
                // Best-effort per subscription: a stale endpoint must not
                // block the others.
                if push
                    .send(
                        &sub.endpoint,
                        &sub.keys.auth,
                        &sub.keys.p256dh,
                        title,
                        &body,
                        Some(&devices_url),
                    )
                    .await
                    .is_ok()
                {
                    pushes += 1;
                }
            }
        }
    }
    info!(tenant = %tenant_id, emails, pushes, "pairing: device-paired notice sent");
    Ok(())
}
