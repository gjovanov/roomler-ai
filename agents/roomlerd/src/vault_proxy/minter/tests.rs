// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! The minter by provider: each provider this build mints for reaches its
//! own minter, and any other is refused.

use serde_json::json;

use super::super::testing::made;
use super::*;

/// A bundle any of the minters reads: an issuer, its database, and a CA made
/// for the test.
fn bundle() -> Vec<u8> {
    let ca = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
        .unwrap()
        .cert
        .pem();
    let word = made();
    serde_json::to_vec(&json!({
        "audience": "db.internal:3306",
        "fields": {
            "admin_user": "issuer",
            "admin_password": word,
            "database": "orders",
            "ca": ca,
        },
    }))
    .unwrap()
}

/// Each provider in `MINTS` is one the wire names, and is read by its own
/// minter; a bundle that does not parse is refused in that minter's words.
#[test]
fn each_provider_this_build_mints_for_reaches_its_own_minter() {
    for p in MINTS {
        assert!(providers::named(p).is_some(), "{p}");
        assert_eq!(
            Issuer::from_bundle(p, b"not a bundle").err().as_deref(),
            Some("the bundle does not parse"),
            "{p}"
        );
    }
    let b = bundle();
    assert!(matches!(
        Issuer::from_bundle(providers::POSTGRES.name, &b),
        Ok(Issuer::Postgres(_))
    ));
    assert!(matches!(
        Issuer::from_bundle(providers::MYSQL.name, &b),
        Ok(Issuer::Mysql(_))
    ));
}

/// A provider this build mints nothing for, however it is spelled, is
/// refused before its bundle is read.
#[test]
fn any_other_provider_is_refused() {
    let b = bundle();
    for p in ["github_app", "Postgres", "MYSQL", "", "from_a_newer_build"] {
        assert_eq!(
            Issuer::from_bundle(p, &b).err(),
            Some(format!("this build mints nothing for {p:?}")),
            "{p}"
        );
    }
}

/// The lease's end, as Postgres's `VALID UNTIL` takes it.
#[test]
fn the_lease_end_is_a_time_postgres_takes() {
    assert_eq!(rfc3339(1_767_225_600_000).unwrap(), "2026-01-01T00:00:00Z");
}
