// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P3-6d-1b — the minter a dynamic secret's provider names: one issuer,
//! one mint and one revocation for each provider this build mints for on the
//! device (`docs/fr/FR-90-hive-agent-sessions.md` §3e, "P3-6d, MySQL, SQL
//! Server and Redis users, as designed"). The lease client
//! (`crate::vault`) calls these, and never names a database.
//!
//! | Provider | Minter | Its end, as the mint gives it |
//! |---|---|---|
//! | `postgres` | [`pg_mint`] | `VALID UNTIL`, RFC 3339 |
//! | `mysql` | [`mysql_mint`] | an event, at a count of seconds |

use roomler_ai_remote_control::vault::providers;

use super::{mysql_mint, pg_mint, upstream_of};

pub(crate) use pg_mint::Minted;

/// The providers this build mints for, on the device: a lease of theirs is
/// held (its bundle lent to no tool) and its credential minted at its first
/// use. Every other provider's bundle is refused unopened. `pub`, as
/// `crate::vault` re-exports it.
pub const MINTS: [&str; 2] = [providers::POSTGRES.name, providers::MYSQL.name];

/// A dynamic secret's issuer, read from its lease's bundle by its provider's
/// minter.
pub(crate) enum Issuer {
    Postgres(pg_mint::Issuer),
    Mysql(mysql_mint::Issuer),
}

impl Issuer {
    /// The issuer `provider` names, in `bundle`. Err, in words that name no
    /// value, for a provider this build mints nothing for, or a bundle its
    /// minter refuses.
    pub(crate) fn from_bundle(provider: &str, bundle: &[u8]) -> Result<Self, String> {
        if provider == providers::POSTGRES.name {
            pg_mint::Issuer::from_bundle(bundle, upstream_of).map(Self::Postgres)
        } else if provider == providers::MYSQL.name {
            mysql_mint::Issuer::from_bundle(bundle, upstream_of).map(Self::Mysql)
        } else {
            Err(format!("this build mints nothing for {provider:?}"))
        }
    }
}

/// Mint for `lease_id`, its end the lease's: `until_ms`, in milliseconds of
/// the server's clock, given to each database in the form it takes.
pub(crate) async fn mint(issuer: &Issuer, lease_id: &str, until_ms: i64) -> Result<Minted, String> {
    match issuer {
        Issuer::Postgres(i) => pg_mint::mint(i, lease_id, &rfc3339(until_ms)?).await,
        // Whole seconds, rounded down: never past the lease's end.
        Issuer::Mysql(i) => mysql_mint::mint(i, lease_id, until_ms.div_euclid(1000)).await,
    }
}

/// Take `user` back, as its issuer.
pub(crate) async fn revoke(issuer: &Issuer, user: &str) -> Result<(), String> {
    match issuer {
        Issuer::Postgres(i) => pg_mint::revoke(i, user).await,
        Issuer::Mysql(i) => mysql_mint::revoke(i, user).await,
    }
}

/// `ms` (milliseconds of the server's clock) as RFC 3339, what a minted
/// Postgres role's `VALID UNTIL` is given.
fn rfc3339(ms: i64) -> Result<String, String> {
    bson::DateTime::from_millis(ms)
        .try_to_rfc3339_string()
        .map_err(|_| "the lease's end is not a time Postgres takes".to_string())
}

#[cfg(test)]
mod tests;
