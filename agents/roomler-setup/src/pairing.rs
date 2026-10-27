// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-129 A3 — the wizard's pairing session: where the device code and the
//! delivered enrollment token live, and the only place either is held.
//!
//! The SPA drives the flow (`cmd_pair_start` → a poll loop over
//! `cmd_pair_poll` → the person's Install click → `cmd_install` with
//! `usePaired`), but it never holds a secret: the start answers it the
//! user code, the URI and the clock; a poll answers it a status, an org and
//! an approver; the device code and the token(s) stay in [`PairStore`], in
//! this process's memory, for exactly as long as the session lives. That is
//! the rule the pasted path already follows (H5: the raw token never
//! round-trips through the webview) applied to the credential the pairing
//! holds, and it is structural: `WizardState` has no field for either, so a
//! `cmd_save_state` cannot persist one.
//!
//! A session has a **generation**. Every start replaces the session and
//! bumps it; a poll copies the generation out with the device code, and its
//! answer is applied only if the session is still that one. So a poll that
//! was in flight when the person clicked "Start again" — or Cancel, then a
//! fresh start — can never deliver an old pairing's token into the new
//! session, or end the new session with the old one's denial.
//!
//! ⚠️ The store's mutex is a `std::sync::Mutex` and is **never held across
//! an await**: a command copies what the request needs out
//! ([`PairStore::snapshot`]), drops the guard, awaits the HTTP call, then
//! applies the answer under a fresh lock ([`PairStore::apply_poll`]). The
//! root `clippy.toml` says why a guard across an await is a stopped runtime.
//!
//! The delivered token is **cloned out, not consumed**, by `cmd_install`
//! ([`PairStore::token_for`]) and cleared when the install **succeeds**
//! ([`PairStore::finish`]): an install that failed before its enroll
//! (a download that broke) can Retry with the same still-valid token,
//! exactly as the pasted path retries with the same pasted one. Delivery is
//! once on the server, so there is no second copy to fetch.

use std::sync::{Mutex, MutexGuard, OnceLock};

use roomlerd::pairing::{Approved, DeviceCode, PollOutcome, Started};
use serde::Serialize;

use crate::role::Role;

/// What the SPA learns from a poll. `status` is the server's vocabulary
/// plus `unknown` for a spelling this build has not heard of (`detail`
/// then carries the server's own); `interval` rides `slow_down`; the org
/// and the approver ride `approved`.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PairPollView {
    pub status: PairPollStatus,
    pub interval: Option<u64>,
    pub org_name: Option<String>,
    pub approver_name: Option<String>,
    pub detail: Option<String>,
}

/// The poll statuses on the SPA's wire — snake_case, the server's spellings.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PairPollStatus {
    AuthorizationPending,
    SlowDown,
    Approved,
    AccessDenied,
    ExpiredToken,
    Unknown,
}

/// One pairing, from its start until it is cancelled, ended by the server
/// or spent by a successful install.
#[derive(Debug)]
pub struct PairSession {
    /// The Server page's URL as the SPA passed it to the start — the same
    /// string it passes to `cmd_install`, compared verbatim there.
    server: String,
    /// The role the pairing was started for: the token's kind follows it.
    role: Role,
    device_code: DeviceCode,
    verification_uri: String,
    /// The delivered token(s), once a poll saw `approved`.
    approved: Option<Approved>,
}

/// A poll's inputs, copied out of the store so no guard outlives the lock.
#[derive(Clone, Debug)]
pub struct Snapshot {
    pub generation: u64,
    pub server: String,
    pub device_code: DeviceCode,
}

#[derive(Debug, Default)]
struct Inner {
    /// Bumped by every start; a session's generation is the value it was
    /// started with.
    generations: u64,
    session: Option<(u64, PairSession)>,
}

/// The process-wide session holder. One per process ([`PairStore::global`]);
/// tests build their own.
#[derive(Debug, Default)]
pub struct PairStore {
    inner: Mutex<Inner>,
}

impl PairStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// The wizard's one store.
    pub fn global() -> &'static PairStore {
        static GLOBAL: OnceLock<PairStore> = OnceLock::new();
        GLOBAL.get_or_init(PairStore::new)
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// A pairing the server opened: it replaces whatever was held — an
    /// earlier session's device code and any token it had delivered are
    /// dropped — and returns the new session's generation.
    pub fn begin(&self, server: String, role: Role, started: Started) -> u64 {
        let mut g = self.lock();
        g.generations += 1;
        let generation = g.generations;
        g.session = Some((
            generation,
            PairSession {
                server,
                role,
                device_code: started.device_code,
                verification_uri: started.verification_uri,
                approved: None,
            },
        ));
        generation
    }

    /// What a poll needs, copied out. `None` when no pairing is in progress.
    pub fn snapshot(&self) -> Option<Snapshot> {
        let g = self.lock();
        g.session.as_ref().map(|(generation, s)| Snapshot {
            generation: *generation,
            server: s.server.clone(),
            device_code: s.device_code.clone(),
        })
    }

    /// Where the person types the code — the page the Open button opens.
    pub fn verification_uri(&self) -> Option<String> {
        let g = self.lock();
        g.session.as_ref().map(|(_, s)| s.verification_uri.clone())
    }

    /// Apply a poll's answer to the session it was polled for, and say what
    /// the SPA sees. An answer for a session that is no longer held (a
    /// cancel or a fresh start raced the poll) changes nothing — it is
    /// reported as `expired_token`, which ends that loop.
    ///
    /// * `approved` stores the token(s) on the session; the SPA then shows
    ///   the org and the approver and waits for the person's click.
    /// * `access_denied` / `expired_token` end the session: the device code
    ///   is dead on the server, so nothing is left to hold.
    /// * the rest leave the session as it is.
    pub fn apply_poll(&self, generation: u64, outcome: PollOutcome) -> PairPollView {
        let mut g = self.lock();
        let current = matches!(&g.session, Some((held, _)) if *held == generation);
        if !current {
            tracing::info!(
                status = outcome.status(),
                "a poll answered for a pairing that is no longer held; ignored"
            );
            return view(PairPollStatus::ExpiredToken);
        }
        match outcome {
            PollOutcome::AuthorizationPending => view(PairPollStatus::AuthorizationPending),
            PollOutcome::SlowDown { interval } => PairPollView {
                interval: Some(interval),
                ..view(PairPollStatus::SlowDown)
            },
            PollOutcome::AccessDenied => {
                g.session = None;
                view(PairPollStatus::AccessDenied)
            }
            PollOutcome::ExpiredToken => {
                g.session = None;
                view(PairPollStatus::ExpiredToken)
            }
            PollOutcome::Approved(approved) => {
                let v = PairPollView {
                    org_name: Some(approved.org_name.clone()),
                    approver_name: Some(approved.approver_name.clone()),
                    ..view(PairPollStatus::Approved)
                };
                if let Some((_, s)) = g.session.as_mut() {
                    s.approved = Some(approved);
                }
                v
            }
            PollOutcome::Unknown(status) => PairPollView {
                detail: Some(status),
                ..view(PairPollStatus::Unknown)
            },
        }
    }

    /// Forget the session: its device code and any delivered token. The
    /// person's Cancel, Back, or a Start again.
    pub fn cancel(&self) {
        let mut g = self.lock();
        g.session = None;
    }

    /// The delivered token for an install of `role` against `server`, cloned
    /// — not consumed, see the module doc. Refused when nothing was delivered,
    /// or when the install asks for a different role or server than the
    /// pairing was started for: the token's kind follows the role, and a
    /// token minted by one server enrolls into no other.
    pub fn token_for(&self, role: Role, server: &str) -> Result<String, String> {
        let g = self.lock();
        let Some((_, s)) = g.session.as_ref() else {
            return Err("no pairing in progress — set up in the browser first".to_string());
        };
        let Some(approved) = s.approved.as_ref() else {
            return Err("the pairing has not been approved yet".to_string());
        };
        if s.role != role {
            return Err(
                "the approved pairing is for a different role — start the pairing again"
                    .to_string(),
            );
        }
        if s.server != server {
            return Err(
                "the approved pairing is for a different server — start the pairing again"
                    .to_string(),
            );
        }
        approved
            .tokens
            .first()
            .cloned()
            .ok_or_else(|| "the pairing delivered no token".to_string())
    }

    /// The install succeeded: the token is spent, the session is over.
    pub fn finish(&self) {
        let mut g = self.lock();
        g.session = None;
    }

    /// Whether a pairing is held, and whether it has been approved — for
    /// tests and a status line, never for a decision a token depends on.
    pub fn state(&self) -> (bool, bool) {
        let g = self.lock();
        match &g.session {
            None => (false, false),
            Some((_, s)) => (true, s.approved.is_some()),
        }
    }
}

fn view(status: PairPollStatus) -> PairPollView {
    PairPollView {
        status,
        interval: None,
        org_name: None,
        approver_name: None,
        detail: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn started(code: &str) -> Started {
        Started {
            device_code: DeviceCode::new(code),
            user_code: "BCDF-GHJK".into(),
            verification_uri: "https://example.invalid/pair".into(),
            expires_in: 900,
            interval: 5,
        }
    }

    fn approved(token: &str) -> PollOutcome {
        PollOutcome::Approved(Approved {
            tokens: vec![token.into()],
            org_name: "Acme".into(),
            approver_name: "Dana".into(),
        })
    }

    const SERVER: &str = "https://example.invalid";

    #[test]
    fn a_start_holds_the_session_and_a_snapshot_copies_what_a_poll_needs() {
        let store = PairStore::new();
        assert!(store.snapshot().is_none());
        assert_eq!(store.state(), (false, false));

        let generation = store.begin(SERVER.into(), Role::TunnelClient, started("dc-1"));
        let snap = store.snapshot().expect("a session");
        assert_eq!(snap.generation, generation);
        assert_eq!(snap.server, SERVER);
        assert_eq!(snap.device_code.expose(), "dc-1");
        assert_eq!(
            store.verification_uri().as_deref(),
            Some("https://example.invalid/pair")
        );
        assert_eq!(store.state(), (true, false));
    }

    /// An approval lands on the session, is handed to an install of the same
    /// role and server without being consumed, and is gone once the install
    /// finishes.
    #[test]
    fn an_approval_is_delivered_cloned_and_spent_on_finish() {
        let store = PairStore::new();
        let generation = store.begin(SERVER.into(), Role::DaemonSystem, started("dc-1"));

        assert_eq!(
            store.token_for(Role::DaemonSystem, SERVER).unwrap_err(),
            "the pairing has not been approved yet"
        );

        let v = store.apply_poll(generation, approved("eyJ.tok.sig"));
        assert_eq!(v.status, PairPollStatus::Approved);
        assert_eq!(v.org_name.as_deref(), Some("Acme"));
        assert_eq!(v.approver_name.as_deref(), Some("Dana"));
        assert_eq!(store.state(), (true, true));

        // Cloned, not consumed: a Retry after a pre-enroll failure reads it
        // again.
        assert_eq!(
            store.token_for(Role::DaemonSystem, SERVER).unwrap(),
            "eyJ.tok.sig"
        );
        assert_eq!(
            store.token_for(Role::DaemonSystem, SERVER).unwrap(),
            "eyJ.tok.sig"
        );

        store.finish();
        assert_eq!(store.state(), (false, false));
        assert!(
            store
                .token_for(Role::DaemonSystem, SERVER)
                .unwrap_err()
                .contains("no pairing in progress")
        );
    }

    /// The token's kind follows the role it was paired for, and a token from
    /// one server enrolls into no other: both mismatches refuse.
    #[test]
    fn the_token_is_refused_for_another_role_or_server() {
        let store = PairStore::new();
        let generation = store.begin(SERVER.into(), Role::TunnelClient, started("dc-1"));
        store.apply_poll(generation, approved("tok"));

        let e = store.token_for(Role::DaemonUser, SERVER).unwrap_err();
        assert!(e.contains("different role"), "{e}");
        let e = store
            .token_for(Role::TunnelClient, "https://other.invalid")
            .unwrap_err();
        assert!(e.contains("different server"), "{e}");
        assert_eq!(store.token_for(Role::TunnelClient, SERVER).unwrap(), "tok");
    }

    /// A fresh start replaces the session — the old device code and its
    /// delivered token are dropped — and a poll that was in flight for the
    /// old session cannot land on the new one.
    #[test]
    fn a_new_start_replaces_the_session_and_stale_polls_are_ignored() {
        let store = PairStore::new();
        let first = store.begin(SERVER.into(), Role::TunnelClient, started("dc-1"));
        store.apply_poll(first, approved("old-token"));
        assert_eq!(store.state(), (true, true));

        let second = store.begin(SERVER.into(), Role::TunnelClient, started("dc-2"));
        assert_ne!(first, second);
        assert_eq!(store.state(), (true, false), "the old token is gone");
        assert_eq!(store.snapshot().unwrap().device_code.expose(), "dc-2");

        // The old loop's late answers: an approval must not deliver into the
        // new session, and a denial must not end it.
        let v = store.apply_poll(first, approved("stale-token"));
        assert_eq!(v.status, PairPollStatus::ExpiredToken);
        assert_eq!(store.state(), (true, false));
        let v = store.apply_poll(first, PollOutcome::AccessDenied);
        assert_eq!(v.status, PairPollStatus::ExpiredToken);
        assert_eq!(store.state(), (true, false));

        // The new loop's own answers apply.
        let v = store.apply_poll(second, PollOutcome::AuthorizationPending);
        assert_eq!(v.status, PairPollStatus::AuthorizationPending);
        let v = store.apply_poll(second, approved("new-token"));
        assert_eq!(v.status, PairPollStatus::Approved);
        assert_eq!(
            store.token_for(Role::TunnelClient, SERVER).unwrap(),
            "new-token"
        );
    }

    /// A denial or an expiry ends the session; `slow_down` carries the new
    /// interval and leaves it; an unknown status carries the spelling and
    /// leaves it.
    #[test]
    fn terminal_answers_end_the_session_and_the_others_keep_it() {
        let store = PairStore::new();
        let generation = store.begin(SERVER.into(), Role::DaemonMachine, started("dc-1"));

        let v = store.apply_poll(generation, PollOutcome::SlowDown { interval: 10 });
        assert_eq!(v.status, PairPollStatus::SlowDown);
        assert_eq!(v.interval, Some(10));
        assert_eq!(store.state(), (true, false));

        let v = store.apply_poll(generation, PollOutcome::Unknown("later".into()));
        assert_eq!(v.status, PairPollStatus::Unknown);
        assert_eq!(v.detail.as_deref(), Some("later"));
        assert_eq!(store.state(), (true, false));

        let v = store.apply_poll(generation, PollOutcome::AccessDenied);
        assert_eq!(v.status, PairPollStatus::AccessDenied);
        assert_eq!(store.state(), (false, false));

        let generation = store.begin(SERVER.into(), Role::DaemonMachine, started("dc-2"));
        let v = store.apply_poll(generation, PollOutcome::ExpiredToken);
        assert_eq!(v.status, PairPollStatus::ExpiredToken);
        assert_eq!(store.state(), (false, false));
    }

    #[test]
    fn cancel_forgets_everything() {
        let store = PairStore::new();
        let generation = store.begin(SERVER.into(), Role::TunnelClient, started("dc-1"));
        store.apply_poll(generation, approved("tok"));
        store.cancel();
        assert_eq!(store.state(), (false, false));
        assert!(store.snapshot().is_none());
        assert!(store.verification_uri().is_none());
        // Cancelling nothing is fine too.
        store.cancel();
        assert_eq!(store.state(), (false, false));
    }

    /// AC3 — the store's `Debug` (what a panic message or a log line would
    /// print) carries neither the device code nor the token.
    #[test]
    fn debug_output_carries_no_secret() {
        let store = PairStore::new();
        let generation = store.begin(SERVER.into(), Role::TunnelClient, started("dc-SECRET"));
        store.apply_poll(generation, approved("eyJ.TOKEN.sig"));
        let text = format!("{store:?}");
        assert!(!text.contains("SECRET"), "{text}");
        assert!(!text.contains("TOKEN"), "{text}");
        assert!(text.contains("<redacted>"), "{text}");
        let text = format!("{:?}", store.snapshot().unwrap());
        assert!(!text.contains("SECRET"), "{text}");
    }

    /// The SPA binds camelCase keys and snake_case statuses.
    #[test]
    fn poll_view_serialises_camel_case_with_snake_case_statuses() {
        let v = PairPollView {
            status: PairPollStatus::SlowDown,
            interval: Some(10),
            org_name: Some("Acme".into()),
            approver_name: Some("Dana".into()),
            detail: None,
        };
        let json = serde_json::to_string(&v).unwrap();
        assert!(json.contains(r#""status":"slow_down""#), "{json}");
        assert!(json.contains(r#""interval":10"#), "{json}");
        assert!(json.contains(r#""orgName":"Acme""#), "{json}");
        assert!(json.contains(r#""approverName":"Dana""#), "{json}");
        assert!(!json.contains("org_name"), "{json}");
        for (status, wire) in [
            (
                PairPollStatus::AuthorizationPending,
                "authorization_pending",
            ),
            (PairPollStatus::Approved, "approved"),
            (PairPollStatus::AccessDenied, "access_denied"),
            (PairPollStatus::ExpiredToken, "expired_token"),
            (PairPollStatus::Unknown, "unknown"),
        ] {
            assert_eq!(
                serde_json::to_string(&status).unwrap(),
                format!("\"{wire}\"")
            );
        }
    }
}
