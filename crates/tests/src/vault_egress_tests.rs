// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P3b-1a — a session's egress allowlist: `rc:vault.egress`, decided
//! by Cedar's `net:egress` (decision 27).
//!
//! Driven through the RAW agent socket the lease tests use. What these lock
//! is the SERVER:
//!
//! - a session reaches the hosts its role's policies name, by name or by
//!   pattern, on the port they name, and no other: nothing is seeded, so a
//!   host no policy names is denied;
//! - a host is decided and answered in its one spelling, recorded first;
//! - a policy names a host only in that spelling, and `simulate` and
//!   `simulate/replay` decide an egress request as the vault does;
//! - it is believed only from the device the session's record places it on,
//!   at the record's fence, for a session holding a role that is not over.

use std::time::Duration;

use bson::{Document, doc, oid::ObjectId};
use futures::StreamExt;
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;

use crate::fixtures::seed::SeededTenant;
use crate::fixtures::test_app::TestApp;
use crate::vault_lease_tests::{
    Device, TAKES_LEASES, device, lease_app, post, refused, rows, send, start,
};

/// Ask to reach a host; the vault's answer to that request.
async fn ask_egress(dev: &mut Device, req_id: &str, mut frame: Value) -> Value {
    frame["t"] = json!("rc:vault.egress");
    frame["req_id"] = json!(req_id);
    send(&mut dev.ws, frame).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        let next = tokio::time::timeout(left, dev.ws.next())
            .await
            .expect("no answer to the egress request within 10 s")
            .expect("agent socket closed")
            .expect("agent socket error");
        if let Message::Text(text) = next {
            let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            let t = v["t"].as_str().unwrap_or_default();
            if (t == "rc:vault.egress.allowed" || t == "rc:vault.refused") && v["req_id"] == req_id
            {
                return v;
            }
        }
    }
}

/// The host and port an allow names.
fn allowed(answer: &Value) -> (&str, u64) {
    assert_eq!(answer["t"], "rc:vault.egress.allowed", "{answer}");
    (
        answer["host"].as_str().unwrap(),
        answer["port"].as_u64().unwrap(),
    )
}

/// A role `dev` anyone may assume, which reaches PyPI on 443, by its name,
/// and the hosts under `pythonhosted.org` on 443, by a pattern. The id of
/// the PyPI permit.
async fn seed_egress(app: &TestApp, t: &SeededTenant) -> String {
    let (code, body) = post(app, t, "/vault/role", json!({ "name": "dev" })).await;
    assert_eq!(code, 201, "{body}");
    let mut pypi = String::new();
    for (name, text) in [
        (
            "anyone-may-be-dev",
            r#"permit (principal, action == Roomler::Action::"role:assume", resource == Roomler::Role::"dev");"#,
        ),
        (
            "dev-reaches-pypi",
            r#"permit (principal in Roomler::Role::"dev", action == Roomler::Action::"net:egress", resource == Roomler::Host::"pypi.org") when { resource.port == 443 };"#,
        ),
        (
            "dev-reaches-pythonhosted",
            r#"permit (principal in Roomler::Role::"dev", action == Roomler::Action::"net:egress", resource) when { resource.name like "*.pythonhosted.org" && resource.port == 443 };"#,
        ),
    ] {
        let (code, body) = post(
            app,
            t,
            "/vault/policy",
            json!({ "name": name, "text": text }),
        )
        .await;
        assert_eq!(code, 201, "{name}: {body}");
        if name == "dev-reaches-pypi" {
            pypi = body["id"].as_str().unwrap().to_string();
        }
    }
    pypi
}

/// FR-90 P3b-1a — a session holding a role reaches the hosts its role's
/// policies name, by name or by pattern, on the port they name; the host is
/// decided, recorded and answered in its one spelling. Another port, a host
/// no policy names, and a request past its bounds are each refused in one
/// word; a policy names a host only in that spelling; and `simulate` and
/// `simulate/replay` decide an egress request as the vault does.
#[tokio::test]
async fn a_session_reaches_the_hosts_its_role_is_allowed_and_nothing_else() {
    let app = lease_app().await;
    let t = app.seed_tenant("vaultegress").await;
    let pypi = seed_egress(&app, &t).await;
    let mut dev = device(&app, &t, "egress-host", "linux", TAKES_LEASES, true).await;
    let (sid, fence) = start(&app, &t, &mut dev, "/srv/e", Some("dev")).await;
    let ask = |host: &str, port: u16| json!({ "session_id": sid, "fence": fence, "host": host, "port": port });

    // PyPI, in any spelling of its name: allowed, and answered as decided.
    let answer = ask_egress(&mut dev, "e1", ask("PyPI.org.", 443)).await;
    assert_eq!(allowed(&answer), ("pypi.org", 443));

    // Recorded before it was answered: the allow, naming what was asked.
    let allows = rows(
        &app,
        "vault_audit",
        doc! { "action": "net:egress", "decision": "allow" },
    )
    .await;
    assert_eq!(allows.len(), 1);
    let allow = &allows[0];
    assert_eq!(allow.get_str("target").unwrap(), "asked:pypi.org:443");
    assert_eq!(allow.get_str("op").unwrap(), "reach");
    assert_eq!(allow.get_str("principal").unwrap(), "session");
    assert_eq!(allow.get_object_id("session_id").unwrap().to_hex(), sid);
    assert_eq!(
        allow.get_object_id("device_id").unwrap().to_hex(),
        dev.agent_id
    );
    let request = allow.get_document("request").unwrap();
    assert_eq!(request.get_str("action").unwrap(), "net:egress");
    assert_eq!(
        (
            request.get_str("host").unwrap(),
            request.get_i32("port").unwrap()
        ),
        ("pypi.org", 443),
        "the request names the host in its one spelling"
    );
    assert_eq!(
        rows(&app, "vault_leases", doc! {}).await.len(),
        0,
        "an allow is no lease"
    );

    // A host the pattern covers: allowed.
    let answer = ask_egress(&mut dev, "e2", ask("files.pythonhosted.org", 443)).await;
    assert_eq!(allowed(&answer), ("files.pythonhosted.org", 443));

    // Another port, a host no policy names, an address: denied, recorded.
    for (i, (host, port, target)) in [
        ("pypi.org", 80, "asked:pypi.org:80"),
        (
            "pythonhosted.org.evil.example",
            443,
            "asked:pythonhosted.org.evil.example:443",
        ),
        ("2001:DB8::1", 443, "asked:[2001:db8::1]:443"),
    ]
    .into_iter()
    .enumerate()
    {
        let a = ask_egress(&mut dev, &format!("d{i}"), ask(host, port)).await;
        assert_eq!(refused(&a), "denied", "{host}:{port}");
        let denied = rows(
            &app,
            "vault_audit",
            doc! { "action": "net:egress", "decision": "deny", "target": target },
        )
        .await;
        assert_eq!(denied.len(), 1, "{target}");
    }

    // Past its bounds, before anything is read.
    for (i, frame) in [
        json!({ "session_id": sid, "fence": fence, "host": "pypi.org" }),
        json!({ "session_id": sid, "fence": fence, "port": 443 }),
        ask("pypi.org:443", 443),
        ask("010.0.0.5", 443),
        ask("pypi.org", 0),
    ]
    .into_iter()
    .enumerate()
    {
        let a = ask_egress(&mut dev, &format!("b{i}"), frame.clone()).await;
        assert_eq!(refused(&a), "bad_request", "{frame}");
    }
    assert_eq!(
        rows(&app, "vault_audit", doc! { "op": "reach" })
            .await
            .len(),
        5,
        "two allows and three denies; nothing past a bound"
    );

    // A policy names a host only in the spelling a request is decided under.
    let (code, body) = post(
        &app,
        &t,
        "/vault/policy",
        json!({
            "name": "no-pypi",
            "text": r#"forbid (principal, action == Roomler::Action::"net:egress", resource == Roomler::Host::"PyPI.org");"#,
        }),
    )
    .await;
    assert_eq!(code, 400, "{body}");
    assert!(body.to_string().contains("write Host::"), "{body}");

    // `simulate` decides a host as the vault decides a request's.
    let sim = |host: &str, port: u16| {
        post(
            &app,
            &t,
            "/vault/simulate",
            json!({
                "action": "net:egress",
                "principal": { "role": "dev", "owner_id": t.admin.id },
                "device": { "os": "linux", "tags": [], "owner_id": t.admin.id },
                "host": host,
                "port": port,
            }),
        )
    };
    let (code, v) = sim("PyPI.org.", 443).await;
    assert_eq!((code, v["decision"].as_str()), (200, Some("allow")), "{v}");
    let (code, v) = sim("evil.example", 443).await;
    assert_eq!((code, v["decision"].as_str()), (200, Some("deny")), "{v}");
    for (host, port) in [("pypi.org:443", 443), ("010.0.0.5", 443), ("pypi.org", 0)] {
        let (code, v) = sim(host, port).await;
        assert_eq!(code, 400, "{host}:{port} {v}");
    }

    // `simulate/replay` reads the recorded requests back: without the PyPI
    // permit, its allow would be a deny, and nothing else would change.
    let (code, v) = post(
        &app,
        &t,
        "/vault/simulate/replay",
        json!({ "without": [pypi] }),
    )
    .await;
    assert_eq!(code, 200, "{v}");
    assert_eq!(v["unreadable"], json!(0), "{v}");
    assert_eq!(v["flipped"], json!(1), "{v}");
    assert_eq!(
        (
            v["flips"][0]["action"].clone(),
            v["flips"][0]["target"].clone(),
            v["flips"][0]["now"].clone()
        ),
        (
            json!("net:egress"),
            json!("asked:pypi.org:443"),
            json!("deny")
        ),
        "{v}"
    );
}

/// FR-90 P3b-1a — an egress request is believed only from the device the
/// session's record places it on, at the record's fence, for a session
/// holding a role that is not over.
#[tokio::test]
async fn an_egress_request_is_believed_only_from_the_device_that_runs_the_session() {
    let app = lease_app().await;
    let t = app.seed_tenant("vaultegresswho").await;
    seed_egress(&app, &t).await;
    let mut a = device(&app, &t, "egress-a", "linux", TAKES_LEASES, true).await;
    let mut b = device(&app, &t, "egress-b", "linux", TAKES_LEASES, true).await;
    let (sid, fence) = start(&app, &t, &mut a, "/srv/a", Some("dev")).await;
    let frame = json!({ "session_id": sid, "fence": fence, "host": "pypi.org", "port": 443 });

    // Another device of the organization, asking for a's session.
    assert_eq!(
        refused(&ask_egress(&mut b, "b1", frame.clone()).await),
        "not_located"
    );
    // A fence the record does not hold.
    let mut stale = frame.clone();
    stale["fence"] = json!(fence + 1);
    assert_eq!(
        refused(&ask_egress(&mut a, "a0", stale).await),
        "stale_fence"
    );
    // A session that took no role.
    let (nsid, nfence) = start(&app, &t, &mut a, "/srv/n", None).await;
    let none = json!({ "session_id": nsid, "fence": nfence, "host": "pypi.org", "port": 443 });
    assert_eq!(refused(&ask_egress(&mut a, "n1", none).await), "no_role");

    // The role's session is allowed, until it is over.
    assert_eq!(
        allowed(&ask_egress(&mut a, "a1", frame.clone()).await),
        ("pypi.org", 443)
    );
    app.db
        .collection::<Document>("agent_sessions")
        .update_one(
            doc! { "_id": ObjectId::parse_str(&sid).unwrap() },
            doc! { "$set": { "status": "ended" } },
        )
        .await
        .unwrap();
    assert_eq!(refused(&ask_egress(&mut a, "a2", frame).await), "ended");
}
