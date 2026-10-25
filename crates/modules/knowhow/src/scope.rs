// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! Which organizations knowhow keeps a map for (`knowhow.tenants`).
//!
//! The same dial as `hive.tenants` (FR-90 P1g) and `vault.tenants`, with the
//! same rules: empty or `*` is every organization, a list is only those, and
//! an entry that is not an organization id matches nothing — a typo shuts
//! the map for the org it meant rather than opening it to every other. Every
//! other organization is answered as if the module were not there for it.
//!
//! A copy of the vault's `TenantScope`, not a call into it: knowhow depends
//! on core alone in P4-1, and an edge for one parser would be the wrong
//! reason to have one.

use std::collections::HashSet;

use bson::oid::ObjectId;
use tracing::{error, info};

/// The organizations knowhow keeps a map for: every one, or a set.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TenantScope(Option<HashSet<ObjectId>>);

impl TenantScope {
    /// Every organization.
    pub fn everyone() -> Self {
        Self(None)
    }

    /// `knowhow.tenants` read: empty or `*` is every organization; otherwise
    /// the ids listed, and an entry that is no id matches nothing.
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
                Err(_) => error!(
                    entry = e,
                    "knowhow.tenants: not an organization id — it matches nothing"
                ),
            }
        }
        Self(Some(ids))
    }

    /// Whether knowhow keeps a map for `tenant`.
    pub fn serves(&self, tenant: ObjectId) -> bool {
        self.0.as_ref().is_none_or(|ids| ids.contains(&tenant))
    }

    /// P4-5-3 — the organizations listed, for a query that reads only
    /// theirs; `None` for every organization.
    pub fn listed(&self) -> Option<Vec<ObjectId>> {
        self.0.as_ref().map(|ids| ids.iter().copied().collect())
    }

    /// Said once at boot, so the log shows whose maps are kept.
    pub fn log(&self) {
        match &self.0 {
            None => info!("knowhow: keeps a map for every organization"),
            Some(ids) => info!(
                organizations = ids.len(),
                "knowhow: keeps a map only for the organizations in knowhow.tenants"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_or_a_star_is_everyone_and_a_list_is_only_those() {
        let a = ObjectId::new();
        let b = ObjectId::new();
        for raw in ["", "  ", "*", "x, *"] {
            assert!(TenantScope::parse(raw).serves(a), "{raw:?}");
        }
        let only_a = TenantScope::parse(&format!("{a}, "));
        assert!(only_a.serves(a) && !only_a.serves(b));
        // A typo matches nothing: it shuts the org it meant, opens no other.
        let typo = TenantScope::parse("not-an-id");
        assert!(!typo.serves(a) && !typo.serves(b));
    }
}
