// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-129 B1 — `GET /api/setup/features`: the server's word on which install
//! paths this wizard may offer (B1-s, key design B.6):
//!
//! ```json
//! {"pairing": true, "wizard_unix_daemon": false}
//! ```
//!
//! ⚠️ Anything but a JSON `true` reads as **false**: an older server's 400 or
//! 404, a 503 with the fleet module unmounted, a proxy's HTML error page, a
//! timeout, a string `"true"`. (The 400 is measured: a server older than B1-s
//! routes `/api/setup/features` to its `/api/setup/{platform}` download and
//! answers `{"error": "bad_request", "message": "unknown platform …"}`.)
//! Absent means "what the wizard could do before this
//! route existed" — the daemon on Windows only — and that is the safe
//! direction: a daemon card the server does not vouch for is the failure the
//! switch exists to prevent. The answer is never cached across runs, and it is
//! fetched again for the chosen server right before an install
//! ([`crate::orchestrator_unix`]): the role is picked before the server.

use std::time::Duration;

use serde::Serialize;

/// How long one fetch may take, connect included. The Welcome page waits on
/// it before it can offer a daemon card, so it is short; a server that does
/// not answer in time offers no card, like a server that answers nothing.
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(5);

/// The route, under the server's origin.
pub const ROUTE: &str = "/api/setup/features";

/// The two switches, each `false` unless the server said `true`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetupFeatures {
    /// `/api/pair/*` is mounted: the browser pairing may be offered.
    pub pairing: bool,
    /// The released wizard installs the full daemon on Linux and macOS, and
    /// the operator has said so: the daemon cards may appear there.
    pub wizard_unix_daemon: bool,
}

/// The URL to fetch for `server`, read as the enroll reads it (a remote
/// `http://` upgraded to `https://`, loopback left alone, no trailing slash).
pub fn features_url(server: &str) -> String {
    let origin = roomler_ai_remote_control::server_url::secure(server, false).url;
    format!("{}{ROUTE}", origin.trim_end_matches('/'))
}

/// The answer as the switches: a 2xx whose body is a JSON object, with each
/// key the JSON boolean `true`. Everything else is `false` for that key.
pub fn parse(status: u16, body: &[u8]) -> SetupFeatures {
    if !(200..300).contains(&status) {
        return SetupFeatures::default();
    }
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(body) else {
        return SetupFeatures::default();
    };
    let Some(object) = value.as_object() else {
        return SetupFeatures::default();
    };
    let flag = |key: &str| object.get(key).and_then(serde_json::Value::as_bool) == Some(true);
    SetupFeatures {
        pairing: flag("pairing"),
        wizard_unix_daemon: flag("wizard_unix_daemon"),
    }
}

/// An answer that only says the server predates the route, which is no
/// failure: a 404, or the 400 an older server's `/api/setup/{platform}`
/// answers for the unknown platform "features".
pub fn older_server(status: u16) -> bool {
    matches!(status, 400 | 404)
}

/// Ask `server`. Never fails: a server that cannot be reached, answers
/// something else, or takes too long reads as both switches off. An older
/// server's answer is quiet ([`older_server`]); any other failure is a warning
/// line, so "why is there no daemon card" has an answer in the log.
pub async fn fetch(server: &str) -> SetupFeatures {
    let url = features_url(server);
    let client = match reqwest::Client::builder()
        .user_agent(crate::proxy::USER_AGENT)
        .timeout(FETCH_TIMEOUT)
        .build()
    {
        Ok(client) => client,
        Err(e) => {
            tracing::warn!(error = %e, "setup features: no HTTP client; reading every switch as off");
            return SetupFeatures::default();
        }
    };
    match client.get(&url).send().await {
        Ok(resp) => {
            let status = resp.status().as_u16();
            let body = resp.bytes().await.unwrap_or_default();
            let features = parse(status, &body);
            if older_server(status) {
                tracing::debug!(%url, status, "setup features: an older server; every switch off");
            } else if !(200..300).contains(&status) {
                tracing::warn!(%url, status, "setup features: not answered; every switch read as off");
            } else {
                tracing::info!(%url, ?features, "setup features");
            }
            features
        }
        Err(e) => {
            tracing::warn!(%url, error = %e, "setup features: not reachable; every switch read as off");
            SetupFeatures::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ON: SetupFeatures = SetupFeatures {
        pairing: true,
        wizard_unix_daemon: true,
    };

    /// Only a JSON `true` switches a key on.
    #[test]
    fn only_a_json_true_turns_a_switch_on() {
        assert_eq!(
            parse(200, br#"{"pairing": true, "wizard_unix_daemon": true}"#),
            ON
        );
        assert_eq!(
            parse(200, br#"{"pairing": true, "wizard_unix_daemon": false}"#),
            SetupFeatures {
                pairing: true,
                wizard_unix_daemon: false
            }
        );
        // The string "true" is not true.
        assert_eq!(
            parse(200, br#"{"pairing": "true", "wizard_unix_daemon": "true"}"#),
            SetupFeatures::default()
        );
        // 1 is not true either.
        assert_eq!(
            parse(200, br#"{"pairing": 1, "wizard_unix_daemon": 1}"#),
            SetupFeatures::default()
        );
        // A key the server did not send is off.
        assert_eq!(
            parse(200, br#"{"pairing": true}"#),
            SetupFeatures {
                pairing: true,
                wizard_unix_daemon: false
            }
        );
        // A key this build has never heard of is ignored, never an error.
        assert_eq!(
            parse(
                200,
                br#"{"pairing": true, "wizard_unix_daemon": true, "later": true}"#
            ),
            ON
        );
    }

    /// Absent, unmounted, an error page, not JSON, not an object: every one
    /// reads as both switches off.
    #[test]
    fn every_other_answer_is_off() {
        assert_eq!(parse(404, b"Not Found"), SetupFeatures::default());
        assert_eq!(
            parse(503, br#"{"pairing": true, "wizard_unix_daemon": true}"#),
            SetupFeatures::default(),
            "a 503 is off even with a body that says on"
        );
        assert_eq!(
            parse(200, b"<html><body>502 Bad Gateway</body></html>"),
            SetupFeatures::default()
        );
        assert_eq!(parse(200, b""), SetupFeatures::default());
        assert_eq!(parse(200, b"true"), SetupFeatures::default());
        assert_eq!(parse(200, b"[true, true]"), SetupFeatures::default());
        assert_eq!(parse(200, b"null"), SetupFeatures::default());
        assert_eq!(
            parse(302, br#"{"pairing": true, "wizard_unix_daemon": true}"#),
            SetupFeatures::default()
        );
    }

    /// Production before B1-s, measured 2026-10-28: its `/api/setup/{platform}`
    /// takes "features" for a platform and answers 400 with an error object.
    /// That is an older server, read as both switches off and said nowhere
    /// but the debug log; a 5xx stays a warning.
    #[test]
    fn an_older_servers_400_is_off_and_quiet() {
        let body = br#"{"error":"bad_request","message":"unknown platform \"features\"; expected one of linux-x86_64 / linux-aarch64 / macos / windows-x86_64"}"#;
        assert_eq!(parse(400, body), SetupFeatures::default());
        assert!(older_server(400), "the pre-B1-s server's answer");
        assert!(older_server(404), "a server without the route at all");
        for status in [200, 302, 500, 502, 503] {
            assert!(
                !older_server(status),
                "{status} is not an older server's answer"
            );
        }
    }

    /// The route is read under the server's origin as the enroll would reach
    /// it: a remote `http://` becomes `https://`, loopback stays plain.
    #[test]
    fn the_url_follows_the_enroll_rule() {
        assert_eq!(
            features_url("https://roomler.ai/"),
            "https://roomler.ai/api/setup/features"
        );
        assert_eq!(
            features_url("http://roomler.ai"),
            "https://roomler.ai/api/setup/features"
        );
        assert_eq!(
            features_url("http://127.0.0.1:3000"),
            "http://127.0.0.1:3000/api/setup/features"
        );
    }

    /// The SPA binds camelCase.
    #[test]
    fn serialises_camel_case() {
        let json = serde_json::to_string(&ON).unwrap();
        assert_eq!(json, r#"{"pairing":true,"wizardUnixDaemon":true}"#);
    }
}
