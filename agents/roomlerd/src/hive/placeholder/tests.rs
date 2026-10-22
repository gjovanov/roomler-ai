// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! `vault_placeholder`'s placeholders, checks and registry. The calls, with
//! a vault, are the toolbelt's tests.

use serde_json::json;

use super::*;

/// A placeholder is `rph_` and 32 base32 characters, every one new; the
/// base32 is RFC 4648's, in lower case.
#[test]
fn a_placeholder_is_rph_and_160_random_bits_in_base32() {
    let a = mint().unwrap();
    let b = mint().unwrap();
    assert_eq!(a.len(), PREFIX.len() + RANDOM_CHARS);
    assert!(a.starts_with("rph_"), "{a}");
    assert!(is_placeholder(&a) && is_placeholder(&b), "{a} {b}");
    assert_ne!(a, b, "a new one every time");
    assert_eq!(base32(&[0; 20]), "a".repeat(32));
    assert_eq!(base32(&[0xff; 20]), "7".repeat(32));
    // RFC 4648's own: "1234567890" is GEZDGNBVGY3TQOJQ, so twice over is
    // that twice (built here: a 32-character literal reads as a key).
    assert_eq!(
        base32(b"12345678901234567890"),
        "GEZDGNBVGY3TQOJQ".to_lowercase().repeat(2)
    );
}

/// Nothing passes for a placeholder but that shape.
#[test]
fn nothing_else_passes_for_a_placeholder() {
    let good = format!("rph_{}", "a2".repeat(16));
    assert!(is_placeholder(&good));
    for bad in [
        String::new(),
        "rph_".to_string(),
        good.to_uppercase(),
        format!("RPH_{}", "a2".repeat(16)),
        // Its random part in upper case, behind the right prefix: base32 of
        // one case only, so a token is matched exactly, byte for byte.
        format!("rph_{}", "A2".repeat(16)),
        format!("rph-{}", "a2".repeat(16)),
        format!("xrph_{}", "a2".repeat(16)),
        format!("rph_{}", "a".repeat(31)),
        format!("rph_{}", "a".repeat(33)),
        // 0, 1, 8 and 9 are not base32's.
        format!("rph_{}1", "a".repeat(31)),
        format!("rph_{}8", "a".repeat(31)),
    ] {
        assert!(!is_placeholder(&bad), "{bad}");
    }
}

/// A call names its variables, each with its secret, and the origin their
/// values go to; its TTL defaults to 300 s.
#[test]
fn a_call_names_its_variables_their_secrets_and_the_audience() {
    let req = parse(&json!({
        "env": {"GH_TOKEN": "github/bot#token"},
        "audience": "https://api.github.com",
    }))
    .unwrap();
    assert_eq!(
        req,
        PlaceholderRequest {
            env: vec![(
                "GH_TOKEN".into(),
                Handle {
                    secret: "github/bot".into(),
                    field: "token".into(),
                }
            )],
            host: "api.github.com".into(),
            port: 443,
            ttl: DEFAULT_TTL,
        }
    );
    assert_eq!(req.audience(), "https://api.github.com");
    let ported = parse(&json!({
        "env": {"TOKEN": "git/bot#token", "USER_NAME": "git/bot#user"},
        "audience": "https://git.example:8443",
        "ttl_s": 900,
    }))
    .unwrap();
    assert_eq!((ported.port, ported.ttl, ported.env.len()), (8443, 900, 2));
    assert_eq!(ported.audience(), "https://git.example:8443");
    let v6 = parse(&json!({"env": {"T": "s#f"}, "audience": "https://[fd00::5]:8443"})).unwrap();
    assert_eq!(v6.host, "fd00::5");
    assert_eq!(v6.audience(), "https://[fd00::5]:8443");
}

/// What the proxy could not serve, or the vault would not take, is refused
/// before anything is asked, saying what to write instead where it can.
#[test]
fn a_call_the_proxy_could_not_serve_is_refused_unasked() {
    let audience = "https://api.github.com";
    let nine: serde_json::Map<String, Value> =
        (0..9).map(|i| (format!("V{i}"), json!("s#f"))).collect();
    for (args, says) in [
        (json!({"audience": audience}), "env must name 1 to 8"),
        (
            json!({"env": {}, "audience": audience}),
            "env must name 1 to 8",
        ),
        (
            json!({"env": nine, "audience": audience}),
            "env must name 1 to 8",
        ),
        (
            json!({"env": {"gh_token": "s#f"}, "audience": audience}),
            "is not a variable",
        ),
        (
            json!({"env": {"PATH": "s#f"}, "audience": audience}),
            "is not a variable",
        ),
        (
            json!({"env": {"LD_PRELOAD": "s#f"}, "audience": audience}),
            "is not a variable",
        ),
        (
            json!({"env": {"1X": "s#f"}, "audience": audience}),
            "is not a variable",
        ),
        (
            json!({"env": {"T": 5}, "audience": audience}),
            "name its secret as a string",
        ),
        (
            json!({"env": {"T": ""}, "audience": audience}),
            "is not a secret",
        ),
        (json!({"env": {"T": "s#f"}}), "audience is required"),
        (
            json!({"env": {"T": "s#f"}, "audience": "http://api.github.com"}),
            "audience must be an origin",
        ),
        (
            json!({"env": {"T": "s#f"}, "audience": "https://api.github.com/user"}),
            "audience must be an origin",
        ),
        (
            json!({"env": {"T": "s#f"}, "audience": "https://api.github.com:443"}),
            "write https://api.github.com",
        ),
        (
            json!({"env": {"T": "s#f"}, "audience": "https://API.github.com"}),
            "write https://api.github.com",
        ),
        (
            json!({"env": {"T": "s#f"}, "audience": "https://api.github.com."}),
            "write https://api.github.com",
        ),
        (
            json!({"env": {"T": "s#f"}, "audience": audience, "ttl_s": 29}),
            "ttl_s must be",
        ),
        (
            json!({"env": {"T": "s#f"}, "audience": audience, "ttl_s": 901}),
            "ttl_s must be",
        ),
        (
            json!({"env": {"T": "s#f"}, "audience": audience, "ttl_s": "300"}),
            "ttl_s must be",
        ),
    ] {
        let refused = parse(&args).expect_err(says);
        assert!(refused.contains(says), "{args}: {refused}");
    }
}

/// The registry answers a run's placeholders for one host and port, and
/// lets them go one by one or all with the run.
#[test]
fn a_runs_placeholders_are_its_own_and_go_with_it() {
    let (s, other) = (ObjectId::new(), ObjectId::new());
    let put = |session: ObjectId, token: &str, host: &str, port: u16| {
        runs().entry((session, 1)).or_default().push(Bound {
            token: token.into(),
            lease_id: format!("L-{token}"),
            host: host.into(),
            port,
        });
    };
    put(s, "t1", "api.github.com", 443);
    put(s, "t2", "api.github.com", 443);
    put(s, "t3", "api.github.com", 8443);
    put(other, "t4", "api.github.com", 443);
    let tokens = |host: &str, port: u16| -> Vec<String> {
        bound(s, 1, host, port)
            .into_iter()
            .map(|b| b.token)
            .collect()
    };
    assert_eq!(tokens("api.github.com", 443), ["t1", "t2"]);
    assert_eq!(tokens("api.github.com", 8443), ["t3"]);
    assert!(tokens("github.com", 443).is_empty(), "another host");
    assert!(
        bound(s, 2, "api.github.com", 443).is_empty(),
        "another fence"
    );
    assert_eq!(held(s, 1), 3);
    forget((s, 1), "t1");
    assert_eq!(tokens("api.github.com", 443), ["t2"]);
    assert!(drop_run(s, 1));
    assert!(!drop_run(s, 1), "once");
    assert_eq!(held(s, 1), 0);
    assert_eq!(held(other, 1), 1, "another run's are its own");
    forget((other, 1), "t4");
    assert!(
        !drop_run(other, 1),
        "its last one gone, the run's entry went with it"
    );
}
