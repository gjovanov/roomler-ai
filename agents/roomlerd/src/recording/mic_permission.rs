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
//! A microphone nobody has decided about yet is asked about: macOS asks the
//! person, naming the app from the bundle's `NSMicrophoneUsageDescription`,
//! and their answer decides. Asking without that key ends the process, which
//! is why the key ships in the same change as this code.
//!
//! ⚠️ **Except on a supervised Mac** (P2f-1b, measured on the operator's
//! MacBook, 2026-10-24): the GUI worker is started by the ROOT daemon, which
//! stays the responsible process of everything the worker starts, and macOS
//! never asks the person about a microphone for root. The question answered
//! "not determined", the real microphone opened, no prompt ever appeared, and
//! three minutes of recording were silence. P2f-2 starts that worker's
//! recorder as its OWN responsible process (`launch::recorder_owns_its_permissions`,
//! [`crate::macos_spawn`]), so macOS asks the person and the plain verdict
//! applies. Where that cannot be done (the private call is missing, or
//! `ROOMLERD_RECORDER_DISCLAIM=0`), every answer refuses by name instead (the
//! manager tells the recorder, [`SUPERVISED_ENV`]).
//!
//! ⚠️ **The question takes as long as the person does** (P2f-3, measured on the
//! operator's MacBook, 2026-10-25). With the permission undecided, CoreAudio's
//! open blocks until the person answers, and the daemon's 20 s start deadline
//! killed the recorder while they were still reading it. Their Allow then
//! stuck, but that first recording had already failed. So an undecided
//! microphone is asked about here, explicitly
//! (`requestAccessForMediaType:completionHandler:`), within [`ASK_WINDOW`].
//! The recorder first tells the daemon it is asking ([`announce_question`]),
//! and the daemon then waits longer than this window
//! (`manager::ASK_TIMEOUT`), so the recorder's own answer always comes first.
//!
//! The decision is plain data so it is tested on every OS (FR-43's practice);
//! only the question itself is macOS's.

use std::time::Duration;

/// P2f-3 — how long the recorder waits for the person to answer macOS's
/// microphone question before it refuses by name.
pub const ASK_WINDOW: Duration = Duration::from_secs(120);

/// P2f-3 — what a recorder says when the question went unanswered.
pub const NO_ANSWER_WORDS: &str = "macOS asked whether Roomler Daemon may use the microphone, and \
     nobody answered within 2 minutes: answer that question, then start the recording again";

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

/// P2f-3 — what the person did with macOS's microphone question.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    Allowed,
    /// They chose "Don't Allow": the same as turning it off.
    Refused,
    /// Nobody answered within [`ASK_WINDOW`].
    NoAnswer,
}

impl Answer {
    /// `Err(words)` unless the person allowed it.
    pub fn verdict(self) -> Result<(), &'static str> {
        match self {
            Self::Allowed => Ok(()),
            Self::Refused => MicPermission::Denied.verdict(),
            Self::NoAnswer => Err(NO_ANSWER_WORDS),
        }
    }
}

/// P2f-3 — tell the daemon a person is being asked, so its start deadline
/// does not end this recorder while they read the question. Only a build
/// that can open a microphone (`audio`) ever asks.
#[cfg(feature = "audio")]
pub(crate) fn announce_question() {
    super::child::emit(&serde_json::json!({"ev": "asking", "what": "microphone"}));
}

/// Ask AVFoundation, and refuse where the microphone would record silence.
/// An undecided microphone is asked about here, and the answer decides
/// (P2f-3).
#[cfg(all(target_os = "macos", feature = "audio"))]
pub fn preflight() -> anyhow::Result<()> {
    let permission = MicPermission::from_raw(av::authorization_status());
    let supervised = std::env::var_os(SUPERVISED_ENV).is_some_and(|v| v == "1");
    tracing::info!(
        ?permission,
        supervised,
        "recording: the microphone's permission"
    );
    permission
        .verdict_where(supervised)
        .map_err(|words| anyhow::anyhow!(words))?;
    if permission == MicPermission::NotDetermined {
        announce_question();
        let answer = av::request_access(ASK_WINDOW);
        tracing::info!(
            ?answer,
            "recording: the person answered the microphone question"
        );
        answer.verdict().map_err(|words| anyhow::anyhow!(words))?;
    }
    Ok(())
}

/// The two AVFoundation calls, macOS only.
#[cfg(all(target_os = "macos", feature = "audio"))]
mod av {
    use std::time::Duration;

    use block2::RcBlock;
    use objc2::msg_send;
    use objc2::runtime::{AnyClass, Bool};
    use objc2_foundation::NSString;

    use super::Answer;

    #[link(name = "AVFoundation", kind = "framework")]
    unsafe extern "C" {
        static AVMediaTypeAudio: &'static NSString;
    }

    /// `[AVCaptureDevice authorizationStatusForMediaType:AVMediaTypeAudio]`,
    /// or -1 where the class is missing. Asking never prompts the person.
    pub(super) fn authorization_status() -> isize {
        let Some(class) = AnyClass::get(c"AVCaptureDevice") else {
            return -1;
        };
        // SAFETY: a class method taking an `AVMediaType` (an `NSString *`)
        // and returning an `NSInteger`; `AVMediaTypeAudio` is a constant
        // AVFoundation exports for exactly this argument.
        unsafe { msg_send![class, authorizationStatusForMediaType: AVMediaTypeAudio] }
    }

    /// `[AVCaptureDevice requestAccessForMediaType:completionHandler:]`: macOS
    /// asks the person (naming the bundle's usage string), and the handler
    /// runs on a queue of AVFoundation's with their answer. Waits at most
    /// `window`; the question stays on screen after that, and its answer is
    /// kept for the next recording.
    pub(super) fn request_access(window: Duration) -> Answer {
        let Some(class) = AnyClass::get(c"AVCaptureDevice") else {
            return Answer::NoAnswer;
        };
        let (tx, rx) = std::sync::mpsc::sync_channel::<bool>(1);
        let handler = RcBlock::new(move |granted: Bool| {
            let _ = tx.try_send(granted.as_bool());
        });
        // SAFETY: a class method taking the same `AVMediaType` constant and a
        // `void (^)(BOOL)` block, which AVFoundation copies and retains until
        // it runs; the handler only sends on a channel it owns.
        unsafe {
            let _: () = msg_send![
                class,
                requestAccessForMediaType: AVMediaTypeAudio,
                completionHandler: &*handler
            ];
        }
        match rx.recv_timeout(window) {
            Ok(true) => Answer::Allowed,
            Ok(false) => Answer::Refused,
            Err(_) => Answer::NoAnswer,
        }
    }
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
    fn the_persons_answer_decides_and_silence_is_named() {
        // P2f-3: an undecided microphone is asked about; only "allow" opens it.
        assert_eq!(Answer::Allowed.verdict(), Ok(()));
        // "Don't Allow" is the same as turning it off, and says where to undo it.
        assert_eq!(Answer::Refused.verdict(), MicPermission::Denied.verdict());
        // A question nobody answered is not a "no": it says so, and how to go on.
        let words = Answer::NoAnswer.verdict().unwrap_err();
        assert_eq!(words, NO_ANSWER_WORDS);
        assert!(words.contains("start the recording again"), "{words}");
    }

    #[test]
    fn the_daemon_outwaits_the_question() {
        // The recorder's own refusal must arrive before the daemon's deadline,
        // or a person who never answered gets "did not start" instead of why.
        assert!(super::super::manager::ASK_TIMEOUT > ASK_WINDOW + Duration::from_secs(10));
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
