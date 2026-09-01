// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
use bson::{DateTime, oid::ObjectId};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tenant {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub name: String,
    pub slug: String,
    pub description: Option<String>,
    pub icon: Option<String>,
    pub owner_id: ObjectId,
    pub plan: Plan,
    pub features: Vec<String>,
    pub settings: TenantSettings,
    pub billing: Option<BillingInfo>,
    pub integrations: Option<IntegrationSettings>,
    #[serde(default)]
    pub is_archived: bool,
    pub created_at: DateTime,
    pub updated_at: DateTime,
    pub deleted_at: Option<DateTime>,
}

/// FR-32 — how far plan-limit checks are allowed to go for a tenant.
///
/// Mirrors [`crate::models::OverlayNetwork`]'s `acl_mode` deliberately, and for
/// the same reason: turning eleven previously-unenforced gates on against a live
/// fleet would lock out tenants that *we* let over the line.
///
/// # ⚠ `Warn` is the default BY STANDING DECISION — do not "fix" it
///
/// The original reason was the observe phase: a mode that does nothing produces
/// no data, and learning who *would* be denied was the point. **That reason is
/// spent.** The observe phase is over, every existing tenant has been moved to
/// [`Self::Enforce`], and the argument for keeping `Warn` as the default is now
/// a different one:
///
/// The grandfathering hazard does not apply to a NEW tenant — it starts at zero
/// usage and cannot be retroactively over a limit. So `Warn` here is not a
/// safety net; it is a deliberate **go-to-market choice** (operator, 2026-08-30):
/// while the product is in early growth, a promising signup must not be stopped
/// at 10 members or 100 MB before anyone has spoken to them.
///
/// The consequence is worth stating plainly, because it looks like a bug: with
/// this default, the advertised plan limits **do not fire for anyone who signs
/// up**. That is intended for now.
///
/// **Revisit when** the product leaves early growth, or when a real (non-test,
/// non-internal) tenant is on a paid plan — whichever comes first. Flipping
/// `#[default]` to `Enforce` is the whole change; the mode stays as a per-tenant
/// escape hatch and as the grandfathering mechanism for any future gate
/// introduced against existing usage. See FR-32 (#898).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PlanEnforcement {
    /// No check runs at all.
    Off,
    /// The check runs and the denial is recorded, but the request succeeds.
    #[default]
    Warn,
    /// The denial is returned to the caller.
    Enforce,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Plan {
    #[default]
    Free,
    Pro,
    Business,
    Enterprise,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TenantSettings {
    #[serde(default = "default_locale")]
    pub default_locale: String,
    #[serde(default)]
    pub default_message_notifications: NotificationLevel,
    #[serde(default)]
    pub mfa_required: bool,
    #[serde(default)]
    pub allow_guest_access: bool,
    // FR-32 — `max_members` and `file_upload_limit` lived here and were REMOVED.
    // They shadowed `PlanLimits::{max_members, storage_bytes}` and had zero
    // readers in the entire workspace — not two enforcers disagreeing, two dead
    // fields shadowing two dead fields. They also disagreed with the plan they
    // shadowed by 10x (this defaulted to 100; Free's PlanLimits says 10), so
    // wiring the wrong one would have changed who is over the line.
    //
    // `PlanLimits` is the single source of truth: it is what
    // `GET /api/stripe/plans` publishes, what `BillingView.vue` renders, and
    // what the billing test asserts. Serde ignores the leftover key in existing
    // tenant documents, so no migration is needed.
    //
    // A per-tenant override may well be wanted for bespoke Enterprise deals —
    // the same problem as the phantom `Enterprise` tier. Add it deliberately
    // then, with stated semantics, rather than inheriting one from an accident.
    /// Phase 2 MagicDNS — the tenant's overlay DNS suffix (e.g.
    /// `"myorg.roomler.net"`). When set, overlay nodes run a local split-DNS
    /// resolver that answers `<node-name>.<domain>` with the peer's overlay IP.
    /// `None` = MagicDNS off for the tenant.
    #[serde(default)]
    pub magic_dns_domain: Option<String>,
    /// Phase 2 MagicDNS — upstream nameservers the resolver forwards
    /// non-overlay queries to (e.g. `["1.1.1.1"]`). Empty = use the node's
    /// existing system resolvers as the fallback.
    #[serde(default)]
    pub magic_dns_nameservers: Vec<String>,
    /// Fleet RPC — the org-wide kill-switch (gate 1 of four). `false` (the
    /// default, and what every pre-feature row deserialises to) means no
    /// device in this org accepts a remote command, whatever its own
    /// `ExecPolicy` says. Flipping it on still leaves each device off until
    /// its own policy is enabled.
    #[serde(default)]
    pub remote_exec_enabled: bool,
    /// Roomler SSH — the org-wide kill-switch (gate 1 of four), the twin of
    /// [`Self::remote_exec_enabled`] and deliberately a SEPARATE switch: an org
    /// that allows bounded diagnostic commands has not thereby allowed
    /// interactive sessions. `false` by default and for every pre-feature row;
    /// flipping it on still leaves each device off until its own `SshPolicy`
    /// is enabled.
    #[serde(default)]
    pub remote_ssh_enabled: bool,

    /// FR-51 — org-wide switch for EPHEMERAL ENROLLMENT KEYS (gate 1 of the
    /// key path). `false` (the default, and what every pre-feature row
    /// deserialises to) means the mint route refuses and — deliberately —
    /// every already-minted key stops working too: the gate is checked on
    /// each USE, ahead of the key's own use-claim, so flipping this off is
    /// an org-wide revocation that burns nothing.
    ///
    /// A separate switch from the exec/SSH pair for the same reason they are
    /// separate from each other: a standing credential that mints device
    /// identities is its own grant, not an implication of any other.
    #[serde(default)]
    pub ephemeral_keys_enabled: bool,

    /// FR-32 — how far plan-limit checks may go for this tenant. Defaults to
    /// `Warn`, so a pre-FR-32 tenant document deserialises into observe mode:
    /// every newly wired gate is measured and logged, and none of them refuse.
    ///
    /// ⚠ This does NOT apply to the limits that were already enforced before
    /// FR-32 (`max_devices`, `max_tunnel_clients`) — see
    /// `roomler_ai_services::quota::Limit::is_established`. An established
    /// limit always enforces, so setting `Off` can never silently un-enforce
    /// the device cap.
    #[serde(default)]
    pub plan_enforcement: PlanEnforcement,
}

impl Default for TenantSettings {
    fn default() -> Self {
        Self {
            default_locale: default_locale(),
            default_message_notifications: NotificationLevel::default(),
            mfa_required: false,
            allow_guest_access: false,
            magic_dns_domain: None,
            magic_dns_nameservers: Vec::new(),
            remote_exec_enabled: false,
            remote_ssh_enabled: false,
            ephemeral_keys_enabled: false,
            plan_enforcement: PlanEnforcement::default(),
        }
    }
}

fn default_locale() -> String {
    "en-US".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum NotificationLevel {
    #[default]
    All,
    Mentions,
    Nothing,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BillingInfo {
    pub customer_id: Option<String>,
    pub subscription_id: Option<String>,
    pub current_period_end: Option<DateTime>,
    #[serde(default)]
    pub status: SubscriptionStatus,
    #[serde(default)]
    pub cancel_at_period_end: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SubscriptionStatus {
    #[default]
    Active,
    PastDue,
    Canceled,
    Trialing,
    Incomplete,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntegrationSettings {
    pub google_drive: Option<OAuthCredential>,
    pub onedrive: Option<OAuthCredential>,
    pub dropbox: Option<OAuthCredential>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthCredential {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: Option<DateTime>,
}

impl Tenant {
    pub const COLLECTION: &'static str = "tenants";
}

/// S5 pivot — the plan matrix is per-user priced with DEVICE caps
/// (Tailscale-style): the fleet limits lead, the collaboration limits
/// ride along. Serialized into `/api/stripe/plans`, snapshot-locked by
/// `crates/tests/src/billing_tests.rs` + `ui/e2e/billing.spec.ts` (update
/// them in tandem with any change here).
#[derive(Debug, Serialize)]
pub struct PlanLimits {
    // ── Fleet (primary) ─────────────────────────────────────────────
    /// Enrolled remote-control devices (`agents` rows, tombstones
    /// excluded). Enforced at agent enrollment.
    pub max_devices: u32,
    /// Enrolled tunnel clients. Enforced at tunnel enrollment.
    pub max_tunnel_clients: u32,
    /// Overlay mesh membership (private network between the tenant's
    /// nodes). On for every plan — it's the product's spine.
    pub overlay_mesh: bool,
    /// Exit nodes (route a node's whole egress through a mesh peer).
    pub exit_nodes: bool,
    /// MagicDNS (tenant overlay DNS domain).
    pub magic_dns: bool,
    /// Concurrent remote-control sessions per tenant. Advisory in S5
    /// (surfaced in the matrix; enforcement is a follow-up).
    pub max_concurrent_sessions: u32,
    // ── Collaboration (bundled) ─────────────────────────────────────
    pub max_members: u32,
    pub max_channels: u32,
    pub max_message_history: i64,
    pub storage_bytes: u64,
    pub video_max_participants: u32,
    pub cloud_integrations: bool,
    pub recordings: bool,
}

impl Plan {
    pub fn limits(&self) -> PlanLimits {
        match self {
            Plan::Free => PlanLimits {
                max_devices: 3,
                max_tunnel_clients: 3,
                overlay_mesh: true,
                exit_nodes: false,
                magic_dns: false,
                max_concurrent_sessions: 1,
                max_members: 10,
                max_channels: 5,
                max_message_history: 5_000,
                storage_bytes: 100 * 1024 * 1024,
                // FR-32 — Free gets 4 video participants (operator decision,
                // 2026-08-29). It was 0, i.e. "Free has no conferencing at
                // all", which was never enforced — so Free tenants have always
                // been able to hold calls, and enforcing 0 would have taken
                // that away rather than held a line. 4 is a real cap that the
                // gate can enforce without removing a capability people use.
                video_max_participants: 4,
                cloud_integrations: false,
                recordings: false,
            },
            Plan::Pro => PlanLimits {
                max_devices: 30,
                max_tunnel_clients: 30,
                overlay_mesh: true,
                exit_nodes: true,
                magic_dns: true,
                max_concurrent_sessions: 5,
                max_members: u32::MAX,
                max_channels: u32::MAX,
                max_message_history: -1,
                storage_bytes: 10 * 1024 * 1024 * 1024,
                video_max_participants: 10,
                cloud_integrations: true,
                recordings: false,
            },
            Plan::Business | Plan::Enterprise => PlanLimits {
                max_devices: 300,
                max_tunnel_clients: 300,
                overlay_mesh: true,
                exit_nodes: true,
                magic_dns: true,
                max_concurrent_sessions: u32::MAX,
                max_members: u32::MAX,
                max_channels: u32::MAX,
                max_message_history: -1,
                storage_bytes: 100 * 1024 * 1024 * 1024,
                video_max_participants: 100,
                cloud_integrations: true,
                recordings: true,
            },
        }
    }

    pub fn price_monthly_cents(&self) -> u32 {
        match self {
            Plan::Free => 0,
            Plan::Pro => 800,
            Plan::Business => 1600,
            Plan::Enterprise => 1600,
        }
    }
}
