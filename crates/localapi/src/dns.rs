// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! #1363 — what MagicDNS's status says, decided once for every client.
//!
//! `roomler status` and the companion's DNS section used to print `active`
//! from three inputs that each record an ATTEMPT: the resolver bound its
//! socket, the OS steer was written, and (Linux) a static read found no bypass.
//! None of them is the outcome, and the field showed it: a policy-managed NRPT
//! discards a written rule, and the status still said `active` while nothing
//! resolved (#1363, comments 3 and 4).
//!
//! The daemon now MEASURES the outcome: once a minute it resolves
//! `_roomler-probe.<magic domain>` through the OS, a name only an overlay
//! resolver answers (FR-72 P6's ladder probe, now shared with the status). The
//! result travels as [`DnsProbeStatus`], and [`DnsStatus::health`] turns it into
//! the words, so the CLI and the companion can never disagree about them.
//!
//! ⚠️ `active` means the probe resolved to THIS node's own overlay address,
//! nothing less. A daemon that predates the measurement sends no `probe`, and
//! then the words are today's, minus the claim: an unmeasured steer is
//! `steered`, never `active`.

use serde::{Deserialize, Serialize};

use crate::DnsStatus;

/// How often the daemon measures the OS path, and FR-72 P6's ladder cadence:
/// one lookup serves both. A reading older than [`DNS_PROBE_STALE_MS`] is not a
/// current measurement.
pub const DNS_PROBE_EVERY_MS: u64 = 60_000;

/// A reading this old means the measurement stopped, and the status says so
/// rather than repeat it. Three missed rounds.
pub const DNS_PROBE_STALE_MS: u64 = 3 * DNS_PROBE_EVERY_MS;

/// #1363 — what the OS made of `_roomler-probe.<magic domain>` the last time
/// the daemon asked. Only an overlay resolver answers that name, and this
/// node's resolver answers it with this node's own overlay address.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct DnsProbeStatus {
    pub outcome: DnsProbeOutcome,
    /// When the reading was taken, in unix milliseconds. Absent while
    /// [`DnsProbeOutcome::Pending`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at_ms: Option<u64>,
    /// The address the OS returned, when it is not this node's own
    /// ([`DnsProbeOutcome::AnsweredElsewhere`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answered_by: Option<String>,
    /// Windows: why the OS does not reach the resolver, read from the NRPT
    /// after a probe that was not answered. It chooses the words and never
    /// decides the state: a rule the NRPT holds is still not an answer (FR-72).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<DnsUnreachedReason>,
}

/// The probe's result.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DnsProbeOutcome {
    /// Not measured yet.
    Pending,
    /// The OS resolved the probe name to this node's own overlay address: the
    /// host's lookups reach this node's resolver.
    Answered,
    /// The OS resolved it to another address: something other than this
    /// node's resolver answers the domain here (on WSL, the Windows host's
    /// resolver through its DNS proxy).
    AnsweredElsewhere,
    /// No address within the probe's timeout.
    Unanswered,
    /// A value this build does not know, from a newer daemon. Read as not
    /// measured, never as an answer.
    #[serde(other)]
    Unknown,
}

/// Windows: what the NRPT stores say about the magic domain's rule, read after
/// a probe that was not answered.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DnsUnreachedReason {
    /// The policy store exists, so Windows ignores every local rule, and it
    /// holds none for the domain. The daemon cannot fix this: the host's
    /// policy owns the NRPT.
    NrptPolicyManaged,
    /// No NRPT store holds a rule for the domain any more.
    NrptRuleMissing,
    /// The store Windows reads (the policy store when there is one) holds a
    /// rule for the domain, and the host's own lookups still do not reach the
    /// resolver. Not "in effect": the registry cannot say that. A local rule on
    /// a policy-managed host can wait for a policy refresh, and only
    /// `Get-DnsClientNrptPolicy -Effective` shows what the DNS client uses.
    /// When it lists the rule, something below the NRPT takes the lookups (an
    /// enforced corporate DNS layer, `docs/magicdns.md`).
    NrptRulePresent,
    /// A value this build does not know, from a newer daemon.
    #[serde(other)]
    Unknown,
}

/// The state MagicDNS is in on this node, in the order an operator needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DnsHealth {
    /// The OS resolved the probe name to this node's own resolver.
    Active,
    /// The OS resolved it through a resolver that is not this node's.
    AnsweredElsewhere {
        by: Option<String>,
        bypass: Option<String>,
    },
    /// FR-72 P6's hosts-file rung serves the names; `cause` is why the DNS
    /// rung does not.
    HostsFile { cause: Option<String> },
    /// The resolver has no socket (it lost it, or never got one) and retries.
    ResolverBinding,
    /// The resolver is up, and the OS steer could not be written.
    SteerFailed,
    /// The probe was not answered: the OS does not reach the resolver.
    NotReached { why: Option<String> },
    /// The first measurement has not been taken.
    NotMeasured,
    /// The last reading is older than [`DNS_PROBE_STALE_MS`].
    Stale { age_s: u64 },
    /// A daemon older than #1363, which measures nothing: its own words,
    /// never `active`.
    Unmeasured(LegacyDns),
}

/// What a daemon older than #1363 can say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LegacyDns {
    ResolverDown,
    SteerFailed,
    Bypassed(String),
    Steered,
}

impl DnsStatus {
    /// The state these facts describe at `now_ms` (unix milliseconds, the
    /// reader's own clock; the daemon is on the same host).
    ///
    /// Precedence, most useful first: a hosts-file rung that serves the names,
    /// then a resolver without a socket (fresher than any reading), then what
    /// the probe measured.
    pub fn health(&self, now_ms: u64) -> DnsHealth {
        let Some(probe) = &self.probe else {
            return DnsHealth::Unmeasured(if !self.resolver_bound {
                LegacyDns::ResolverDown
            } else if !self.os_steer_active {
                LegacyDns::SteerFailed
            } else if let Some(why) = &self.os_steer_bypassed {
                LegacyDns::Bypassed(why.clone())
            } else {
                LegacyDns::Steered
            });
        };
        let not_reached_why = || {
            probe
                .reason
                .and_then(unreached_words)
                .map(str::to_string)
                .or_else(|| {
                    self.os_steer_bypassed
                        .as_deref()
                        .map(|b| format!("the steer is bypassed: {b}"))
                })
        };
        if self.hosts_fallback {
            let cause = if !self.resolver_bound {
                Some("the resolver is binding (retrying)".to_string())
            } else if !self.os_steer_active {
                // The NRPT's words describe a rule this daemon WROTE; with no
                // steer written they would say something that is not so.
                Some("the OS steer could not be written".to_string())
            } else {
                let why = not_reached_why();
                Some(match why {
                    Some(why) => format!("the OS does not reach the resolver: {why}"),
                    None => "the OS does not reach the resolver".to_string(),
                })
            };
            return DnsHealth::HostsFile { cause };
        }
        if !self.resolver_bound {
            return DnsHealth::ResolverBinding;
        }
        if let Some(at) = probe.at_ms
            && now_ms.saturating_sub(at) > DNS_PROBE_STALE_MS
        {
            return DnsHealth::Stale {
                age_s: now_ms.saturating_sub(at) / 1000,
            };
        }
        match probe.outcome {
            DnsProbeOutcome::Answered => DnsHealth::Active,
            DnsProbeOutcome::AnsweredElsewhere => DnsHealth::AnsweredElsewhere {
                by: probe.answered_by.clone(),
                bypass: self.os_steer_bypassed.clone(),
            },
            _ if !self.os_steer_active => DnsHealth::SteerFailed,
            DnsProbeOutcome::Unanswered => DnsHealth::NotReached {
                why: not_reached_why(),
            },
            DnsProbeOutcome::Pending | DnsProbeOutcome::Unknown => DnsHealth::NotMeasured,
        }
    }
}

/// Windows' reason, in words. `None` for a reason this build does not know.
fn unreached_words(reason: DnsUnreachedReason) -> Option<&'static str> {
    match reason {
        DnsUnreachedReason::NrptPolicyManaged => Some(
            "the NRPT rule is written but not in effect, because the NRPT is policy-managed on this host",
        ),
        DnsUnreachedReason::NrptRuleMissing => Some("the NRPT holds no rule for this domain"),
        DnsUnreachedReason::NrptRulePresent => Some(
            "the NRPT holds a rule for this domain, and the host's own lookups still do not reach the resolver",
        ),
        DnsUnreachedReason::Unknown => None,
    }
}

impl DnsHealth {
    /// The few words that go beside the domain: `magicdns <domain> (<these>, …)`.
    pub fn headline(&self) -> String {
        match self {
            Self::Active => "active".into(),
            Self::AnsweredElsewhere { .. } => "names resolve through another resolver".into(),
            Self::HostsFile { .. } => "names served from the hosts file".into(),
            Self::ResolverBinding => "resolver binding (retrying)".into(),
            Self::SteerFailed => "resolver up, OS steer FAILED".into(),
            Self::NotReached { .. } => "the OS does not reach the resolver".into(),
            Self::NotMeasured => "not measured yet".into(),
            Self::Stale { age_s } => format!("not measured for {} min", age_s / 60),
            Self::Unmeasured(LegacyDns::ResolverDown) => "resolver DOWN".into(),
            Self::Unmeasured(LegacyDns::SteerFailed) => "resolver up, OS steer FAILED".into(),
            Self::Unmeasured(LegacyDns::Bypassed(_)) => "steered but BYPASSED".into(),
            // An older daemon measured nothing: never `active`.
            Self::Unmeasured(LegacyDns::Steered) => "steered, not measured (older daemon)".into(),
        }
    }

    /// The reason, when there is one worth a second line.
    pub fn detail(&self) -> Option<String> {
        match self {
            Self::AnsweredElsewhere { by, bypass } => {
                let who = by.as_deref().unwrap_or("another resolver");
                Some(match bypass {
                    Some(b) => format!(
                        "{who} answered for this domain, not this node's resolver; the steer is bypassed: {b}"
                    ),
                    None => format!("{who} answered for this domain, not this node's resolver"),
                })
            }
            Self::HostsFile { cause } => cause.clone(),
            Self::NotReached { why } => why.clone(),
            Self::Unmeasured(LegacyDns::Bypassed(why)) => Some(format!(
                "{why}: this host's own lookups never reach the steer"
            )),
            _ => None,
        }
    }

    /// One line, for a surface with room for one: `<headline>: <detail>`.
    pub fn one_line(&self) -> String {
        match self.detail() {
            Some(d) => format!("{}: {d}", self.headline()),
            None => self.headline(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_800_000_000_000;

    fn status(probe: Option<DnsProbeStatus>) -> DnsStatus {
        DnsStatus {
            magic_domain: "myorg.roomler.net".into(),
            resolver_bound: true,
            os_steer_active: true,
            upstream: "1.1.1.1:53".into(),
            answer_aaaa: true,
            os_steer_bypassed: None,
            probe,
            hosts_fallback: false,
        }
    }

    fn reading(outcome: DnsProbeOutcome) -> DnsProbeStatus {
        DnsProbeStatus {
            outcome,
            at_ms: (outcome != DnsProbeOutcome::Pending).then_some(NOW - 5_000),
            answered_by: None,
            reason: None,
        }
    }

    #[test]
    fn answered_is_the_only_state_that_reads_active() {
        let s = status(Some(reading(DnsProbeOutcome::Answered)));
        assert_eq!(s.health(NOW), DnsHealth::Active);
        assert_eq!(s.health(NOW).headline(), "active");
        assert_eq!(s.health(NOW).detail(), None);
    }

    #[test]
    fn not_answered_with_a_policy_managed_nrpt_names_the_policy() {
        let mut r = reading(DnsProbeOutcome::Unanswered);
        r.reason = Some(DnsUnreachedReason::NrptPolicyManaged);
        let h = status(Some(r)).health(NOW);
        assert_eq!(h.headline(), "the OS does not reach the resolver");
        assert_eq!(
            h.detail().as_deref(),
            Some(
                "the NRPT rule is written but not in effect, because the NRPT is policy-managed on this host"
            )
        );
    }

    #[test]
    fn not_answered_with_the_rule_present_says_the_lookups_still_do_not_arrive() {
        let mut r = reading(DnsProbeOutcome::Unanswered);
        r.reason = Some(DnsUnreachedReason::NrptRulePresent);
        let h = status(Some(r)).health(NOW);
        assert_eq!(h.headline(), "the OS does not reach the resolver");
        assert_eq!(
            h.detail().as_deref(),
            Some(
                "the NRPT holds a rule for this domain, and the host's own lookups still do not reach the resolver"
            )
        );
        // The registry cannot say a rule is in effect, so the words never do.
        assert!(!h.detail().unwrap().contains("in effect"));
        let mut r = reading(DnsProbeOutcome::Unanswered);
        r.reason = Some(DnsUnreachedReason::NrptRuleMissing);
        assert_eq!(
            status(Some(r)).health(NOW).detail().as_deref(),
            Some("the NRPT holds no rule for this domain")
        );
    }

    #[test]
    fn not_answered_with_a_linux_bypass_gives_the_bypass() {
        let mut s = status(Some(reading(DnsProbeOutcome::Unanswered)));
        s.os_steer_bypassed =
            Some("/mnt/wsl/resolv.conf asks 10.255.255.254, and nsswitch has no `resolve`".into());
        let h = s.health(NOW);
        assert_eq!(h.headline(), "the OS does not reach the resolver");
        assert_eq!(
            h.detail().as_deref(),
            Some(
                "the steer is bypassed: /mnt/wsl/resolv.conf asks 10.255.255.254, and nsswitch has no `resolve`"
            )
        );
    }

    #[test]
    fn not_answered_with_no_known_reason_says_only_that() {
        let h = status(Some(reading(DnsProbeOutcome::Unanswered))).health(NOW);
        assert_eq!(h, DnsHealth::NotReached { why: None });
        assert_eq!(h.one_line(), "the OS does not reach the resolver");
    }

    #[test]
    fn answered_by_another_resolver_is_not_active_and_says_who() {
        // WSL in the same org as its Windows host: the Windows node's resolver
        // answers through the host's DNS proxy, so names resolve, and not
        // through this node's resolver.
        let mut r = reading(DnsProbeOutcome::AnsweredElsewhere);
        r.answered_by = Some("100.65.4.2".into());
        let mut s = status(Some(r));
        s.os_steer_bypassed = Some("/mnt/wsl/resolv.conf asks 10.255.255.254".into());
        let h = s.health(NOW);
        assert_ne!(h, DnsHealth::Active);
        assert_eq!(h.headline(), "names resolve through another resolver");
        assert_eq!(
            h.detail().as_deref(),
            Some(
                "100.65.4.2 answered for this domain, not this node's resolver; the steer is bypassed: /mnt/wsl/resolv.conf asks 10.255.255.254"
            )
        );
    }

    #[test]
    fn a_resolver_without_its_socket_reads_binding_over_a_stale_answer() {
        // The last reading said answered, then the resolver lost its socket:
        // the bound signal is fresher than any reading.
        let mut s = status(Some(reading(DnsProbeOutcome::Answered)));
        s.resolver_bound = false;
        assert_eq!(s.health(NOW), DnsHealth::ResolverBinding);
        assert_eq!(s.health(NOW).headline(), "resolver binding (retrying)");
    }

    #[test]
    fn the_hosts_file_rung_says_names_are_served_and_why() {
        let mut r = reading(DnsProbeOutcome::Unanswered);
        r.reason = Some(DnsUnreachedReason::NrptRulePresent);
        let mut s = status(Some(r));
        s.hosts_fallback = true;
        let h = s.health(NOW);
        assert_eq!(h.headline(), "names served from the hosts file");
        assert_eq!(
            h.detail().as_deref(),
            Some(
                "the OS does not reach the resolver: the NRPT holds a rule for this domain, and the host's own lookups still do not reach the resolver"
            )
        );
        // With no steer written, the NRPT's words would describe a rule this
        // daemon never wrote: the cause is the steer.
        s.os_steer_active = false;
        assert_eq!(
            s.health(NOW).detail().as_deref(),
            Some("the OS steer could not be written")
        );
        // The rung also serves a host whose resolver is rebinding.
        s.resolver_bound = false;
        assert_eq!(
            s.health(NOW).detail().as_deref(),
            Some("the resolver is binding (retrying)")
        );
    }

    #[test]
    fn not_measured_yet_until_the_first_probe() {
        let h = status(Some(reading(DnsProbeOutcome::Pending))).health(NOW);
        assert_eq!(h, DnsHealth::NotMeasured);
        assert_eq!(h.headline(), "not measured yet");
    }

    #[test]
    fn a_steer_that_failed_to_write_is_named_unless_the_os_answers_anyway() {
        let mut s = status(Some(reading(DnsProbeOutcome::Unanswered)));
        s.os_steer_active = false;
        assert_eq!(s.health(NOW), DnsHealth::SteerFailed);
        s.probe = Some(reading(DnsProbeOutcome::Pending));
        assert_eq!(s.health(NOW), DnsHealth::SteerFailed);
        // A rule the host's own policy wrote can still reach this resolver:
        // the outcome is what the words follow.
        s.probe = Some(reading(DnsProbeOutcome::Answered));
        assert_eq!(s.health(NOW), DnsHealth::Active);
    }

    #[test]
    fn a_reading_that_stopped_is_not_repeated_as_current() {
        let mut r = reading(DnsProbeOutcome::Answered);
        r.at_ms = Some(NOW - DNS_PROBE_STALE_MS - 1_000);
        let h = status(Some(r)).health(NOW);
        assert_eq!(h, DnsHealth::Stale { age_s: 181 });
        assert_eq!(h.headline(), "not measured for 3 min");
        // At the bound it is still current.
        let mut r = reading(DnsProbeOutcome::Answered);
        r.at_ms = Some(NOW - DNS_PROBE_STALE_MS);
        assert_eq!(status(Some(r)).health(NOW), DnsHealth::Active);
    }

    #[test]
    fn an_older_daemons_json_keeps_its_words_and_never_claims_an_answer() {
        // Verbatim the shape a daemon before #1363 sends: no `probe`, no
        // `hosts_fallback`.
        let old: DnsStatus = serde_json::from_str(
            r#"{"magic_domain":"vmtest.roomler.ai","resolver_bound":true,
                "os_steer_active":true,"upstream":"192.0.2.53:53","answer_aaaa":true}"#,
        )
        .unwrap();
        assert_eq!(old.probe, None);
        assert!(!old.hosts_fallback);
        let h = old.health(NOW);
        assert_eq!(h, DnsHealth::Unmeasured(LegacyDns::Steered));
        assert_ne!(h.headline(), "active");
        assert_eq!(h.headline(), "steered, not measured (older daemon)");

        let words = |bound: bool, steered: bool, bypassed: Option<&str>| {
            let mut s = old.clone();
            s.resolver_bound = bound;
            s.os_steer_active = steered;
            s.os_steer_bypassed = bypassed.map(str::to_string);
            let h = s.health(NOW);
            (h.headline(), h.detail())
        };
        assert_eq!(words(false, false, None).0, "resolver DOWN");
        assert_eq!(words(true, false, None).0, "resolver up, OS steer FAILED");
        let (head, detail) = words(true, true, Some("/etc/resolv.conf asks 192.0.2.1"));
        assert_eq!(head, "steered but BYPASSED");
        assert_eq!(
            detail.as_deref(),
            Some("/etc/resolv.conf asks 192.0.2.1: this host's own lookups never reach the steer")
        );
    }

    #[test]
    fn a_newer_daemons_values_read_as_not_measured_never_as_an_answer() {
        let s: DnsStatus = serde_json::from_str(
            r#"{"magic_domain":"d","resolver_bound":true,"os_steer_active":true,
                "upstream":"1.1.1.1:53","answer_aaaa":true,
                "probe":{"outcome":"answered_by_carrier_pigeon","at_ms":1,"reason":"policy_from_mars"}}"#,
        )
        .unwrap();
        let p = s.probe.as_ref().unwrap();
        assert_eq!(p.outcome, DnsProbeOutcome::Unknown);
        assert_eq!(p.reason, Some(DnsUnreachedReason::Unknown));
        assert_eq!(s.health(2_000), DnsHealth::NotMeasured);
    }

    #[test]
    fn the_new_fields_are_additive_on_the_wire() {
        // Nothing new is written while there is nothing to say, so an older
        // reader sees exactly today's object.
        let mut s = status(None);
        let json = serde_json::to_value(&s).unwrap();
        assert!(json.get("probe").is_none());
        assert!(json.get("hosts_fallback").is_none());
        s.probe = Some(reading(DnsProbeOutcome::Pending));
        let json = serde_json::to_value(&s).unwrap();
        assert_eq!(json["probe"], serde_json::json!({ "outcome": "pending" }));
        s.hosts_fallback = true;
        s.probe = Some(DnsProbeStatus {
            outcome: DnsProbeOutcome::Unanswered,
            at_ms: Some(7),
            answered_by: None,
            reason: Some(DnsUnreachedReason::NrptPolicyManaged),
        });
        let back: DnsStatus = serde_json::from_value(serde_json::to_value(&s).unwrap()).unwrap();
        assert_eq!(back, s);
        assert_eq!(
            serde_json::to_value(&s).unwrap()["probe"]["reason"],
            "nrpt_policy_managed"
        );
    }
}
