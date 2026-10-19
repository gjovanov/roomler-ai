// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! `vault_connect`'s checks. The endpoint itself is driven through the
//! toolbelt (`toolbelt/tests.rs`), against a Postgres upstream over real TLS.

use serde_json::json;

use super::*;

#[test]
fn an_upstream_is_a_host_and_a_port_as_an_audience_names_them() {
    for (raw, host, port) in [
        ("db.internal:5432", "db.internal", 5432),
        ("10.0.0.5:6432", "10.0.0.5", 6432),
        ("[fd00::5]:5432", "fd00::5", 5432),
        ("pg-1.example.com:65535", "pg-1.example.com", 65535),
        ("orders_db:5432", "orders_db", 5432),
    ] {
        assert_eq!(
            upstream_of(raw),
            Ok((host.to_string(), port)),
            "{raw} is an upstream"
        );
        #[cfg(unix)]
        assert!(
            crate::vault_proxy::tls::server_name(host).is_ok(),
            "{host} is a name TLS verifies"
        );
    }
    for bad in [
        "db.internal",
        "db.internal:",
        ":5432",
        "db.internal:0",
        "db.internal:65536",
        // One spelling per port: no sign, no leading zero.
        "db.internal:+5432",
        "db.internal:05432",
        // An IPv6 address is bracketed, and an address at all.
        "fd00::5:5432",
        "[fd00::zz]:5432",
        "[fe80::1%eth0]:5432",
        "-db:5432",
        ".db:5432",
        "db internal:5432",
        "db/x:5432",
        &format!("{}:5432", "a".repeat(254)),
        &format!("{}.internal:5432", "a".repeat(260)),
    ] {
        assert!(upstream_of(bad).is_err(), "{bad} is not an upstream");
    }
}

#[test]
fn a_call_names_a_secret_an_engine_and_an_upstream() {
    let req = parse(&json!({
        "handle": "db/main",
        "engine": "postgres",
        "upstream": "db.internal:5432",
    }))
    .unwrap();
    assert_eq!(
        req,
        ConnectRequest {
            secret: "db/main".into(),
            engine: Engine::Postgres,
            upstream: "db.internal:5432".into(),
            host: "db.internal".into(),
            port: 5432,
            database: None,
            ca: None,
            ttl: DEFAULT_TTL,
        }
    );
    let field = |s: &str, f: &str| Handle {
        secret: s.into(),
        field: f.into(),
    };
    assert_eq!(
        req.handles(),
        [field("db/main", "user"), field("db/main", "password")],
        "the endpoint signs in with the secret's user and password"
    );
    let full = parse(&json!({
        "handle": "rrn:grox:secret:db/main",
        "engine": "postgres",
        "upstream": "[fd00::5]:5432",
        "database": "orders",
        "ca": "pki/db-ca#pem",
        "ttl_s": 900,
    }))
    .unwrap();
    assert_eq!(full.database.as_deref(), Some("orders"));
    assert_eq!(full.ttl, 900);
    assert_eq!(
        full.handles(),
        [
            field("rrn:grox:secret:db/main", "user"),
            field("rrn:grox:secret:db/main", "password"),
            field("pki/db-ca", "pem"),
        ],
        "and the CA's value, where the call names one"
    );
}

#[test]
fn a_call_no_endpoint_could_serve_is_refused() {
    let ok = json!({"handle": "db/main", "engine": "postgres", "upstream": "db.internal:5432"});
    let with = |k: &str, v: serde_json::Value| {
        let mut call = ok.clone();
        call[k] = v;
        call
    };
    for bad in [
        json!({"engine": "postgres", "upstream": "db.internal:5432"}),
        // The secret itself: the endpoint leases its fields.
        with("handle", json!("db/main#password")),
        with("handle", json!("")),
        with("engine", json!("mysql")),
        json!({"handle": "db/main", "upstream": "db.internal:5432"}),
        json!({"handle": "db/main", "engine": "postgres"}),
        with("upstream", json!("db.internal")),
        with("database", json!("")),
        with("database", json!("x".repeat(MAX_DATABASE + 1))),
        with("database", json!("a\nb")),
        with("database", json!(5)),
        with("ca", json!(true)),
        with("ca", json!("")),
        with("ttl_s", json!(29)),
        with("ttl_s", json!(901)),
    ] {
        assert!(parse(&bad).is_err(), "{bad}");
    }
}
