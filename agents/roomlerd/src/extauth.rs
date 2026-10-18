// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-52 P3c — the device's answer to `rc:extauth.ke1` and `rc:extauth.ke3`.
//!
//! Compiled into EVERY build, credential stack or not, because every build
//! advertises `RpcCap::ExternalAccess` (P2a) and so can be sent a KE1. A build
//! that cannot hold a password must still ANSWER — `unavailable`, at once — or
//! the server waits out its whole bound and the outsider is told nothing for
//! seconds. Before this module, `handle_server_msg`'s catch-all dropped the
//! frame at `debug!`.
//!
//! Every refusal that concerns the device's configuration is the same
//! `unavailable`: not the primary org, gate 3 off, no password, an unreadable
//! record, a build without the stack. Telling them apart would let whoever
//! holds the connect code — which is dictated aloud — read the device's setup.

use roomler_ai_remote_control::models::ExtauthRefusal;
use roomler_ai_remote_control::signaling::ClientMsg;

use crate::remote_config::RemoteConfigServices;

fn refused(attempt_id: String, why: ExtauthRefusal, retry_after_secs: Option<u32>) -> ClientMsg {
    ClientMsg::ExtauthOutcome {
        attempt_id,
        refused: Some(why),
        retry_after_secs,
    }
}

/// `rc:extauth.ke1` → the frame to send back: `rc:extauth.ke2`, or an outcome
/// refusing it.
pub async fn on_ke1(
    is_primary: bool,
    remote_cfg: &RemoteConfigServices,
    attempt_id: String,
    principal: String,
    ke1: &str,
) -> ClientMsg {
    #[cfg(feature = "external-access")]
    {
        imp::on_ke1(
            remote_cfg.external_logins(),
            is_primary,
            remote_cfg,
            attempt_id,
            principal,
            ke1,
            std::time::Instant::now(),
        )
        .await
    }
    #[cfg(not(feature = "external-access"))]
    {
        let _ = (is_primary, remote_cfg, principal, ke1);
        tracing::info!(%attempt_id, "extauth: KE1 refused — this build cannot hold a password");
        refused(attempt_id, ExtauthRefusal::Unavailable, None)
    }
}

/// `rc:extauth.ke3` → the outcome to send back.
pub async fn on_ke3(
    is_primary: bool,
    remote_cfg: &RemoteConfigServices,
    attempt_id: String,
    principal: String,
    ke3: &str,
) -> ClientMsg {
    #[cfg(feature = "external-access")]
    {
        imp::on_ke3(
            remote_cfg.external_logins(),
            is_primary,
            remote_cfg,
            attempt_id,
            principal,
            ke3,
            std::time::Instant::now(),
        )
        .await
    }
    #[cfg(not(feature = "external-access"))]
    {
        let _ = (is_primary, remote_cfg, principal, ke3);
        refused(attempt_id, ExtauthRefusal::Unavailable, None)
    }
}

#[cfg(feature = "external-access")]
mod imp {
    use std::time::{Duration, Instant};

    use base64::Engine as _;
    use roomler_ai_remote_control::models::ExtauthRefusal;
    use roomler_ai_remote_control::signaling::ClientMsg;
    use tracing::info;

    use super::refused;
    use crate::external_logins::{ExternalLogins, Refusal};
    use crate::remote_config::RemoteConfigServices;

    /// `@serenity-kit/opaque`'s encoding, strictly: a padded or standard-alphabet
    /// blob is a client this protocol does not speak, and is refused as
    /// malformed before anything is answered or debited.
    fn b64url() -> base64::engine::general_purpose::GeneralPurpose {
        base64::engine::general_purpose::URL_SAFE_NO_PAD
    }

    /// Whole seconds, rounded UP: a client told "retry in 29 s" when 29.4 s
    /// remain would retry into a second refusal.
    fn retry_secs(d: Duration) -> u32 {
        let secs = d.as_secs() + u64::from(d.subsec_nanos() > 0);
        u32::try_from(secs).unwrap_or(u32::MAX)
    }

    fn map(r: Refusal) -> (ExtauthRefusal, Option<u32>) {
        match r {
            Refusal::Throttled { retry_after } => {
                (ExtauthRefusal::Throttled, Some(retry_secs(retry_after)))
            }
            Refusal::Busy => (ExtauthRefusal::Busy, None),
            // A malformed attempt id or principal came from the SERVER, not the
            // outsider, but it is the same answer to them: the request could
            // not be read.
            Refusal::BadIdentifier | Refusal::Malformed => (ExtauthRefusal::Malformed, None),
            Refusal::Unavailable => (ExtauthRefusal::Unavailable, None),
            Refusal::UnknownAttempt => (ExtauthRefusal::UnknownAttempt, None),
            Refusal::PrincipalMismatch | Refusal::Rejected => (ExtauthRefusal::Rejected, None),
        }
    }

    pub(super) async fn on_ke1(
        logins: &ExternalLogins,
        is_primary: bool,
        remote_cfg: &RemoteConfigServices,
        attempt_id: String,
        principal: String,
        ke1: &str,
        now: Instant,
    ) -> ClientMsg {
        if !is_primary {
            info!(%attempt_id, "extauth: KE1 refused — not the primary org's socket");
            return refused(attempt_id, ExtauthRefusal::Unavailable, None);
        }
        // Gate 3 and the record, from the FILE: a revocation takes effect for
        // the next knock, not the next restart.
        let Some(cred) = remote_cfg.external_access_live().await else {
            info!(%attempt_id, "extauth: KE1 refused — external access is not enabled here");
            return refused(attempt_id, ExtauthRefusal::Unavailable, None);
        };
        let Ok(ke1) = b64url().decode(ke1) else {
            return refused(attempt_id, ExtauthRefusal::Malformed, None);
        };
        match logins.begin(&attempt_id, &principal, &cred, &ke1, now) {
            Ok(ke2) => {
                info!(%attempt_id, %principal, "extauth: KE1 answered (one guess spent)");
                ClientMsg::ExtauthKe2 {
                    attempt_id,
                    ke2: b64url().encode(ke2),
                }
            }
            Err(r) => {
                info!(%attempt_id, %principal, refusal = ?r, "extauth: KE1 refused");
                let (why, retry) = map(r);
                refused(attempt_id, why, retry)
            }
        }
    }

    pub(super) async fn on_ke3(
        logins: &ExternalLogins,
        is_primary: bool,
        remote_cfg: &RemoteConfigServices,
        attempt_id: String,
        principal: String,
        ke3: &str,
        now: Instant,
    ) -> ClientMsg {
        if !is_primary {
            return refused(attempt_id, ExtauthRefusal::Unavailable, None);
        }
        // Re-checked at KE3: an owner who switches external access off while a
        // login is in flight must not see it complete a moment later.
        if remote_cfg.external_access_live().await.is_none() {
            info!(%attempt_id, "extauth: KE3 refused — external access was switched off mid-login");
            return refused(attempt_id, ExtauthRefusal::Unavailable, None);
        }
        let Ok(ke3) = b64url().decode(ke3) else {
            return refused(attempt_id, ExtauthRefusal::Malformed, None);
        };
        match logins.finish(&attempt_id, &principal, &ke3, now) {
            Ok(()) => {
                info!(%attempt_id, %principal, "extauth: login VERIFIED");
                ClientMsg::ExtauthOutcome {
                    attempt_id,
                    refused: None,
                    retry_after_secs: None,
                }
            }
            Err(r) => {
                info!(%attempt_id, %principal, refusal = ?r, "extauth: KE3 refused");
                let (why, retry) = map(r);
                refused(attempt_id, why, retry)
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::external_access::{Suite, set_password};
        use opaque_ke::{ClientLogin, ClientLoginFinishParameters, CredentialResponse};
        use rand_opaque::rngs::OsRng;

        const WHO: &str = "665f1c2a9b3e4d5f6a7b8c9d";

        fn pw(tag: &str) -> String {
            format!("fr52-not-a-credential-{tag}")
        }

        /// A daemon's config on disk, with gate 3 and gate 4 as asked.
        fn device(
            enabled: bool,
            password: Option<&str>,
        ) -> (tempfile::TempDir, RemoteConfigServices) {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("config.toml");
            let mut cfg = crate::config::test_fixture();
            cfg.external_access_enabled = enabled;
            if let Some(p) = password {
                let (cred, _) = set_password(None, p).unwrap();
                cfg.external_access_setup = Some(cred.setup);
                cfg.external_access_verifier = Some(cred.verifier);
            }
            crate::config::save(&path, &cfg).unwrap();
            let svc = RemoteConfigServices::new(
                path,
                std::sync::Arc::new(tokio::sync::Mutex::new(())),
                false,
                false,
            );
            (dir, svc)
        }

        fn client_ke1(password: &str) -> (ClientLogin<Suite>, String) {
            let s = ClientLogin::<Suite>::start(&mut OsRng, password.as_bytes()).unwrap();
            let ke1 = b64url().encode(s.message.serialize());
            (s.state, ke1)
        }

        fn refusal_of(m: &ClientMsg) -> Option<(ExtauthRefusal, Option<u32>)> {
            match m {
                ClientMsg::ExtauthOutcome {
                    refused: Some(r),
                    retry_after_secs,
                    ..
                } => Some((*r, *retry_after_secs)),
                _ => None,
            }
        }

        /// The whole device-side exchange: KE1 → KE2 → KE3 → verified, with
        /// the blobs in the browser library's encoding throughout.
        #[tokio::test]
        async fn a_correct_login_is_answered_and_verified() {
            let logins = ExternalLogins::new();
            let (_dir, svc) = device(true, Some(&pw("right")));
            let now = Instant::now();

            let (state, ke1) = client_ke1(&pw("right"));
            let reply = on_ke1(&logins, true, &svc, "a1".into(), WHO.into(), &ke1, now).await;
            let ClientMsg::ExtauthKe2 { attempt_id, ke2 } = reply else {
                panic!("expected a KE2, got {reply:?}");
            };
            assert_eq!(attempt_id, "a1");

            let ke2 = b64url().decode(ke2).expect("KE2 is base64url, no padding");
            let done = state
                .finish(
                    &mut OsRng,
                    pw("right").as_bytes(),
                    CredentialResponse::<Suite>::deserialize(&ke2).unwrap(),
                    ClientLoginFinishParameters::default(),
                )
                .expect("the right password opens KE2");
            let ke3 = b64url().encode(done.message.serialize());

            let outcome = on_ke3(&logins, true, &svc, "a1".into(), WHO.into(), &ke3, now).await;
            assert!(
                matches!(outcome, ClientMsg::ExtauthOutcome { refused: None, .. }),
                "expected VERIFIED, got {outcome:?}"
            );
            assert!(logins.take_verified("a1", WHO, now).is_some());
        }

        /// Every configuration refusal is the same `unavailable`, and none of
        /// them spends a guess — nothing was answered.
        #[tokio::test]
        async fn every_configuration_refusal_is_the_same_unavailable() {
            let now = Instant::now();
            let (_, ke1) = client_ke1(&pw("right"));
            for (label, primary, enabled, password) in [
                ("secondary org", false, true, Some(pw("right"))),
                ("gate 3 off", true, false, Some(pw("right"))),
                ("no password", true, true, None),
            ] {
                let logins = ExternalLogins::new();
                let (_dir, svc) = device(enabled, password.as_deref());
                let reply =
                    on_ke1(&logins, primary, &svc, "a1".into(), WHO.into(), &ke1, now).await;
                assert_eq!(
                    refusal_of(&reply),
                    Some((ExtauthRefusal::Unavailable, None)),
                    "{label}"
                );
                assert_eq!(
                    logins.answered_guesses(now),
                    0,
                    "{label}: nothing was answered"
                );
            }
        }

        /// Revocation is live: an owner who switches external access off
        /// between KE1 and KE3 does not see the login complete.
        #[tokio::test]
        async fn switching_gate_3_off_mid_login_refuses_the_ke3() {
            let logins = ExternalLogins::new();
            let (dir, svc) = device(true, Some(&pw("right")));
            let now = Instant::now();

            let (state, ke1) = client_ke1(&pw("right"));
            let ClientMsg::ExtauthKe2 { ke2, .. } =
                on_ke1(&logins, true, &svc, "a1".into(), WHO.into(), &ke1, now).await
            else {
                panic!("expected a KE2");
            };

            // The owner runs `roomler config set external_access_enabled false`.
            let path = dir.path().join("config.toml");
            let mut cfg = crate::config::load(&path).unwrap();
            cfg.external_access_enabled = false;
            crate::config::save(&path, &cfg).unwrap();

            let ke2 = b64url().decode(ke2).unwrap();
            let done = state
                .finish(
                    &mut OsRng,
                    pw("right").as_bytes(),
                    CredentialResponse::<Suite>::deserialize(&ke2).unwrap(),
                    ClientLoginFinishParameters::default(),
                )
                .unwrap();
            let ke3 = b64url().encode(done.message.serialize());
            let outcome = on_ke3(&logins, true, &svc, "a1".into(), WHO.into(), &ke3, now).await;
            assert_eq!(
                refusal_of(&outcome),
                Some((ExtauthRefusal::Unavailable, None))
            );
            assert!(logins.take_verified("a1", WHO, now).is_none());
        }

        /// The throttle's wait reaches the outsider, rounded UP.
        #[tokio::test]
        async fn a_throttled_device_says_when_to_retry() {
            let logins = ExternalLogins::new();
            let (_dir, svc) = device(true, Some(&pw("right")));
            let now = Instant::now();
            for n in 0..crate::external_logins::FREE_GUESSES {
                let (_, ke1) = client_ke1(&pw("wrong"));
                let r = on_ke1(&logins, true, &svc, format!("a{n}"), WHO.into(), &ke1, now).await;
                assert!(matches!(r, ClientMsg::ExtauthKe2 { .. }));
            }
            let (_, ke1) = client_ke1(&pw("wrong"));
            let r = on_ke1(&logins, true, &svc, "a99".into(), WHO.into(), &ke1, now).await;
            assert_eq!(refusal_of(&r), Some((ExtauthRefusal::Throttled, Some(30))));
            assert_eq!(retry_secs(Duration::from_millis(29_400)), 30, "rounded up");
        }

        /// Not the browser library's encoding → malformed, and not a guess.
        #[tokio::test]
        async fn a_ke1_in_the_wrong_encoding_is_malformed_and_free() {
            let logins = ExternalLogins::new();
            let (_dir, svc) = device(true, Some(&pw("right")));
            let now = Instant::now();
            let (_, ke1) = client_ke1(&pw("right"));
            let padded = format!("{ke1}==");
            let r = on_ke1(&logins, true, &svc, "a1".into(), WHO.into(), &padded, now).await;
            assert_eq!(refusal_of(&r), Some((ExtauthRefusal::Malformed, None)));
            assert_eq!(logins.answered_guesses(now), 0);
        }
    }
}
