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
//! ([`PairStore::tokens_for`]) and cleared when the install **succeeds**
//! ([`PairStore::finish`]): an install that failed before its enroll
//! (a download that broke) can Retry with the same still-valid token,
//! exactly as the pasted path retries with the same pasted one. Delivery is
//! once on the server, so there is no second copy to fetch.
//!
//! **FR-129 B2 — one approval, two tokens.** A macOS install that adds the
//! root half starts its pairing with `halves: 2`, and the session remembers
//! it: the approval then delivers two ordinary single-use tokens, the first
//! for the person's half and the second for the root half
//! ([`InstallTokens`]). An install whose halves are not the pairing's is
//! refused rather than half-served — the SPA cancels the pairing whenever the
//! person goes back to where the choice is made, so a mismatch is a wizard
//! bug, never a choice to guess at.

use std::sync::{Mutex, MutexGuard, OnceLock};

use roomlerd::pairing::{Approved, DeviceCode, PollOutcome, Started};
use serde::Serialize;

use crate::role::Role;

/// What the SPA learns from a poll. `status` is the server's vocabulary
/// plus `rate_limited` (an HTTP 429 from the per-IP governor, handled like
/// `slow_down`) and `unknown` for a spelling this build has not heard of
/// (`detail` then carries the server's own). `interval` rides `slow_down`
/// and `rate_limited` as the NEXT interval in seconds, computed here from
/// the one the SPA polled with — never shorter than it. The org and the
/// approver ride `approved`: each a display name (free text, not unique)
/// beside its unique identifier (the org's slug, the approver's username),
/// which an older server omits.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PairPollView {
    pub status: PairPollStatus,
    pub interval: Option<u64>,
    pub org_name: Option<String>,
    pub org_slug: Option<String>,
    pub approver_name: Option<String>,
    pub approver_username: Option<String>,
    pub detail: Option<String>,
}

/// The poll statuses on the SPA's wire — snake_case, the server's spellings.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PairPollStatus {
    AuthorizationPending,
    SlowDown,
    RateLimited,
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
    /// How many enrollments it was started for (FR-129 B2): `2` when the
    /// install adds the macOS root half, else `1`.
    halves: u8,
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

    /// A pairing the server opened for `halves` enrollments: it replaces
    /// whatever was held — an earlier session's device code and any token it
    /// had delivered are dropped — and returns the new session's generation.
    pub fn begin(&self, server: String, role: Role, halves: u8, started: Started) -> u64 {
        let mut g = self.lock();
        g.generations += 1;
        let generation = g.generations;
        g.session = Some((
            generation,
            PairSession {
                server,
                role,
                halves: halves.max(1),
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
    /// * `slow_down` and a 429 (`rate_limited`) leave it and name the next
    ///   interval, from `polled_interval` (seconds, what the SPA polled
    ///   with): the server's on `slow_down`, `+5 s` or `Retry-After` on a
    ///   429 — never shorter than what was polled with.
    /// * the rest leave the session as it is.
    pub fn apply_poll(
        &self,
        generation: u64,
        outcome: PollOutcome,
        polled_interval: u64,
    ) -> PairPollView {
        use roomlerd::pairing::{interval_after_rate_limit, interval_after_slow_down};
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
                interval: Some(interval_after_slow_down(polled_interval, interval)),
                ..view(PairPollStatus::SlowDown)
            },
            PollOutcome::RateLimited { retry_after } => PairPollView {
                interval: Some(interval_after_rate_limit(polled_interval, retry_after)),
                ..view(PairPollStatus::RateLimited)
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
                    org_slug: approved.org_slug.clone(),
                    approver_name: Some(approved.approver_name.clone()),
                    approver_username: approved.approver_username.clone(),
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

    /// The delivered tokens for an install of `role` against `server`, with
    /// or without the macOS root half, cloned — not consumed, see the module
    /// doc. Refused when nothing was delivered, when the install asks for a
    /// different role or server than the pairing was started for (the
    /// token's kind follows the role, and a token minted by one server
    /// enrolls into no other), and when it asks for other halves than the
    /// pairing covers (FR-129 B2).
    pub fn tokens_for(
        &self,
        role: Role,
        server: &str,
        root_half: bool,
    ) -> Result<InstallTokens, String> {
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
        let wanted = crate::role::pair_halves(root_half);
        if s.halves != wanted {
            return Err(if root_half {
                "the approval covers this Mac's screen half only, and the private network is a \
                 second device that needs its own enrollment — start the pairing again"
                    .to_string()
            } else {
                "the approval covers this Mac's screen and its private network, and this install \
                 is the screen alone — start the pairing again"
                    .to_string()
            });
        }
        if approved.tokens.len() < usize::from(wanted) {
            return Err(format!(
                "the pairing delivered {} enrollment token(s) for {wanted} enrollments — start \
                 the pairing again",
                approved.tokens.len()
            ));
        }
        let main = approved
            .tokens
            .first()
            .cloned()
            .ok_or_else(|| "the pairing delivered no token".to_string())?;
        let root_half = root_half.then(|| approved.tokens[1].clone());
        if root_half.as_deref() == Some(main.as_str()) {
            // A token works once: the second enroll would be refused, after
            // the first had already made its device.
            return Err(
                "the pairing delivered the same token twice — start the pairing again".to_string(),
            );
        }
        Ok(InstallTokens { main, root_half })
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

/// The enrollment tokens one install enrolls with: `main` for the device the
/// role installs, and — FR-129 B2 — `root_half` for the macOS root half
/// beside it. Ordinary single-use tokens, both; `Debug` prints neither.
#[derive(Clone, Eq, PartialEq)]
pub struct InstallTokens {
    pub main: String,
    pub root_half: Option<String>,
}

impl std::fmt::Debug for InstallTokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InstallTokens")
            .field("main", &"<redacted>")
            .field("root_half", &self.root_half.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

fn view(status: PairPollStatus) -> PairPollView {
    PairPollView {
        status,
        interval: None,
        org_name: None,
        org_slug: None,
        approver_name: None,
        approver_username: None,
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
            org_slug: Some("acme".into()),
            approver_name: "Dana".into(),
            approver_username: Some("dana".into()),
        })
    }

    const SERVER: &str = "https://example.invalid";

    /// The interval every test polls with, where it does not matter.
    const POLLED: u64 = 5;

    /// The one token of an install without the root half.
    fn one_token(store: &PairStore, role: Role, server: &str) -> Result<String, String> {
        store.tokens_for(role, server, false).map(|t| {
            assert!(t.root_half.is_none());
            t.main
        })
    }

    fn approved_two(first: &str, second: &str) -> PollOutcome {
        PollOutcome::Approved(Approved {
            tokens: vec![first.into(), second.into()],
            org_name: "Acme".into(),
            org_slug: Some("acme".into()),
            approver_name: "Dana".into(),
            approver_username: Some("dana".into()),
        })
    }

    /// FR-129 B2 — one approval, two tokens: a pairing started for the macOS
    /// root half hands its install the first token for the person's half and
    /// the second for the root half, both cloned (a Retry reads them again),
    /// and never the same one twice.
    #[test]
    fn a_two_half_pairing_hands_out_both_tokens() {
        let store = PairStore::new();
        let generation = store.begin(SERVER.into(), Role::DaemonUser, 2, started("dc-1"));
        assert!(
            store
                .tokens_for(Role::DaemonUser, SERVER, true)
                .unwrap_err()
                .contains("not been approved")
        );
        store.apply_poll(
            generation,
            approved_two("eyJ.user.sig", "eyJ.root.sig"),
            POLLED,
        );
        for _ in 0..2 {
            let t = store.tokens_for(Role::DaemonUser, SERVER, true).unwrap();
            assert_eq!(t.main, "eyJ.user.sig");
            assert_eq!(t.root_half.as_deref(), Some("eyJ.root.sig"));
        }
        // Nothing of either in a log line.
        let t = store.tokens_for(Role::DaemonUser, SERVER, true).unwrap();
        let text = format!("{t:?}");
        assert!(
            !text.contains("user.sig") && !text.contains("root.sig"),
            "{text}"
        );
        assert!(text.contains("<redacted>"), "{text}");
        store.finish();
        assert!(store.tokens_for(Role::DaemonUser, SERVER, true).is_err());
    }

    /// FR-129 B2 — the halves an install asks for must be the pairing's: a
    /// one-half approval never stands in for two (the root half would have no
    /// token), and a two-half one is not quietly spent on one. A server that
    /// delivered fewer tokens than asked is refused too.
    #[test]
    fn an_install_with_other_halves_than_the_pairing_is_refused() {
        let store = PairStore::new();
        let generation = store.begin(SERVER.into(), Role::DaemonUser, 1, started("dc-1"));
        store.apply_poll(generation, approved("eyJ.one.sig"), POLLED);
        let e = store
            .tokens_for(Role::DaemonUser, SERVER, true)
            .unwrap_err();
        assert!(
            e.contains("screen half only") && e.contains("start the pairing again"),
            "{e}"
        );
        assert_eq!(
            one_token(&store, Role::DaemonUser, SERVER).unwrap(),
            "eyJ.one.sig"
        );

        let generation = store.begin(SERVER.into(), Role::DaemonUser, 2, started("dc-2"));
        store.apply_poll(generation, approved_two("eyJ.a.sig", "eyJ.b.sig"), POLLED);
        let e = one_token(&store, Role::DaemonUser, SERVER).unwrap_err();
        assert!(e.contains("screen alone"), "{e}");

        let generation = store.begin(SERVER.into(), Role::DaemonUser, 2, started("dc-3"));
        store.apply_poll(generation, approved("eyJ.only.sig"), POLLED);
        let e = store
            .tokens_for(Role::DaemonUser, SERVER, true)
            .unwrap_err();
        assert!(e.contains("delivered 1 enrollment token"), "{e}");

        let generation = store.begin(SERVER.into(), Role::DaemonUser, 2, started("dc-4"));
        store.apply_poll(generation, approved_two("eyJ.x.sig", "eyJ.x.sig"), POLLED);
        let e = store
            .tokens_for(Role::DaemonUser, SERVER, true)
            .unwrap_err();
        assert!(e.contains("same token twice"), "{e}");
    }

    #[test]
    fn a_start_holds_the_session_and_a_snapshot_copies_what_a_poll_needs() {
        let store = PairStore::new();
        assert!(store.snapshot().is_none());
        assert_eq!(store.state(), (false, false));

        let generation = store.begin(SERVER.into(), Role::TunnelClient, 1, started("dc-1"));
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
        let generation = store.begin(SERVER.into(), Role::DaemonSystem, 1, started("dc-1"));

        assert_eq!(
            one_token(&store, Role::DaemonSystem, SERVER).unwrap_err(),
            "the pairing has not been approved yet"
        );

        let v = store.apply_poll(generation, approved("eyJ.tok.sig"), POLLED);
        assert_eq!(v.status, PairPollStatus::Approved);
        assert_eq!(v.org_name.as_deref(), Some("Acme"));
        assert_eq!(v.org_slug.as_deref(), Some("acme"));
        assert_eq!(v.approver_name.as_deref(), Some("Dana"));
        assert_eq!(v.approver_username.as_deref(), Some("dana"));
        assert_eq!(store.state(), (true, true));

        // An older server's approval, without the unique identifiers, still
        // reaches the SPA — with them absent, not invented.
        let generation = store.begin(SERVER.into(), Role::DaemonSystem, 1, started("dc-2"));
        let v = store.apply_poll(
            generation,
            PollOutcome::Approved(Approved {
                tokens: vec!["eyJ.tok2.sig".into()],
                org_name: "Acme".into(),
                org_slug: None,
                approver_name: "Dana".into(),
                approver_username: None,
            }),
            POLLED,
        );
        assert_eq!(v.status, PairPollStatus::Approved);
        assert_eq!(v.org_name.as_deref(), Some("Acme"));
        assert_eq!(v.org_slug, None);
        assert_eq!(v.approver_username, None);
        let generation = store.begin(SERVER.into(), Role::DaemonSystem, 1, started("dc-1"));
        store.apply_poll(generation, approved("eyJ.tok.sig"), POLLED);

        // Cloned, not consumed: a Retry after a pre-enroll failure reads it
        // again.
        assert_eq!(
            one_token(&store, Role::DaemonSystem, SERVER).unwrap(),
            "eyJ.tok.sig"
        );
        assert_eq!(
            one_token(&store, Role::DaemonSystem, SERVER).unwrap(),
            "eyJ.tok.sig"
        );

        store.finish();
        assert_eq!(store.state(), (false, false));
        assert!(
            one_token(&store, Role::DaemonSystem, SERVER)
                .unwrap_err()
                .contains("no pairing in progress")
        );
    }

    /// The token's kind follows the role it was paired for, and a token from
    /// one server enrolls into no other: both mismatches refuse.
    #[test]
    fn the_token_is_refused_for_another_role_or_server() {
        let store = PairStore::new();
        let generation = store.begin(SERVER.into(), Role::TunnelClient, 1, started("dc-1"));
        store.apply_poll(generation, approved("tok"), POLLED);

        let e = one_token(&store, Role::DaemonUser, SERVER).unwrap_err();
        assert!(e.contains("different role"), "{e}");
        let e = one_token(&store, Role::TunnelClient, "https://other.invalid").unwrap_err();
        assert!(e.contains("different server"), "{e}");
        assert_eq!(
            one_token(&store, Role::TunnelClient, SERVER).unwrap(),
            "tok"
        );
    }

    /// A fresh start replaces the session — the old device code and its
    /// delivered token are dropped — and a poll that was in flight for the
    /// old session cannot land on the new one.
    #[test]
    fn a_new_start_replaces_the_session_and_stale_polls_are_ignored() {
        let store = PairStore::new();
        let first = store.begin(SERVER.into(), Role::TunnelClient, 1, started("dc-1"));
        store.apply_poll(first, approved("old-token"), POLLED);
        assert_eq!(store.state(), (true, true));

        let second = store.begin(SERVER.into(), Role::TunnelClient, 1, started("dc-2"));
        assert_ne!(first, second);
        assert_eq!(store.state(), (true, false), "the old token is gone");
        assert_eq!(store.snapshot().unwrap().device_code.expose(), "dc-2");

        // The old loop's late answers: an approval must not deliver into the
        // new session, and a denial must not end it.
        let v = store.apply_poll(first, approved("stale-token"), POLLED);
        assert_eq!(v.status, PairPollStatus::ExpiredToken);
        assert_eq!(store.state(), (true, false));
        let v = store.apply_poll(first, PollOutcome::AccessDenied, POLLED);
        assert_eq!(v.status, PairPollStatus::ExpiredToken);
        assert_eq!(store.state(), (true, false));

        // The new loop's own answers apply.
        let v = store.apply_poll(second, PollOutcome::AuthorizationPending, POLLED);
        assert_eq!(v.status, PairPollStatus::AuthorizationPending);
        let v = store.apply_poll(second, approved("new-token"), POLLED);
        assert_eq!(v.status, PairPollStatus::Approved);
        assert_eq!(
            one_token(&store, Role::TunnelClient, SERVER).unwrap(),
            "new-token"
        );
    }

    /// A denial or an expiry ends the session; `slow_down` carries the new
    /// interval and leaves it; an unknown status carries the spelling and
    /// leaves it.
    #[test]
    fn terminal_answers_end_the_session_and_the_others_keep_it() {
        let store = PairStore::new();
        let generation = store.begin(SERVER.into(), Role::DaemonMachine, 1, started("dc-1"));

        let v = store.apply_poll(generation, PollOutcome::SlowDown { interval: 10 }, 5);
        assert_eq!(v.status, PairPollStatus::SlowDown);
        assert_eq!(v.interval, Some(10));
        assert_eq!(store.state(), (true, false));
        // Never shorter than what was polled with.
        let v = store.apply_poll(generation, PollOutcome::SlowDown { interval: 10 }, 15);
        assert_eq!(v.interval, Some(15));

        let v = store.apply_poll(generation, PollOutcome::Unknown("later".into()), POLLED);
        assert_eq!(v.status, PairPollStatus::Unknown);
        assert_eq!(v.detail.as_deref(), Some("later"));
        assert_eq!(store.state(), (true, false));

        let v = store.apply_poll(generation, PollOutcome::AccessDenied, POLLED);
        assert_eq!(v.status, PairPollStatus::AccessDenied);
        assert_eq!(store.state(), (false, false));

        let generation = store.begin(SERVER.into(), Role::DaemonMachine, 1, started("dc-2"));
        let v = store.apply_poll(generation, PollOutcome::ExpiredToken, POLLED);
        assert_eq!(v.status, PairPollStatus::ExpiredToken);
        assert_eq!(store.state(), (false, false));
    }

    /// A 429 from the per-IP governor is handled like a `slow_down`: the
    /// session stays, and the next interval is the polled one plus 5 s, or
    /// `Retry-After` when that is longer — never shorter.
    #[test]
    fn a_rate_limit_backs_off_like_a_slow_down_and_keeps_the_session() {
        let store = PairStore::new();
        let generation = store.begin(SERVER.into(), Role::TunnelClient, 1, started("dc-1"));

        let v = store.apply_poll(
            generation,
            PollOutcome::RateLimited { retry_after: None },
            5,
        );
        assert_eq!(v.status, PairPollStatus::RateLimited);
        assert_eq!(v.interval, Some(10));
        assert_eq!(store.state(), (true, false));

        let v = store.apply_poll(
            generation,
            PollOutcome::RateLimited {
                retry_after: Some(30),
            },
            10,
        );
        assert_eq!(v.interval, Some(30), "Retry-After is honoured");

        let v = store.apply_poll(
            generation,
            PollOutcome::RateLimited {
                retry_after: Some(3),
            },
            30,
        );
        assert_eq!(v.interval, Some(35), "a short Retry-After never shrinks it");

        // The pairing itself is untouched: an approval still lands.
        let v = store.apply_poll(generation, approved("tok"), 35);
        assert_eq!(v.status, PairPollStatus::Approved);
        assert_eq!(
            one_token(&store, Role::TunnelClient, SERVER).unwrap(),
            "tok"
        );
    }

    #[test]
    fn cancel_forgets_everything() {
        let store = PairStore::new();
        let generation = store.begin(SERVER.into(), Role::TunnelClient, 1, started("dc-1"));
        store.apply_poll(generation, approved("tok"), POLLED);
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
        let generation = store.begin(SERVER.into(), Role::TunnelClient, 1, started("dc-SECRET"));
        store.apply_poll(generation, approved("eyJ.TOKEN.sig"), POLLED);
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
            org_slug: Some("acme".into()),
            approver_name: Some("Dana".into()),
            approver_username: Some("dana".into()),
            detail: None,
        };
        let json = serde_json::to_string(&v).unwrap();
        assert!(json.contains(r#""status":"slow_down""#), "{json}");
        assert!(json.contains(r#""interval":10"#), "{json}");
        assert!(json.contains(r#""orgName":"Acme""#), "{json}");
        assert!(json.contains(r#""orgSlug":"acme""#), "{json}");
        assert!(json.contains(r#""approverName":"Dana""#), "{json}");
        assert!(json.contains(r#""approverUsername":"dana""#), "{json}");
        assert!(!json.contains("org_name"), "{json}");
        assert!(!json.contains("approver_username"), "{json}");
        for (status, wire) in [
            (
                PairPollStatus::AuthorizationPending,
                "authorization_pending",
            ),
            (PairPollStatus::RateLimited, "rate_limited"),
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
