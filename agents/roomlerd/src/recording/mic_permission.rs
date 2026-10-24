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
//! The decision is plain data so it is tested on every OS (FR-43's practice);
//! only the question itself is macOS's.

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
}

/// Ask AVFoundation, and refuse where the microphone would record silence.
#[cfg(all(target_os = "macos", feature = "audio"))]
pub fn preflight() -> anyhow::Result<()> {
    let permission = MicPermission::from_raw(authorization_status());
    tracing::info!(?permission, "recording: the microphone's permission");
    permission.verdict().map_err(|words| anyhow::anyhow!(words))
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
}
