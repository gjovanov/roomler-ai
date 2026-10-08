// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P1g — which organizations agent sessions serve (`hive.tenants`).
//!
//! The module switch (`[modules] hive`) is all or nothing for a server. A
//! hosted server opens the pillar to a test organization first, so the
//! switch has a second dial: a list of organizations. Every other one is
//! answered as if the module were not there for it — its routes are not
//! found, its sessions cannot be viewed, and its devices are told to stop
//! what they still run for it.

use std::collections::HashSet;

use bson::oid::ObjectId;
use tracing::{error, info};

/// The organizations agent sessions serve: every one, or a set.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TenantScope(Option<HashSet<ObjectId>>);

impl TenantScope {
    /// Every organization.
    pub fn everyone() -> Self {
        Self(None)
    }

    /// `hive.tenants` read: empty or `*` is every organization; otherwise the
    /// ids listed. An entry that is not an organization id is said in the log
    /// and matches nothing, so a typo shuts the pillar for the org it meant
    /// rather than opening it to every other.
    pub fn parse(raw: &str) -> Self {
        let entries: Vec<&str> = raw
            .split(|c: char| c == ',' || c.is_whitespace())
            .filter(|e| !e.is_empty())
            .collect();
        if entries.is_empty() || entries.contains(&"*") {
            return Self::everyone();
        }
        let mut ids = HashSet::new();
        for e in entries {
            match ObjectId::parse_str(e) {
                Ok(id) => {
                    ids.insert(id);
                }
                Err(_) => {
                    error!(
                        entry = e,
                        "hive.tenants: not an organization id — it matches nothing"
                    )
                }
            }
        }
        Self(Some(ids))
    }

    /// Whether agent sessions serve `tenant`.
    pub fn serves(&self, tenant: ObjectId) -> bool {
        self.0.as_ref().is_none_or(|ids| ids.contains(&tenant))
    }

    /// Said once at boot, so the log shows whom the pillar is open to.
    pub fn log(&self) {
        match &self.0 {
            None => info!("hive: agent sessions serve every organization"),
            Some(ids) => info!(
                organizations = ids.len(),
                "hive: agent sessions serve only the organizations in hive.tenants"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "65f0000000000000000000a1";
    const B: &str = "65f0000000000000000000b2";

    fn oid(s: &str) -> ObjectId {
        ObjectId::parse_str(s).unwrap()
    }

    #[test]
    fn empty_or_a_star_is_every_organization() {
        for raw in ["", "  ", ",", "*", &format!("{A},*")] {
            let s = TenantScope::parse(raw);
            assert_eq!(s, TenantScope::everyone(), "{raw:?}");
            assert!(s.serves(oid(B)));
        }
    }

    #[test]
    fn a_list_serves_only_its_organizations() {
        let s = TenantScope::parse(&format!(" {A} , "));
        assert!(s.serves(oid(A)));
        assert!(!s.serves(oid(B)));
        let both = TenantScope::parse(&format!("{A},{B}"));
        assert!(both.serves(oid(A)) && both.serves(oid(B)));
        let spaced = TenantScope::parse(&format!("{A} {B}"));
        assert!(spaced.serves(oid(B)));
    }

    #[test]
    fn an_entry_that_is_no_id_matches_nothing_and_opens_nothing() {
        // A typo for the one org meant: that org is shut, nobody is opened.
        let s = TenantScope::parse("65f0000000000000000000aZ");
        assert!(!s.serves(oid(A)));
        assert!(!s.serves(oid(B)));
        let mixed = TenantScope::parse(&format!("not-an-id,{A}"));
        assert!(mixed.serves(oid(A)));
        assert!(!mixed.serves(oid(B)));
    }
}
