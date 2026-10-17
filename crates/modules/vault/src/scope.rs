// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! Which organizations the vault serves (`vault.tenants`).
//!
//! The same dial as `hive.tenants` (FR-90 P1g), with the same rules: empty
//! or `*` is every organization, a list is only those, and an entry that is
//! not an organization id matches nothing — a typo shuts the vault for the
//! org it meant rather than opening it to every other. Every other
//! organization is answered as if the module were not there for it.
//!
//! A copy of hive's `TenantScope`, not a call into it: the vault depends on
//! core alone in P3-1, and an edge to hive for one parser would be the
//! wrong reason to have one.

use std::collections::HashSet;

use bson::oid::ObjectId;
use tracing::{error, info};

/// The organizations the vault serves: every one, or a set.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TenantScope(Option<HashSet<ObjectId>>);

impl TenantScope {
    /// Every organization.
    pub fn everyone() -> Self {
        Self(None)
    }

    /// `vault.tenants` read: empty or `*` is every organization; otherwise
    /// the ids listed.
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
                        "vault.tenants: not an organization id — it matches nothing"
                    )
                }
            }
        }
        Self(Some(ids))
    }

    /// Whether the vault serves `tenant`.
    pub fn serves(&self, tenant: ObjectId) -> bool {
        self.0.as_ref().is_none_or(|ids| ids.contains(&tenant))
    }

    /// Said once at boot, so the log shows whom the vault is open to.
    pub fn log(&self) {
        match &self.0 {
            None => info!("vault: serves every organization"),
            Some(ids) => info!(
                organizations = ids.len(),
                "vault: serves only the organizations in vault.tenants"
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
        let spaced = TenantScope::parse(&format!("{A} {B}"));
        assert!(spaced.serves(oid(A)) && spaced.serves(oid(B)));
    }

    #[test]
    fn an_entry_that_is_no_id_matches_nothing_and_opens_nothing() {
        let s = TenantScope::parse("65f0000000000000000000aZ");
        assert!(!s.serves(oid(A)));
        assert!(!s.serves(oid(B)));
        let mixed = TenantScope::parse(&format!("not-an-id,{A}"));
        assert!(mixed.serves(oid(A)));
        assert!(!mixed.serves(oid(B)));
    }
}
