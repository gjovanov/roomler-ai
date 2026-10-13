// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-101 P1 — the remote-desktop session's transport hardening.
//!
//! Two settings on every session's `SettingEngine`, each with its own way
//! back:
//!
//! | Setting | Default | Why | Undo |
//! |---|---|---|---|
//! | SRTP + SRTCP replay window | 64 | Without one, webrtc-srtp 0.14 falls back to `srtp_no_replay_protection()`: an on-path attacker can replay a captured SRTCP PLI (forcing an IDR) or REMB (capping the rate) without decrypting anything | `ROOMLERD_SRTP_REPLAY_WINDOW=0` |
//! | DTLS cipher suites | the two ECDHE-AES-128-GCM-SHA256 suites | webrtc-dtls' default list still offers ECDHE-AES-256-CBC-SHA. Every browser we support negotiates GCM | `ROOMLERD_DTLS_LEGACY_SUITES=1` |
//!
//! Only the remote-desktop peer reads this. Tunnel and overlay peers build
//! their own engines and are unchanged.

use tunnel_core::env::node_env;
use webrtc::api::setting_engine::SettingEngine;
use webrtc::dtls::cipher_suite::CipherSuiteId;

/// The replay window when nothing says otherwise. libwebrtc's own default.
pub const DEFAULT_REPLAY_WINDOW: usize = 64;
/// The largest window an operator may ask for.
const MAX_REPLAY_WINDOW: usize = 4096;

/// The suites a session offers and accepts unless the legacy switch is on.
pub const GCM_SUITES: [CipherSuiteId; 2] = [
    CipherSuiteId::Tls_Ecdhe_Ecdsa_With_Aes_128_Gcm_Sha256,
    CipherSuiteId::Tls_Ecdhe_Rsa_With_Aes_128_Gcm_Sha256,
];

/// What one session's transport is hardened with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    /// SRTP and SRTCP replay window; 0 = off (today's behaviour).
    pub replay_window: usize,
    /// Restrict DTLS to [`GCM_SUITES`]; false = webrtc-dtls' default list.
    pub gcm_only: bool,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            replay_window: DEFAULT_REPLAY_WINDOW,
            gcm_only: true,
        }
    }
}

fn truthy(v: &str) -> bool {
    let v = v.trim();
    v == "1"
        || v.eq_ignore_ascii_case("true")
        || v.eq_ignore_ascii_case("yes")
        || v.eq_ignore_ascii_case("on")
}

impl Policy {
    /// The policy from the two knobs, as raw strings (`None` = unset). An
    /// unparseable window keeps the default: a typo must not switch replay
    /// protection off.
    pub fn parse(replay_window: Option<&str>, legacy_suites: Option<&str>) -> Self {
        let replay_window = replay_window
            .and_then(|v| v.trim().parse::<usize>().ok())
            .map(|n| n.min(MAX_REPLAY_WINDOW))
            .unwrap_or(DEFAULT_REPLAY_WINDOW);
        Self {
            replay_window,
            gcm_only: !legacy_suites.is_some_and(truthy),
        }
    }

    /// The policy from `ROOMLERD_SRTP_REPLAY_WINDOW` and
    /// `ROOMLERD_DTLS_LEGACY_SUITES` (or their config fallbacks).
    pub fn from_env() -> Self {
        Self::parse(
            node_env("SRTP_REPLAY_WINDOW").as_deref(),
            node_env("DTLS_LEGACY_SUITES").as_deref(),
        )
    }

    /// Apply it to a session's engine.
    pub fn apply(&self, setting: &mut SettingEngine) {
        if self.replay_window > 0 {
            setting.set_srtp_replay_protection_window(self.replay_window);
            setting.set_srtcp_replay_protection_window(self.replay_window);
        }
        if self.gcm_only {
            setting.set_dtls_cipher_suites(GCM_SUITES.to_vec());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use webrtc::srtp::context::Context;
    use webrtc::srtp::option::srtcp_replay_protection;
    use webrtc::srtp::protection_profile::ProtectionProfile;

    #[test]
    fn the_defaults_harden_and_the_knobs_undo_it() {
        assert_eq!(Policy::parse(None, None), Policy::default());
        assert_eq!(Policy::default().replay_window, 64);
        assert!(Policy::default().gcm_only);
        // Each switch undoes exactly its own setting.
        assert_eq!(Policy::parse(Some("0"), None).replay_window, 0);
        assert!(Policy::parse(Some("0"), None).gcm_only);
        assert!(!Policy::parse(None, Some("1")).gcm_only);
        assert_eq!(Policy::parse(None, Some("true")).replay_window, 64);
        assert!(
            Policy::parse(None, Some("0")).gcm_only,
            "0 is not 'legacy on'"
        );
    }

    #[test]
    fn a_typo_never_switches_protection_off_and_a_huge_window_is_capped() {
        assert_eq!(Policy::parse(Some("sixty-four"), None).replay_window, 64);
        assert_eq!(Policy::parse(Some(""), None).replay_window, 64);
        assert_eq!(Policy::parse(Some(" 256 "), None).replay_window, 256);
        assert_eq!(
            Policy::parse(Some("1000000"), None).replay_window,
            MAX_REPLAY_WINDOW
        );
    }

    #[test]
    fn the_gcm_suites_are_exactly_the_two_aes_128_gcm_ones() {
        assert_eq!(GCM_SUITES.len(), 2);
        for s in GCM_SUITES {
            let name = s.to_string();
            assert!(name.contains("GCM"), "{name}");
            assert!(!name.contains("CBC"), "{name}");
        }
    }

    /// One SRTCP receiver context, with or without a replay window, keyed like
    /// its sender.
    fn rtcp_pair(window: Option<usize>) -> (Context, Context) {
        let key = [0x11u8; 16];
        let salt = [0x22u8; 14];
        let profile = ProtectionProfile::AeadAes128Gcm;
        let sender = Context::new(&key, &salt[..12], profile, None, None).unwrap();
        let receiver = Context::new(
            &key,
            &salt[..12],
            profile,
            None,
            window.map(srtcp_replay_protection),
        )
        .unwrap();
        (sender, receiver)
    }

    /// A minimal RTCP packet: a picture-loss indication.
    fn pli() -> Vec<u8> {
        use webrtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
        use webrtc::util::marshal::Marshal;
        PictureLossIndication {
            sender_ssrc: 0x1234_5678,
            media_ssrc: 0x8765_4321,
        }
        .marshal()
        .unwrap()
        .to_vec()
    }

    #[test]
    fn without_a_window_a_replayed_pli_is_accepted_twice() {
        // The red half: this is what every session ran with before FR-101.
        let (mut tx, mut rx) = rtcp_pair(None);
        let wire = tx.encrypt_rtcp(&pli()).unwrap();
        assert!(rx.decrypt_rtcp(&wire).is_ok());
        assert!(
            rx.decrypt_rtcp(&wire).is_ok(),
            "no window: the replay is accepted"
        );
    }

    #[test]
    fn with_the_window_the_replay_is_refused_and_fresh_packets_still_pass() {
        let (mut tx, mut rx) = rtcp_pair(Some(Policy::default().replay_window));
        let first = tx.encrypt_rtcp(&pli()).unwrap();
        assert!(rx.decrypt_rtcp(&first).is_ok());
        assert!(
            rx.decrypt_rtcp(&first).is_err(),
            "the window refuses the replay"
        );
        let second = tx.encrypt_rtcp(&pli()).unwrap();
        assert!(
            rx.decrypt_rtcp(&second).is_ok(),
            "a fresh packet still passes"
        );
    }
}
