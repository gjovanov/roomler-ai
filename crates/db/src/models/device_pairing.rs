// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
use bson::{DateTime, oid::ObjectId};
use serde::{Deserialize, Serialize};

/// FR-129 A1 — one device pairing: an installer's RFC 8628-shaped request
/// for an enrollment token, from `POST /api/pair/start` until the first poll
/// that finds it approved mints the token and marks it `delivered`
/// (`docs/fr/FR-129-guided-cross-platform-installer.md`, "The pairing API").
///
/// Two codes, two holders. The **device code** is the installer's secret
/// and the only credential that can retrieve a token; it is stored ONLY as
/// `device_code_hash` (`hex(SHA-256(code))`), so a copy of the database
/// cannot poll. The **user code** names the pairing to the person who types
/// it in the browser and retrieves nothing. **No token is ever stored**: a
/// token is minted at delivery, by the poll that wins the CAS
/// `approved → delivered`, and handed to the installer once.
///
/// ⚠️ `expires_at` is the authority on a pairing's life and every reader
/// compares it to the clock; the TTL index on it only tidies, and the
/// monitor runs about a minute late.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DevicePairing {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    /// `hex(SHA-256(device_code))`. Unique. The code itself is never stored.
    pub device_code_hash: String,
    /// The 8 consonants the person types (`BCDFGHJKLMNPQRSTVWXZ`), stored
    /// without the display hyphen. Unique among `pending` rows.
    pub user_code: String,
    #[serde(default)]
    pub status: PairingStatus,
    /// What the installer said about the device, clamped (trimmed, capped),
    /// never refused: shown on `/pair` before Approve.
    pub machine_name: String,
    /// `OsKind`'s wire spelling (`linux` · `macos` · `windows`), validated at
    /// the route; a string here because this crate sits below the wire crate.
    pub os: String,
    /// `x86_64` · `aarch64`.
    pub arch: String,
    /// `daemon` · `tunnel` — which enrollment token delivery mints.
    pub role: String,
    /// How many tokens delivery mints: 1, or 2 for a macOS daemon whose
    /// root half is a second enrollment (operator decision 7, 2026-10-28).
    pub halves: u8,
    /// The client that started the pairing (`wizard` · `roomlerd` ·
    /// `roomler` · `script-sh` · `script-ps1` · `other`) and its version.
    pub client: String,
    pub client_version: String,
    /// The client address `start` was seen from, as the trusted-proxy chain
    /// reports it; shown to the person only as a country.
    pub start_ip: Option<String>,
    /// ISO country of `start_ip`, or `None` when the lookup had no answer —
    /// never fabricated.
    pub country: Option<String>,
    pub created_at: DateTime,
    pub expires_at: DateTime,
    /// The last poll's arrival; a poll sooner than `poll_interval_secs`
    /// after it answers `slow_down`.
    pub last_poll_at: Option<DateTime>,
    /// The installer's current interval, lengthened by 5 s on each
    /// `slow_down` (RFC 8628 §3.5).
    pub poll_interval_secs: u32,
    /// The person who approved or denied, and the org they approved into.
    pub approver_user_id: Option<ObjectId>,
    pub tenant_id: Option<ObjectId>,
    pub decided_at: Option<DateTime>,
    pub delivered_at: Option<DateTime>,
    pub updated_at: DateTime,
}

/// The pairing's state. The transitions are compare-and-set on the stored
/// status, so two concurrent approvals, or two polls after one approval,
/// cannot both win: `pending → approved | denied | expired`,
/// `approved → delivered | denied | expired`. Delivery is once.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PairingStatus {
    #[default]
    Pending,
    /// The decision is recorded; no token exists yet.
    Approved,
    /// Denied by a person, or the approver's gate was gone at delivery.
    Denied,
    /// The token(s) were minted and handed to the installer, once.
    Delivered,
    /// Found past `expires_at` by a reader before the TTL sweep took the
    /// row; recorded so the expiry is audited exactly once.
    Expired,
}

impl PairingStatus {
    /// The stored spelling — the CAS filters name it.
    pub fn wire(self) -> &'static str {
        match self {
            PairingStatus::Pending => "pending",
            PairingStatus::Approved => "approved",
            PairingStatus::Denied => "denied",
            PairingStatus::Delivered => "delivered",
            PairingStatus::Expired => "expired",
        }
    }
}

impl DevicePairing {
    pub const COLLECTION: &'static str = "device_pairings";
}
