// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-97 P2f — may a recording open the microphone on a Mac?
//!
//! macOS records SILENCE from a microphone the person has not allowed: the
//! stream opens and runs, and the file carries nothing. A recording that
//! silently lacks what the person asked for is worse than a refusal that
//! names why (the recorder's own rule, `child::open_audio`), so the recorder
//! asks AVFoundation first and refuses where the answer is already no.
//!
//! A microphone nobody has decided about yet is opened: macOS asks the person
//! at that moment, naming the app from the bundle's
//! `NSMicrophoneUsageDescription`, and the track is silent until they answer.
//! Opening it without that key ends the process, which is why the key ships
//! in the same change as this code.
//!
//! ⚠️ **Except on a supervised Mac** (P2f-1b, measured on the operator's
//! MacBook, 2026-10-24): the GUI worker is started by the ROOT daemon, which
//! stays the responsible process of everything the worker starts, and macOS
//! never asks the person about a microphone for root. The question answered
//! "not determined", the real microphone opened, no prompt ever appeared, and
//! three minutes of recording were silence. There, every answer refuses by
//! name (the manager tells the recorder, [`SUPERVISED_ENV`]) until the worker
//! is its own responsible process (P2f-2).
//!
//! The decision is plain data so it is tested on every OS (FR-43's practice);
//! only the question itself is macOS's.

/// Set (to `1`) by a supervised Mac worker's recording manager on its
/// recorder: the microphone's question would be asked for the root daemon.
pub const SUPERVISED_ENV: &str = "ROOMLERD_MACOS_SUPERVISED_RECORDER";

/// What a supervised Mac's recorder answers, whatever the permission says.
pub const SUPERVISED_WORDS: &str = "this Mac can't record the microphone yet: Roomler runs here \
     under its system service, and macOS won't ask you about the microphone for it";

/// `AVAuthorizationStatus`, as AVFoundation numbers it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MicPermission {
    /// Nobody has decided: opening the microphone makes macOS ask.
    NotDetermined,
    /// A configuration profile or parental controls forbid it; the person
    /// cannot change it.
    Restricted,
    /// The person turned it off for this app.
    Denied,
    Authorized,
    /// A value this build does not know, or no answer at all.
    Unknown(isize),
}

impl MicPermission {
    pub fn from_raw(raw: isize) -> Self {
        match raw {
            0 => Self::NotDetermined,
            1 => Self::Restricted,
            2 => Self::Denied,
            3 => Self::Authorized,
            other => Self::Unknown(other),
        }
    }

    /// `Err(words)` where opening the microphone would record silence.
    /// `Unknown` opens it: an answer this build cannot read is not a "no",
    /// and the stream then behaves exactly as it would have before P2f.
    pub fn verdict(self) -> Result<(), &'static str> {
        match self {
            Self::Denied => Err(
                "the microphone is turned off for Roomler Daemon: allow it in System Settings › \
                 Privacy & Security › Microphone",
            ),
            Self::Restricted => Err(
                "the microphone is restricted on this Mac by a configuration profile or parental \
                 controls",
            ),
            Self::NotDetermined | Self::Authorized | Self::Unknown(_) => Ok(()),
        }
    }

    /// [`Self::verdict`], unless the recorder runs under a supervised Mac's
    /// worker: there even "authorized" is the person's answer for an app
    /// macOS does not judge this microphone by, so every answer refuses.
    pub fn verdict_where(self, supervised: bool) -> Result<(), &'static str> {
        if supervised {
            return Err(SUPERVISED_WORDS);
        }
        self.verdict()
    }
}

/// Ask AVFoundation, and refuse where the microphone would record silence.
#[cfg(all(target_os = "macos", feature = "audio"))]
pub fn preflight() -> anyhow::Result<()> {
    let permission = MicPermission::from_raw(authorization_status());
    let supervised = std::env::var_os(SUPERVISED_ENV).is_some_and(|v| v == "1");
    tracing::info!(
        ?permission,
        supervised,
        "recording: the microphone's permission"
    );
    permission
        .verdict_where(supervised)
        .map_err(|words| anyhow::anyhow!(words))
}

/// `[AVCaptureDevice authorizationStatusForMediaType:AVMediaTypeAudio]`,
/// or -1 where the class is missing. Asking never prompts the person.
#[cfg(all(target_os = "macos", feature = "audio"))]
fn authorization_status() -> isize {
    use objc2::msg_send;
    use objc2::runtime::AnyClass;
    use objc2_foundation::NSString;

    #[link(name = "AVFoundation", kind = "framework")]
    unsafe extern "C" {
        static AVMediaTypeAudio: &'static NSString;
    }

    let Some(class) = AnyClass::get(c"AVCaptureDevice") else {
        return -1;
    };
    // SAFETY: a class method taking an `AVMediaType` (an `NSString *`) and
    // returning an `NSInteger`; `AVMediaTypeAudio` is a constant AVFoundation
    // exports for exactly this argument.
    unsafe { msg_send![class, authorizationStatusForMediaType: AVMediaTypeAudio] }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn avfoundation_numbers_map_to_their_meaning() {
        assert_eq!(MicPermission::from_raw(0), MicPermission::NotDetermined);
        assert_eq!(MicPermission::from_raw(1), MicPermission::Restricted);
        assert_eq!(MicPermission::from_raw(2), MicPermission::Denied);
        assert_eq!(MicPermission::from_raw(3), MicPermission::Authorized);
        assert_eq!(MicPermission::from_raw(-1), MicPermission::Unknown(-1));
        assert_eq!(MicPermission::from_raw(7), MicPermission::Unknown(7));
    }

    #[test]
    fn a_microphone_that_would_record_silence_is_refused_by_name() {
        let denied = MicPermission::Denied.verdict().unwrap_err();
        assert!(
            denied.contains("Privacy & Security › Microphone"),
            "{denied}"
        );
        let restricted = MicPermission::Restricted.verdict().unwrap_err();
        assert!(restricted.contains("restricted"), "{restricted}");
    }

    #[test]
    fn an_undecided_or_allowed_microphone_opens() {
        // Undecided: opening it is what makes macOS ask the person.
        assert_eq!(MicPermission::NotDetermined.verdict(), Ok(()));
        assert_eq!(MicPermission::Authorized.verdict(), Ok(()));
        // An answer this build cannot read is not a "no".
        assert_eq!(MicPermission::Unknown(-1).verdict(), Ok(()));
    }

    #[test]
    fn a_supervised_macs_recorder_refuses_every_answer() {
        // Measured on the operator's MacBook: "not determined", no prompt,
        // three minutes of silence. "Authorized" is the person's answer, and
        // macOS judges this microphone by the root daemon instead.
        for p in [
            MicPermission::NotDetermined,
            MicPermission::Authorized,
            MicPermission::Denied,
            MicPermission::Restricted,
            MicPermission::Unknown(-1),
        ] {
            assert_eq!(p.verdict_where(true), Err(SUPERVISED_WORDS), "{p:?}");
        }
    }

    #[test]
    fn an_unsupervised_recorder_keeps_the_plain_verdict() {
        for p in [
            MicPermission::NotDetermined,
            MicPermission::Authorized,
            MicPermission::Denied,
            MicPermission::Restricted,
            MicPermission::Unknown(7),
        ] {
            assert_eq!(p.verdict_where(false), p.verdict(), "{p:?}");
        }
    }
}
