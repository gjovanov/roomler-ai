// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
use bson::{DateTime, oid::ObjectId};
use serde::{Deserialize, Serialize};

/// FR-129 A1 — one decision about a device pairing (`pairing_audit`): a
/// start, a lookup, an approval, a denial, a delivery, an expiry, or a
/// refusal of any of them. The refused rows are the point — an account
/// probing codes, an address flooding `start`, an approver whose gate was
/// gone at delivery must all leave a trace. Best-effort, never gating the
/// request; 90-day TTL on `at` like the other decision logs.
///
/// Never the device code, never a token: a pairing is named by its row id.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairingAuditEvent {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    /// `start` · `lookup` · `approve` · `deny` · `deliver` · `expire` ·
    /// `refuse` (a request refused before it named an action, e.g. a poll
    /// with a code that names no pairing).
    pub action: String,
    /// `ok`, or `refused:<reason>` — the reason a short code
    /// (`ip_ceiling`, `account_locked`, `not_pending`, `no_permission`,
    /// `not_a_member`, `network_unmounted`, `gate_lost`, `slow_down`,
    /// `unknown_code`, `already_delivered`, `invalid_request`, …).
    pub outcome: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pairing_id: Option<ObjectId>,
    /// The signed-in person acting (lookup, approve, deny), or the approver
    /// a delivery minted for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<ObjectId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant_id: Option<ObjectId>,
    /// The request's client address as the trusted-proxy chain reports it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_ip: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine_name: Option<String>,
    /// `daemon` · `tunnel`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    pub at: DateTime,
}

impl PairingAuditEvent {
    pub const COLLECTION: &'static str = "pairing_audit";

    /// How long a row is kept, from `at`: 90 days, the decision-log norm.
    pub const TTL_SECS: u64 = 90 * 24 * 60 * 60;
}
