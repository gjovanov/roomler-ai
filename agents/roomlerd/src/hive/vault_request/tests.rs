// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! `vault_request`'s checks.

use serde_json::json;

use super::*;

#[test]
fn a_request_names_a_secret_a_mode_a_use_takes_and_its_audience() {
    let req =
        parse(&json!({"handle": "db/main#password", "mode": "exec", "audience": "psql"})).unwrap();
    assert_eq!(
        req,
        AheadRequest {
            handle: Handle {
                secret: "db/main".into(),
                field: "password".into(),
            },
            mode: "exec",
            audience: "psql".into(),
            ttl: DEFAULT_TTL,
        }
    );
    let file = parse(
        &json!({"handle": "k8s/prod", "mode": "file", "audience": "kubeconfig", "ttl_s": 900}),
    )
    .unwrap();
    assert_eq!((file.mode, file.ttl), ("file", 900));
}

#[test]
fn a_request_no_use_could_take_is_refused() {
    for bad in [
        json!({"mode": "exec", "audience": "psql"}),
        json!({"handle": "", "mode": "exec", "audience": "psql"}),
        // Only the modes a use tool takes: proxy waits for its own phase.
        json!({"handle": "db/main", "mode": "proxy", "audience": "psql"}),
        json!({"handle": "db/main", "mode": "env", "audience": "psql"}),
        json!({"handle": "db/main", "audience": "psql"}),
        json!({"handle": "db/main", "mode": "exec"}),
        json!({"handle": "db/main", "mode": "exec", "audience": "/usr/bin/psql"}),
        json!({"handle": "db/main", "mode": "file", "audience": "../kubeconfig"}),
        json!({"handle": "db/main", "mode": "file", "audience": ".env"}),
        json!({"handle": "db/main", "mode": "exec", "audience": "-rf"}),
        json!({"handle": "db/main", "mode": "exec", "audience": "psql", "ttl_s": 29}),
        json!({"handle": "db/main", "mode": "exec", "audience": "psql", "ttl_s": 901}),
    ] {
        assert!(parse(&bad).is_err(), "{bad}");
    }
}
