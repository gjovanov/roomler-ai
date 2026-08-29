// SPDX-License-Identifier: AGPL-3.0-only
//! FR-32 — the single decision point for plan-limit checks.
//!
//! `Plan::limits()` publishes fourteen numbers through `GET /api/stripe/plans`
//! and we take money against them. Before FR-32 exactly three were read back:
//! `max_devices` (twice) and `max_tunnel_clients` (once), each as a hand-rolled
//! count-compare-format block. The other eleven were advertised and enforced
//! nowhere.
//!
//! Eleven more copies of that block would be eleven chances to forget the mode
//! check, the log line, or the record. So the comparison lives here instead —
//! the same argument the SSH work settled on, where `decide` returns
//! `Result<Granted, SshDenyReason>` and `dispatch` records *both* arms in one
//! place: the refusals are the load-bearing rows, so recording them cannot be
//! per-call-site.

use roomler_ai_db::models::{Plan, PlanEnforcement, PlanLimits};

/// Which advertised limit a check is about.
///
/// The `match` in [`Limit::describe`] is exhaustive, so adding a field to
/// [`PlanLimits`] fails to compile until someone decides whether it is a gate.
/// Same structural trick as `RpcCap::wire()` — the compiler asks the question
/// that review would otherwise have to remember to ask.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Limit {
    // ── Established before FR-32: always enforced ───────────────────
    MaxDevices,
    MaxTunnelClients,
    // ── Wired by FR-32: subject to `PlanEnforcement` ────────────────
    MaxMembers,
    MaxChannels,
    StorageBytes,
    VideoMaxParticipants,
    MaxConcurrentSessions,
    ExitNodes,
    MagicDns,
    Recordings,
    CloudIntegrations,
}

impl Limit {
    /// Was this limit already enforced before FR-32?
    ///
    /// ⚠ **Load-bearing.** `PlanEnforcement` exists to stage *new* enforcement
    /// safely, not to hand anyone a switch that turns the device cap off. An
    /// established limit therefore ignores the mode and always refuses.
    ///
    /// Without this, re-pointing the three existing call sites through
    /// [`check`] would silently downgrade them to warnings the moment this
    /// lands, because the mode defaults to [`PlanEnforcement::Warn`] — a
    /// billing regression introduced by a refactor that reads as a cleanup.
    pub fn is_established(self) -> bool {
        matches!(self, Limit::MaxDevices | Limit::MaxTunnelClients)
    }

    /// The limit's value for a plan, plus the noun used in the refusal message.
    ///
    /// `None` = the plan does not cap this limit (an unlimited count, or an
    /// enabled boolean feature).
    pub fn describe(self, limits: &PlanLimits) -> (Option<u64>, &'static str) {
        match self {
            Limit::MaxDevices => (count_cap(limits.max_devices), "devices"),
            Limit::MaxTunnelClients => (count_cap(limits.max_tunnel_clients), "tunnel clients"),
            Limit::MaxMembers => (count_cap(limits.max_members), "members"),
            Limit::MaxChannels => (count_cap(limits.max_channels), "channels"),
            Limit::StorageBytes => (Some(limits.storage_bytes), "bytes of storage"),
            Limit::VideoMaxParticipants => (
                count_cap(limits.video_max_participants),
                "call participants",
            ),
            Limit::MaxConcurrentSessions => (
                count_cap(limits.max_concurrent_sessions),
                "concurrent remote-control sessions",
            ),
            // Boolean features: `false` caps at zero, `true` does not cap.
            Limit::ExitNodes => (bool_cap(limits.exit_nodes), "exit nodes"),
            Limit::MagicDns => (bool_cap(limits.magic_dns), "MagicDNS"),
            Limit::Recordings => (bool_cap(limits.recordings), "recordings"),
            Limit::CloudIntegrations => (bool_cap(limits.cloud_integrations), "cloud integrations"),
        }
    }
}

/// A boolean feature as a cap: off means zero allowed, on means uncapped.
fn bool_cap(enabled: bool) -> Option<u64> {
    if enabled { None } else { Some(0) }
}

/// A counted cap, honouring this codebase's **unlimited sentinel**.
///
/// ⚠ `PlanLimits` spells "unlimited" as `u32::MAX` (`Pro.max_members`,
/// `Pro.max_channels`, `Business.max_concurrent_sessions`, …), not as an
/// `Option`. Comparing against it as if it were a real number is wrong twice
/// over: it would refuse a tenant that the plan matrix calls unlimited, and it
/// would file denial records with a bogus `max` of 4 294 967 295 — poisoning
/// exactly the data P2 and P3 are supposed to read.
///
/// A unit test pins this; it is the reason the test exists.
fn count_cap(v: u32) -> Option<u64> {
    if v == u32::MAX { None } else { Some(v as u64) }
}

/// A limit that was exceeded. Returned only when the caller must refuse.
#[derive(Debug, Clone)]
pub struct QuotaDenial {
    pub limit: Limit,
    pub plan: Plan,
    pub used: u64,
    pub max: u64,
    pub noun: &'static str,
}

impl QuotaDenial {
    /// Is this a disabled *feature* rather than an exhausted *count*?
    ///
    /// A boolean feature that the plan does not include caps at zero, so the
    /// counting wording ("0 of 0 recordings used") would be nonsense. The two
    /// cases need different sentences, and the distinction is `max == 0`.
    pub fn is_feature_gate(&self) -> bool {
        self.max == 0
    }

    /// The message shown to the caller. Deliberately the same shape the three
    /// pre-FR-32 sites already used, so P0 is observable-behaviour-neutral.
    pub fn message(&self) -> String {
        if self.is_feature_gate() {
            return format!(
                "{} {} not available on the {:?} plan. Upgrade to enable.",
                capitalise(self.noun),
                if self.noun.ends_with('s') {
                    "are"
                } else {
                    "is"
                },
                self.plan,
            );
        }
        format!(
            "{} limit reached for the {:?} plan ({} of {} {} used). \
             Upgrade the plan or remove one first.",
            capitalise(self.noun),
            self.plan,
            self.used,
            self.max,
            self.noun,
        )
    }
}

fn capitalise(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// Check one limit for a tenant.
///
/// `used` is the count *before* the operation the caller is about to perform,
/// so the comparison is `used >= max` — matching the three pre-FR-32 sites,
/// where a tenant at exactly its cap is refused the next one.
///
/// Returns `Ok(())` when the operation may proceed. That includes the
/// [`PlanEnforcement::Warn`] case, which records the denial and lets the
/// request through — the observe phase must produce data without refusing.
pub fn check(
    plan: Plan,
    mode: PlanEnforcement,
    limit: Limit,
    used: u64,
) -> Result<(), QuotaDenial> {
    // "May I add one more?" is the delta form with delta = 1.
    check_delta(plan, mode, limit, used, 1)
}

/// Check whether `delta` more units fit under the limit.
///
/// ⚠ The counted limits add ONE thing at a time, so `used >= max` is the right
/// test for them. `storage_bytes` does not: a tenant at 99 MB of 100 MB must be
/// refused a 10 MB upload and ALLOWED a 100 KB one, which `used >= max` cannot
/// express — it would accept both, right up until the quota was already blown,
/// and then reject a 1-byte file. The size is only knowable after the multipart
/// body is read, so the check is inherently "current + incoming", not "current".
///
/// Refuses when `current + delta > max`, so a file that exactly fills the quota
/// still fits.
pub fn check_delta(
    plan: Plan,
    mode: PlanEnforcement,
    limit: Limit,
    current: u64,
    delta: u64,
) -> Result<(), QuotaDenial> {
    check_inner(plan, mode, limit, current, delta)
}

fn check_inner(
    plan: Plan,
    mode: PlanEnforcement,
    limit: Limit,
    used: u64,
    delta: u64,
) -> Result<(), QuotaDenial> {
    // `Off` means no check runs — but never for a limit that was already
    // enforced before FR-32, which the mode was never meant to reach.
    if matches!(mode, PlanEnforcement::Off) && !limit.is_established() {
        return Ok(());
    }

    let limits = plan.limits();
    let (Some(max), noun) = limit.describe(&limits) else {
        return Ok(()); // uncapped for this plan
    };

    // Saturating, so a pathological delta cannot wrap into "fits".
    if used.saturating_add(delta) <= max {
        return Ok(());
    }

    // An established limit ignores the mode entirely — see `is_established`.
    let enforcing = limit.is_established() || matches!(mode, PlanEnforcement::Enforce);
    let denial = QuotaDenial {
        limit,
        plan,
        used,
        max,
        noun,
    };

    // Every denial is recorded here, in both arms, exactly once. A gate that
    // forgets to log is a gate whose impact cannot be measured before P2 flips
    // it, which is the whole reason the observe phase exists.
    tracing::warn!(
        limit = ?limit,
        plan = ?denial.plan,
        used = denial.used,
        max = denial.max,
        mode = ?mode,
        enforced = enforcing,
        "plan limit exceeded"
    );

    if enforcing { Err(denial) } else { Ok(()) }
}

/// Gate a boolean plan feature.
///
/// The counted form of [`check`] already models this — a feature the plan does
/// not include caps at zero, so `used = 0` trips it — but spelling that at each
/// call site would mean writing a bare `0` whose meaning is not local. This
/// names it instead.
pub fn require_feature(plan: Plan, mode: PlanEnforcement, limit: Limit) -> Result<(), QuotaDenial> {
    check(plan, mode, limit, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Free: max_members = 10, max_devices = 3, recordings = false.

    #[test]
    fn under_the_cap_always_passes() {
        for mode in [
            PlanEnforcement::Off,
            PlanEnforcement::Warn,
            PlanEnforcement::Enforce,
        ] {
            assert!(check(Plan::Free, mode, Limit::MaxMembers, 9).is_ok());
        }
    }

    #[test]
    fn warn_records_but_does_not_refuse() {
        // At the cap: the observe phase must let this through.
        assert!(check(Plan::Free, PlanEnforcement::Warn, Limit::MaxMembers, 10).is_ok());
    }

    #[test]
    fn enforce_refuses_at_the_cap() {
        let d = check(Plan::Free, PlanEnforcement::Enforce, Limit::MaxMembers, 10)
            .expect_err("a Free tenant at 10 members must be refused under Enforce");
        assert_eq!(d.used, 10);
        assert_eq!(d.max, 10);
        assert_eq!(d.limit, Limit::MaxMembers);
    }

    #[test]
    fn off_disables_a_newly_wired_limit() {
        assert!(check(Plan::Free, PlanEnforcement::Off, Limit::MaxMembers, 999).is_ok());
    }

    /// The regression this whole `is_established` distinction exists to stop:
    /// P0 re-points the device cap through `check`, and the mode defaults to
    /// `Warn`. If the mode applied to it, shipping P0 would stop enforcing the
    /// device limit fleet-wide — a billing regression that reads as a cleanup.
    #[test]
    fn an_established_limit_ignores_the_mode() {
        for mode in [
            PlanEnforcement::Off,
            PlanEnforcement::Warn,
            PlanEnforcement::Enforce,
        ] {
            assert!(
                check(Plan::Free, mode, Limit::MaxDevices, 3).is_err(),
                "max_devices must refuse under {mode:?} — it was enforced before FR-32"
            );
            assert!(
                check(Plan::Free, mode, Limit::MaxTunnelClients, 3).is_err(),
                "max_tunnel_clients must refuse under {mode:?} — it was enforced before FR-32"
            );
        }
    }

    /// A disabled feature must not be described with counting words. "0 of 0
    /// recordings used. Upgrade the plan or remove one first" tells a customer
    /// to remove something that does not exist.
    #[test]
    fn a_feature_gate_reads_as_a_feature_not_a_count() {
        let d = require_feature(Plan::Free, PlanEnforcement::Enforce, Limit::Recordings)
            .expect_err("Free does not include recordings");
        assert!(d.is_feature_gate());
        let m = d.message();
        assert_eq!(
            m,
            "Recordings are not available on the Free plan. Upgrade to enable."
        );
        assert!(
            !m.contains("0 of 0"),
            "feature gates must not use counting words: {m}"
        );

        // Singular noun agreement — "MagicDNS is", not "MagicDNS are".
        let d = require_feature(Plan::Free, PlanEnforcement::Enforce, Limit::MagicDns)
            .expect_err("Free does not include MagicDNS");
        assert_eq!(
            d.message(),
            "MagicDNS is not available on the Free plan. Upgrade to enable."
        );
    }

    /// A count that runs out is still described by counting.
    #[test]
    fn a_count_gate_keeps_the_counting_message() {
        let d = check(Plan::Free, PlanEnforcement::Enforce, Limit::MaxMembers, 10).unwrap_err();
        assert!(!d.is_feature_gate());
        assert!(
            d.message().contains("10 of 10 members used"),
            "{}",
            d.message()
        );
    }

    /// `require_feature` passes when the plan includes the feature.
    #[test]
    fn require_feature_passes_when_the_plan_includes_it() {
        assert!(require_feature(Plan::Pro, PlanEnforcement::Enforce, Limit::MagicDns).is_ok());
        assert!(require_feature(Plan::Pro, PlanEnforcement::Enforce, Limit::ExitNodes).is_ok());
        assert!(
            require_feature(Plan::Business, PlanEnforcement::Enforce, Limit::Recordings).is_ok()
        );
        // ...and Warn never refuses, even for a feature the plan lacks.
        assert!(require_feature(Plan::Free, PlanEnforcement::Warn, Limit::Recordings).is_ok());
    }

    /// The bug the delta form exists to prevent: a tenant near its storage
    /// quota must be judged on the SIZE of what they are uploading, not merely
    /// on whether they are already at the cap.
    #[test]
    fn storage_is_judged_on_the_incoming_size() {
        const MB: u64 = 1024 * 1024;
        // Free.storage_bytes is 100 MB; sit the tenant 1 MB under it.
        let used = 99 * MB;

        // A 100 KB file still fits...
        assert!(
            check_delta(
                Plan::Free,
                PlanEnforcement::Enforce,
                Limit::StorageBytes,
                used,
                100 * 1024
            )
            .is_ok()
        );
        // ...a 10 MB file does not.
        assert!(
            check_delta(
                Plan::Free,
                PlanEnforcement::Enforce,
                Limit::StorageBytes,
                used,
                10 * MB
            )
            .is_err()
        );
        // A file that EXACTLY fills the remaining quota is allowed — `>` not `>=`.
        assert!(
            check_delta(
                Plan::Free,
                PlanEnforcement::Enforce,
                Limit::StorageBytes,
                used,
                MB
            )
            .is_ok()
        );
        // One byte more is not.
        assert!(
            check_delta(
                Plan::Free,
                PlanEnforcement::Enforce,
                Limit::StorageBytes,
                used,
                MB + 1
            )
            .is_err()
        );
    }

    /// A pathological delta must not wrap into "fits".
    #[test]
    fn a_saturating_delta_cannot_overflow_into_success() {
        assert!(
            check_delta(
                Plan::Free,
                PlanEnforcement::Enforce,
                Limit::StorageBytes,
                u64::MAX,
                1
            )
            .is_err()
        );
    }

    /// `check` is the delta form with delta = 1, so the two must agree.
    #[test]
    fn check_is_delta_of_one() {
        for used in [0u64, 5, 9, 10, 11] {
            let a = check(
                Plan::Free,
                PlanEnforcement::Enforce,
                Limit::MaxMembers,
                used,
            )
            .is_err();
            let b = check_delta(
                Plan::Free,
                PlanEnforcement::Enforce,
                Limit::MaxMembers,
                used,
                1,
            )
            .is_err();
            assert_eq!(a, b, "disagreement at used={used}");
        }
    }

    #[test]
    fn a_disabled_boolean_feature_caps_at_zero() {
        // Free has recordings = false, so even the first one is refused.
        assert!(check(Plan::Free, PlanEnforcement::Enforce, Limit::Recordings, 0).is_err());
        // Business has recordings = true, so it is uncapped.
        assert!(
            check(
                Plan::Business,
                PlanEnforcement::Enforce,
                Limit::Recordings,
                500
            )
            .is_ok()
        );
    }

    /// `PlanLimits` spells "unlimited" as `u32::MAX`, not as an `Option`.
    /// Treating that as a real cap would refuse a tenant the plan matrix calls
    /// unlimited, and would file denial rows with a bogus max — corrupting the
    /// dataset P2/P3 exist to read. This test caught exactly that bug.
    #[test]
    fn the_unlimited_sentinel_is_not_a_cap() {
        // Pro: max_members = u32::MAX. Business: max_concurrent_sessions = u32::MAX.
        assert!(
            check(
                Plan::Pro,
                PlanEnforcement::Enforce,
                Limit::MaxMembers,
                u64::MAX
            )
            .is_ok()
        );
        assert!(
            check(
                Plan::Pro,
                PlanEnforcement::Enforce,
                Limit::MaxChannels,
                1_000_000
            )
            .is_ok()
        );
        assert!(
            check(
                Plan::Business,
                PlanEnforcement::Enforce,
                Limit::MaxConcurrentSessions,
                9_999
            )
            .is_ok()
        );
        // ...but a real cap on the same field still bites.
        assert!(check(Plan::Free, PlanEnforcement::Enforce, Limit::MaxMembers, 10).is_err());
    }
}
