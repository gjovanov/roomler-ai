// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-102 P2 — what counts as a VALUE event, in one place (spec §3d, D3).
//!
//! The digest's "active orgs", the nightly `growth_org_daily` fold, the
//! funnel's first-value step, the cohorts and the north star all read the
//! same four definitions from here, so no two views can disagree about
//! whether an org was active:
//!
//! | event | definition | source | module |
//! |---|---|---|---|
//! | `rc` | a remote-desktop session that reached Active and lasted ≥ [`VALUE_SESSION_SECS`] | `remote_sessions` | `remote` |
//! | `call` | a call with ≥ 2 participants at its peak | `call_sessions` | `conference` |
//! | `tunnel` | a tunnel flow accepted (`tunnel_audit.kind = tcp_accept`) | `tunnel_audit` | `network` |
//! | `ssh` | an SSH grant issued (`ssh_audit` without `denied`) | `ssh_audit` | `network` |
//!
//! An `exec` run is counted on the org-day but is not a value event: it is
//! an operator's diagnostic, not the product doing its job for a user.
//!
//! A module that is not mounted cannot produce its events; a view says so
//! with the module's name ([`unmounted_reason`]) rather than reading the
//! absence as "none happened".

use bson::{Bson, Document, doc};

/// A remote-desktop session is a value event once it reached Active and
/// lasted this long.
pub const VALUE_SESSION_SECS: i64 = 30;
/// A call is a value event from this many participants at its peak.
pub const VALUE_CALL_PARTICIPANTS: i64 = 2;
/// The `tunnel_audit.kind` that is a flow.
pub const TUNNEL_FLOW_KIND: &str = "tcp_accept";

/// The four kinds, in report order. The wire spelling is `as_str`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ValueKind {
    Rc,
    Call,
    Tunnel,
    Ssh,
}

impl ValueKind {
    pub const ALL: [ValueKind; 4] = [
        ValueKind::Rc,
        ValueKind::Call,
        ValueKind::Tunnel,
        ValueKind::Ssh,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ValueKind::Rc => "rc",
            ValueKind::Call => "call",
            ValueKind::Tunnel => "tunnel",
            ValueKind::Ssh => "ssh",
        }
    }

    /// The module whose rows the event is read from.
    pub fn module(self) -> &'static str {
        match self {
            ValueKind::Rc => "remote",
            ValueKind::Call => "conference",
            ValueKind::Tunnel | ValueKind::Ssh => "network",
        }
    }
}

/// The modules that produce value events, in report order.
pub const VALUE_MODULES: [&str; 3] = ["remote", "conference", "network"];

pub fn rc_session_is_value(started_ms: i64, ended_ms: i64) -> bool {
    ended_ms - started_ms >= VALUE_SESSION_SECS * 1000
}

pub fn call_is_value(peak_participants: i64) -> bool {
    peak_participants >= VALUE_CALL_PARTICIPANTS
}

/// `tunnel_audit` rows that are flows, with `at` in `range`.
pub fn tunnel_flow_filter(range: Document) -> Document {
    doc! { "kind": TUNNEL_FLOW_KIND, "at": range }
}

/// `ssh_audit` rows that issued a grant — `denied` absent or null (the
/// writer skips the field when there is no refusal) — with `at` in `range`.
pub fn ssh_session_filter(range: Document) -> Document {
    doc! { "denied": Bson::Null, "at": range }
}

/// `exec_audit` rows whose command ran, with `at` in `range`.
pub fn exec_run_filter(range: Document) -> Document {
    doc! { "denied": Bson::Null, "at": range }
}

/// Which value-producing modules are mounted, in report order — the
/// `basis` every view reports beside a value count.
pub fn value_basis(mounted: &[&str]) -> Vec<String> {
    VALUE_MODULES
        .iter()
        .filter(|m| mounted.contains(m))
        .map(|m| m.to_string())
        .collect()
}

/// The reason a value count is not measured: the value modules this server
/// does not mount, named.
pub fn unmounted_reason(basis: &[String]) -> String {
    let missing: Vec<&str> = VALUE_MODULES
        .iter()
        .copied()
        .filter(|m| !basis.iter().any(|b| b == m))
        .collect();
    match missing.len() {
        0 => "every value module is mounted".to_string(),
        1 => format!("{} module not mounted", missing[0]),
        n => format!(
            "{} and {} modules not mounted",
            missing[..n - 1].join(", "),
            missing[n - 1]
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_four_definitions_hold_at_their_edges() {
        assert!(rc_session_is_value(0, 30_000));
        assert!(!rc_session_is_value(0, 29_999));
        assert!(!rc_session_is_value(0, 0));
        assert!(call_is_value(2));
        assert!(!call_is_value(1));
        assert!(!call_is_value(0));
        let f = tunnel_flow_filter(doc! { "$gte": 1 });
        assert_eq!(f.get_str("kind").unwrap(), "tcp_accept");
        let s = ssh_session_filter(doc! { "$gte": 1 });
        assert_eq!(s.get("denied"), Some(&Bson::Null), "absent OR null");
        assert!(exec_run_filter(doc! {}).contains_key("denied"));
    }

    #[test]
    fn kinds_spell_their_wire_names_and_modules() {
        for k in ValueKind::ALL {
            assert!(VALUE_MODULES.contains(&k.module()));
        }
        assert_eq!(ValueKind::Tunnel.as_str(), "tunnel");
        assert_eq!(ValueKind::Ssh.module(), "network");
        let mut sorted = ValueKind::ALL.to_vec();
        sorted.sort();
        assert_eq!(
            sorted,
            ValueKind::ALL.to_vec(),
            "report order is sort order"
        );
    }

    #[test]
    fn the_basis_names_what_is_mounted_and_the_reason_what_is_not() {
        assert_eq!(
            value_basis(&["fleet", "network", "remote", "chat"]),
            vec!["remote", "network"]
        );
        assert!(value_basis(&["fleet", "chat"]).is_empty());
        assert_eq!(
            unmounted_reason(&[]),
            "remote, conference and network modules not mounted"
        );
        assert_eq!(
            unmounted_reason(&["remote".into(), "network".into()]),
            "conference module not mounted"
        );
        assert_eq!(
            unmounted_reason(&["conference".into()]),
            "remote and network modules not mounted"
        );
    }
}
