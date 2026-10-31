// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-129 B1 — `GET /api/setup/features`: the server's word on which install
//! paths this wizard may offer (B1-s, key design B.6):
//!
//! ```json
//! {"pairing": true, "wizard_linux_daemon": true, "wizard_macos_daemon": false,
//!  "wizard_unix_daemon": false, "wizard_pairing": false}
//! ```
//!
//! **One daemon switch per OS** (B1-s2): Linux can go live while macOS still
//! owes its field run. [`SetupFeatures::daemon_offered`] is the one place
//! this wizard picks its OS's switch: that OS's own key, OR
//! `wizard_unix_daemon`. A server from before the split sends only
//! `wizard_unix_daemon`, meaning both OSes; a split server sends it as
//! `wizard_linux_daemon && wizard_macos_daemon`, the answer every wizard
//! released before the split reads for both, so the OR never turns on an OS
//! whose own key that server left off.
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

use crate::role::HostOs;

/// How long one fetch may take, connect included. The Welcome page waits on
/// it before it can offer a daemon card, so it is short; a server that does
/// not answer in time offers no card, like a server that answers nothing.
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(5);

/// The route, under the server's origin.
pub const ROUTE: &str = "/api/setup/features";

/// The switches this wizard reads, each `false` unless the server said
/// `true`. Which daemon switch applies here is [`Self::daemon_offered`]'s,
/// never a field read on its own.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SetupFeatures {
    /// `/api/pair/*` is mounted: the browser pairing may be offered.
    pub pairing: bool,
    /// The server said `"pairing": false` in so many words (AC10). Unlike a
    /// `false` in [`Self::pairing`], never an older server, a timeout or an
    /// error page, so this and only this takes the pairing card away.
    pub pairing_off: bool,
    /// B1-s2 — the released wizard installs the full daemon on Linux, and
    /// the operator has said so: the Linux daemon cards may appear.
    pub wizard_linux_daemon: bool,
    /// B1-s2 — the same for macOS.
    pub wizard_macos_daemon: bool,
    /// Both OSes at once: a server from before the split sends this key
    /// alone; a split server sends `wizard_linux_daemon &&
    /// wizard_macos_daemon` for the wizards released before it.
    pub wizard_unix_daemon: bool,
}

impl SetupFeatures {
    /// Does this server offer the daemon install from this wizard on `os`?
    /// THE per-OS rule (B1-s2), for every place that decides it: the Welcome
    /// cards and the Server page (`commands`), and the re-check before an
    /// install (`orchestrator_unix`). Linux reads `wizard_linux_daemon`,
    /// macOS `wizard_macos_daemon`, each OR `wizard_unix_daemon`, so a
    /// server from before the split still turns both on. No other OS has a
    /// switch: Windows offers its daemon cards without asking a server, and
    /// any other OS has no daemon role, so both read `false` here.
    pub fn daemon_offered(self, os: HostOs) -> bool {
        match os {
            HostOs::Linux => self.wizard_linux_daemon || self.wizard_unix_daemon,
            HostOs::Macos => self.wizard_macos_daemon || self.wizard_unix_daemon,
            HostOs::Windows | HostOs::Other => false,
        }
    }

    /// The answer as the page binds it, for `os`: the per-OS rule applied
    /// here, so the page holds one boolean and no rule of its own.
    pub fn view(self, os: HostOs) -> FeaturesView {
        FeaturesView {
            pairing: self.pairing,
            pairing_off: self.pairing_off,
            daemon_offered: self.daemon_offered(os),
        }
    }
}

/// What `cmd_setup_features` hands the page: the server's answer for THIS
/// OS. camelCase, as the SPA binds it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FeaturesView {
    /// `/api/pair/*` is mounted.
    pub pairing: bool,
    /// [`SetupFeatures::pairing_off`].
    pub pairing_off: bool,
    /// [`SetupFeatures::daemon_offered`] for this OS.
    pub daemon_offered: bool,
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
        pairing_off: object.get("pairing").and_then(serde_json::Value::as_bool) == Some(false),
        wizard_linux_daemon: flag("wizard_linux_daemon"),
        wizard_macos_daemon: flag("wizard_macos_daemon"),
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
/// something else, or takes too long reads as every switch off. An older
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
        pairing_off: false,
        wizard_linux_daemon: true,
        wizard_macos_daemon: true,
        wizard_unix_daemon: true,
    };
    const ALL_ON: &[u8] = br#"{"pairing": true, "wizard_linux_daemon": true, "wizard_macos_daemon": true, "wizard_unix_daemon": true, "wizard_pairing": true}"#;

    /// Only a JSON `true` switches a key on.
    #[test]
    fn only_a_json_true_turns_a_switch_on() {
        assert_eq!(parse(200, ALL_ON), ON);
        assert_eq!(
            parse(
                200,
                br#"{"pairing": true, "wizard_linux_daemon": true, "wizard_macos_daemon": false, "wizard_unix_daemon": false}"#
            ),
            SetupFeatures {
                pairing: true,
                wizard_linux_daemon: true,
                ..SetupFeatures::default()
            }
        );
        assert_eq!(
            parse(200, br#"{"wizard_macos_daemon": true}"#),
            SetupFeatures {
                wizard_macos_daemon: true,
                ..SetupFeatures::default()
            }
        );
        // The string "true" is not true.
        assert_eq!(
            parse(
                200,
                br#"{"pairing": "true", "wizard_linux_daemon": "true", "wizard_macos_daemon": "true", "wizard_unix_daemon": "true"}"#
            ),
            SetupFeatures::default()
        );
        // 1 is not true either.
        assert_eq!(
            parse(
                200,
                br#"{"pairing": 1, "wizard_linux_daemon": 1, "wizard_macos_daemon": 1, "wizard_unix_daemon": 1}"#
            ),
            SetupFeatures::default()
        );
        // A key the server did not send is off: an older server's answer
        // carries no per-OS key at all.
        assert_eq!(
            parse(200, br#"{"pairing": true, "wizard_unix_daemon": true}"#),
            SetupFeatures {
                pairing: true,
                wizard_unix_daemon: true,
                ..SetupFeatures::default()
            }
        );
        assert_eq!(
            parse(200, br#"{"pairing": true}"#),
            SetupFeatures {
                pairing: true,
                ..SetupFeatures::default()
            }
        );
        // A key this build has never heard of is ignored, never an error.
        assert_eq!(
            parse(
                200,
                br#"{"pairing": true, "wizard_linux_daemon": true, "wizard_macos_daemon": true, "wizard_unix_daemon": true, "later": true}"#
            ),
            ON
        );
    }

    /// B1-s2 — the per-OS pick, against a split server and against a server
    /// from before the split, which sends `wizard_unix_daemon` alone.
    #[test]
    fn each_os_reads_its_own_switch_or_the_older_one_key() {
        let pick = |body: &[u8]| {
            let f = parse(200, body);
            (
                f.daemon_offered(HostOs::Linux),
                f.daemon_offered(HostOs::Macos),
            )
        };
        // A split server: Linux on alone, macOS on alone, both, neither.
        assert_eq!(
            pick(
                br#"{"pairing": true, "wizard_linux_daemon": true, "wizard_macos_daemon": false, "wizard_unix_daemon": false, "wizard_pairing": false}"#
            ),
            (true, false),
            "Linux on alone"
        );
        assert_eq!(
            pick(
                br#"{"pairing": true, "wizard_linux_daemon": false, "wizard_macos_daemon": true, "wizard_unix_daemon": false, "wizard_pairing": false}"#
            ),
            (false, true),
            "macOS on alone"
        );
        assert_eq!(pick(ALL_ON), (true, true), "both on");
        assert_eq!(
            pick(
                br#"{"pairing": true, "wizard_linux_daemon": false, "wizard_macos_daemon": false, "wizard_unix_daemon": false, "wizard_pairing": false}"#
            ),
            (false, false),
            "the defaults"
        );
        // A server from before the split: its one key is both OSes.
        assert_eq!(
            pick(br#"{"pairing": true, "wizard_unix_daemon": true, "wizard_pairing": false}"#),
            (true, true),
            "an older server, on"
        );
        assert_eq!(
            pick(br#"{"pairing": true, "wizard_unix_daemon": false, "wizard_pairing": false}"#),
            (false, false),
            "an older server, off"
        );
        // No answer at all: neither.
        assert_eq!(pick(b"Not Found"), (false, false));
        // Windows never asks a server, and no other OS has a daemon role.
        for os in [HostOs::Windows, HostOs::Other] {
            assert!(!ON.daemon_offered(os), "{os:?}");
        }
    }

    /// The page gets this OS's answer and no rule of its own.
    #[test]
    fn the_view_is_this_oss_answer() {
        let linux_only = SetupFeatures {
            pairing: true,
            wizard_linux_daemon: true,
            ..SetupFeatures::default()
        };
        assert_eq!(
            linux_only.view(HostOs::Linux),
            FeaturesView {
                pairing: true,
                pairing_off: false,
                daemon_offered: true
            }
        );
        assert_eq!(
            linux_only.view(HostOs::Macos),
            FeaturesView {
                pairing: true,
                pairing_off: false,
                daemon_offered: false
            }
        );
        assert_eq!(
            SetupFeatures::default().view(HostOs::Linux),
            FeaturesView::default()
        );
    }

    /// AC10 — only the server's own `false` takes the pairing card away. An
    /// answer that says nothing about pairing leaves the start to decide, so a
    /// blink of the network never hides it from a server that has it.
    #[test]
    fn only_the_server_s_own_word_turns_pairing_off() {
        assert!(parse(200, br#"{"pairing": false}"#).pairing_off);
        assert!(
            parse(200, br#"{"pairing": false}"#)
                .view(HostOs::Windows)
                .pairing_off
        );
        let not_its_word: [(u16, &[u8]); 6] = [
            (200, br#"{"pairing": true}"#),
            (200, br#"{"wizard_linux_daemon": true}"#),
            (200, br#"{"pairing": "false"}"#),
            (200, b"not json"),
            (404, br#"{"pairing": false}"#),
            (503, b""),
        ];
        for (status, body) in not_its_word {
            assert!(!parse(status, body).pairing_off, "{status}");
        }
    }

    /// Absent, unmounted, an error page, not JSON, not an object: every one
    /// reads as every switch off.
    #[test]
    fn every_other_answer_is_off() {
        assert_eq!(parse(404, b"Not Found"), SetupFeatures::default());
        assert_eq!(
            parse(503, ALL_ON),
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
        assert_eq!(parse(302, ALL_ON), SetupFeatures::default());
    }

    /// Production before B1-s, measured 2026-10-28: its `/api/setup/{platform}`
    /// takes "features" for a platform and answers 400 with an error object.
    /// That is an older server, read as every switch off and said nowhere
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
        let json = serde_json::to_string(&ON.view(HostOs::Linux)).unwrap();
        assert_eq!(json, r#"{"pairing":true,"daemonOffered":true}"#);
    }
}
