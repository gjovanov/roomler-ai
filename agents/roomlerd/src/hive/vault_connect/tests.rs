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
            auth_source: None,
            auth: None,
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
    // P3-5c-2 — MongoDB, its auth database `admin` unless the call names one.
    let mongo = parse(&json!({
        "handle": "db/docs",
        "engine": "mongodb",
        "upstream": "mongo.internal:27017",
    }))
    .unwrap();
    assert_eq!(
        (mongo.engine, mongo.auth_source.as_deref()),
        (Engine::Mongodb, Some("admin"))
    );
    let named = parse(&json!({
        "handle": "db/docs",
        "engine": "mongodb",
        "upstream": "mongo.internal:27017",
        "auth_source": "users",
    }))
    .unwrap();
    assert_eq!(named.auth_source.as_deref(), Some("users"));
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
        // P3-5c-2 — an auth database is MongoDB's, and a database's name.
        with("auth_source", json!("admin")),
        json!({"handle": "db/main", "engine": "mongodb", "upstream": "m:27017", "auth_source": "a/b"}),
        json!({"handle": "db/main", "engine": "mongodb", "upstream": "m:27017", "auth_source": ""}),
        json!({"handle": "db/main", "engine": "mongodb", "upstream": "m:27017", "auth_source": "x".repeat(64)}),
        json!({"handle": "db/main", "engine": "mongodb", "upstream": "m:27017", "auth_source": 7}),
    ] {
        assert!(parse(&bad).is_err(), "{bad}");
    }
}

/// P3-5d-2 — an HTTP upstream is an origin: `https`, a host, a port only
/// where it is not 443, and nothing after it. One spelling per origin.
#[test]
fn an_http_upstream_is_an_origin_as_an_audience_names_it() {
    for (raw, host, port) in [
        ("https://github.com", "github.com", 443),
        ("https://git.example:8443", "git.example", 8443),
        ("https://10.0.0.5:9443", "10.0.0.5", 9443),
        ("https://[fd00::5]", "fd00::5", 443),
        ("https://[fd00::5]:8443", "fd00::5", 8443),
    ] {
        assert_eq!(
            origin_of(raw),
            Ok((host.to_string(), port)),
            "{raw} is an origin"
        );
    }
    for bad in [
        // TLS always, spelled one way.
        "http://github.com",
        "HTTPS://github.com",
        "github.com",
        "github.com:443",
        // An origin, and nothing after it.
        "https://github.com/",
        "https://github.com/org/repo",
        "https://github.com?x=1",
        "https://github.com#top",
        "https://someone@github.com",
        // Its port only where it is not 443, written plainly.
        "https://github.com:443",
        "https://[fd00::5]:443",
        "https://github.com:0443",
        "https://github.com:0",
        "https://github.com:",
        "https://",
        // An IPv6 address in brackets.
        "https://fd00::5",
        &format!("https://{}.example", "a".repeat(260)),
    ] {
        assert!(origin_of(bad).is_err(), "{bad} is not an origin");
    }
    assert!(
        origin_of("https://github.com:443")
            .unwrap_err()
            .contains("write https://github.com"),
        "port 443 is left out, and the answer says how"
    );
}

/// P3-5d-2 — an `http` call names an origin and how the value is
/// presented; a database's fields are refused in it, and its own in a
/// database's call.
#[test]
fn an_http_call_names_an_origin_and_how_the_value_goes() {
    let field = |f: &str| Handle {
        secret: "git/bot".into(),
        field: f.into(),
    };
    let basic = parse(&json!({
        "handle": "git/bot",
        "engine": "http",
        "upstream": "https://github.com",
        "auth": "basic",
    }))
    .unwrap();
    assert_eq!(
        (
            basic.engine,
            basic.auth,
            basic.host.as_str(),
            basic.port,
            basic.upstream.as_str()
        ),
        (
            Engine::Http,
            Some(Auth::Basic),
            "github.com",
            443,
            "https://github.com"
        )
    );
    assert_eq!(basic.handles(), [field("user"), field("password")]);
    assert_eq!((basic.database, basic.auth_source), (None, None));
    let bearer = parse(&json!({
        "handle": "git/bot",
        "engine": "http",
        "upstream": "https://git.example:8443",
        "auth": "bearer",
        "ca": "pki/git-ca",
    }))
    .unwrap();
    assert_eq!(bearer.auth, Some(Auth::Bearer));
    assert_eq!(
        bearer.handles(),
        [
            field("token"),
            Handle {
                secret: "pki/git-ca".into(),
                field: String::new(),
            }
        ],
        "a token, then the CA"
    );
    let ok = json!({"handle": "git/bot", "engine": "http", "upstream": "https://github.com", "auth": "bearer"});
    let with = |k: &str, v: serde_json::Value| {
        let mut call = ok.clone();
        call[k] = v;
        call
    };
    for bad in [
        json!({"handle": "git/bot", "engine": "http", "upstream": "https://github.com"}),
        with("auth", json!("digest")),
        with("auth", json!("x-api-key")),
        with("auth", json!(true)),
        with("upstream", json!("github.com:443")),
        with("database", json!("orders")),
        with("auth_source", json!("admin")),
        json!({"handle": "db/main", "engine": "postgres", "upstream": "db.internal:5432", "auth": "basic"}),
        json!({"handle": "db/main", "engine": "postgres", "upstream": "https://db.internal"}),
        json!({"handle": "db/docs", "engine": "mongodb", "upstream": "m:27017", "auth": "bearer"}),
    ] {
        assert!(parse(&bad).is_err(), "{bad}");
    }
}

/// P3-5d-2 — a row's end, as Linux writes it: native words of an address
/// held in network order, and an IPv4 address mapped into IPv6 read as
/// IPv4.
#[cfg(target_os = "linux")]
#[test]
fn a_rows_end_reads_as_the_kernel_writes_it() {
    use std::net::{Ipv4Addr, SocketAddrV4};
    let word = |b: [u8; 4]| format!("{:08X}", u32::from_ne_bytes(b));
    let v4 = format!("{}:1F90", word([127, 0, 0, 1]));
    assert_eq!(
        proc_net::end(&v4),
        Some(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 8080))
    );
    let mapped = format!(
        "{}{}{}{}:0050",
        word([0; 4]),
        word([0; 4]),
        word([0, 0, 0xff, 0xff]),
        word([10, 0, 0, 5])
    );
    assert_eq!(
        proc_net::end(&mapped),
        Some(SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 5), 80))
    );
    let v6 = format!(
        "{}{}{}{}:0050",
        word([0xfd, 0, 0, 0]),
        word([0; 4]),
        word([0; 4]),
        word([0, 0, 0, 5])
    );
    assert_eq!(proc_net::end(&v6), None, "an IPv6 end is not one of ours");
    for bad in [
        "",
        "0100007F",
        "0100007F:",
        "0100007G:0050",
        "01007F:0050",
        "0100007F:10000",
    ] {
        assert_eq!(proc_net::end(bad), None, "{bad:?}");
    }
}

/// P3-5d-2 — the kernel names the account that owns a loopback
/// connection's client end, and nothing for a pair no connection is.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_loopback_connection_is_owned_by_whoever_opened_it() {
    use std::net::SocketAddr;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = tokio::net::TcpStream::connect(listener.local_addr().unwrap())
        .await
        .unwrap();
    let (accepted, _) = listener.accept().await.unwrap();
    let (SocketAddr::V4(from), SocketAddr::V4(to)) = (
        accepted.peer_addr().unwrap(),
        accepted.local_addr().unwrap(),
    ) else {
        panic!("IPv4 loopback");
    };
    // SAFETY: geteuid reads our own credentials.
    let me = unsafe { libc::geteuid() };
    assert_eq!(proc_net::owner(from, to), Some(me));
    let other = std::net::SocketAddrV4::new(*to.ip(), to.port().wrapping_add(1));
    assert_eq!(proc_net::owner(from, other), None, "no such connection");
    drop(client);
}
