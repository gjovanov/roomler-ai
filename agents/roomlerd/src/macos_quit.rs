// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! #3164 — on macOS a Quit AppleEvent restarts the agent, and a launch by
//! LaunchServices never becomes a second, unsupervised daemon.
//!
//! ## The Quit AppleEvent
//!
//! With `viewer-indicator-macos` the daemon's main thread runs `NSApplication`
//! (FR-27, `indicator::mac::run_main_loop`), so `roomlerd` answers the Quit
//! AppleEvent like any app does: AppKit's own handler ends the process with
//! **exit 0**. macOS sends exactly that event from System Settings: turning
//! Screen Recording on for the running agent offers "Quit & Reopen" as the
//! default button, because macOS applies that grant to a fresh process only.
//! Field, 2026-10-30, agent 0.4.188 on macOS 15.7: `Handling Quit AppleEvent` →
//! `applicationShouldTerminate: YES` → `Termination complete`, exit 0. The
//! LaunchAgent's `KeepAlive {SuccessfulExit: false}` reads a 0 as "done, stay
//! down", so the device went offline and stayed offline until somebody typed
//! `launchctl kickstart -k` in Terminal. "Reopen" failed too ("The application
//! “roomlerd” is not open anymore.").
//!
//! So the delegate `indicator::mac` installs answers `applicationShouldTerminate:`
//! by [`classify`] and [`decide`], from WHO sent the quit
//! (`keySenderPIDAttr`, resolved to the sender's executable):
//!
//! | the quit's sender | when | the answer |
//! |---|---|---|
//! | `loginwindow` | a log-out, restart or shut-down: it quits the session's apps, then launchd tears the `gui/<uid>` domain down | AppKit's own: exit 0 |
//! | anything else | System Settings' Quit & Reopen, `osascript … to quit` | a restart, through the graceful path FR-84 D3's `RestartDaemon` takes, leaving with the supervisor's code ([`crate::supervision::Supervision::restart_exit_code`]); AppKit is told to wait (`NSTerminateLater`) and never reaches its `exit(0)` |
//! | not named by the event | — | its `kAEQuitReason`: a session-ending one ([`SESSION_ENDING_REASONS`]) is AppKit's exit 0, none is a restart |
//! | any quit, under no supervisor (a `roomlerd run` started by hand) | — | AppKit's own: there a quit is a stop |
//! | not a Quit AppleEvent at all | — | AppKit's own |
//!
//! ⚠️ **The reason alone cannot decide.** System Settings' Quit & Reopen sends
//! its quit WITH `kAEQuitReason = 'rlgo'` (kAEReallyLogOut), the very reason a
//! log-out carries (field, 2026-10-30, macOS 15.7: a classifier by reason left
//! the agent down exactly as before). Only the sender tells them apart.
//!
//! ⚠️ The `loginwindow` row is load-bearing. A log-out quits every app of the
//! session BEFORE launchd tears the `gui/<uid>` domain down, so a restart there
//! would put the agent back into a session that is ending, once per quit, until
//! the domain went. AppKit's exit 0 is what the agent has always done there,
//! and `RunAtLoad` starts it again at the next login.
//!
//! **Activity Monitor's Quit is a stop, and stays one (#3195).** It sends a
//! signal, not a Quit AppleEvent, so nothing above runs: the daemon's ordinary
//! SIGTERM path ends it with exit 0, and launchd leaves it down until the next
//! login, a reboot, or an open of `roomlerd.app` (field, 2026-10-31, agent
//! 0.4.190 on macOS 15.7.7). That keeps one explicit "make it go away" for the
//! person at the Mac. A restart there would need the signal path to tell that
//! person from a log-out, a `launchctl bootout`, the update helper and an
//! uninstall, none of which may relaunch. The Dock sends nothing here:
//! `roomlerd` has no Dock tile (`LSUIElement`).
//!
//! ## LaunchServices' own launch
//!
//! A bare `roomlerd` (no subcommand) runs the daemon (`app.rs`, `Command::Run`).
//! LaunchServices starts the bundle exactly that way: System Settings' "Reopen",
//! a double-click in Finder, Spotlight. The single-instance lock refuses such a
//! process when the LaunchAgent's own is already running (the macOS
//! `already_running_exit` is a 0, so nothing relaunches it). But "Reopen" comes
//! right after the quit, while launchd is relaunching the job, and the first of
//! the two to take the lock wins. When LaunchServices' process won, it served
//! with no supervisor (`XPC_SERVICE_NAME` = `application.com.roomler.agent.…`),
//! launchd's own exited 0 at the lock and stayed down, and every later
//! `launchctl bootout` of the job (an update's postinstall, an uninstall) missed
//! the process that was actually running — the trap the companion fell into
//! through nine updates (`packaging/macos/postinstall`, `stop_companions`). So
//! a bare `roomlerd` that LaunchServices started ([`launched_by_launchservices`])
//! never runs the daemon: it starts the LaunchAgent's job if that job is loaded
//! (`launchctl kickstart`, without `-k`, which leaves a running job alone) and
//! exits 0.
//!
//! The decisions are pure and build and test on every platform; the AppKit and
//! launchd calls are macOS-only (`indicator::mac`, `app.rs`).

/// A four-character code as the Apple Event Manager stores it (big-endian).
pub const fn fourcc(code: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*code)
}

/// `kCoreEventClass` (`AppleEvents.h`).
pub const CORE_EVENT_CLASS: u32 = fourcc(b"aevt");
/// `kAEQuitApplication` (`AppleEvents.h`): "May include a property
/// kAEQuitReason indicating what lead to the quit being sent."
pub const QUIT_APPLICATION: u32 = fourcc(b"quit");
/// `kAEQuitReason` (`AERegistry.h`): "in a kAEQuitApplication event, this
/// parameter if present is the reason the quit is being sent. The possible
/// values are kAEQuitAll, kAEShutDown, kAERestart, kAEReallyLogOut". The
/// header calls it a parameter, and Apple's samples read it as an attribute;
/// `indicator::mac` reads both.
pub const QUIT_REASON_KEYWORD: u32 = fourcc(b"why?");

/// `keySenderPIDAttr` (`AEDataModel.h`): "read only, returned as typeSInt32.
/// Will be the pid of the sender of this event."
pub const SENDER_PID_KEYWORD: u32 = fourcc(b"spid");

/// The executable that quits a session's apps when the session (or the
/// machine) is going away.
pub const LOGINWINDOW: &str = "loginwindow";

/// Every quit reason that means the session, or the machine, is going away:
/// the four `AERegistry.h` names as a quit's possible reasons (`quia`, `shut`,
/// `rest`, `rlgo`), and the log-out and dialog variants beside them in the same
/// header (`logo`, `rrst`, `rsdn`). Checked against the macOS 27 SDK. Consulted
/// only when the event names no sender ([`Sender::Unknown`]): System Settings'
/// Quit & Reopen says `rlgo` too.
pub const SESSION_ENDING_REASONS: [u32; 7] = [
    fourcc(b"quia"), // kAEQuitAll
    fourcc(b"shut"), // kAEShutDown
    fourcc(b"rest"), // kAERestart
    fourcc(b"rlgo"), // kAEReallyLogOut
    fourcc(b"logo"), // kAELogOut
    fourcc(b"rrst"), // kAEShowRestartDialog
    fourcc(b"rsdn"), // kAEShowShutdownDialog
];

/// The bundle identifier of `/Library/Roomler/roomlerd.app`: what TCC grants
/// to, and the launchd label of the per-user agent (`com.roomler.agent.plist`).
/// FR-46 P5b kept it on purpose when the bundle was renamed.
pub const BUNDLE_ID: &str = "com.roomler.agent";

/// Who sent the quit, as far as the event says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sender {
    /// `loginwindow`: the session, or the machine, is going away.
    Loginwindow,
    /// Any other process: System Settings, a script.
    Other,
    /// The event named no sender, or its executable could not be read.
    Unknown,
}

/// The sender, from its executable's path (`proc_pidpath` of the event's
/// `keySenderPIDAttr`): `loginwindow` by its file name, exactly.
pub fn sender_of(path: Option<&str>) -> Sender {
    match path {
        Some(p) if p.rsplit('/').next() == Some(LOGINWINDOW) => Sender::Loginwindow,
        Some(p) if !p.is_empty() => Sender::Other,
        _ => Sender::Unknown,
    }
}

/// What asked the application to terminate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuitIntent {
    /// No Quit AppleEvent is being handled: AppKit's `terminate:` came from
    /// somewhere else.
    NotAQuitEvent,
    /// The session's apps are being quit: a log-out, restart or shut-down.
    /// Carries the reason code (`0` when there was none).
    SessionEnding(u32),
    /// Somebody asked this app to quit: System Settings' "Quit & Reopen", a
    /// script. Carries the reason code, if any.
    Quit(Option<u32>),
}

/// How `applicationShouldTerminate:` answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuitReply {
    /// `NSTerminateNow`: AppKit ends the process with exit 0, as it always has.
    TerminateNow,
    /// `NSTerminateLater`: the daemon has armed its graceful restart and leaves
    /// with its supervisor's code; AppKit waits and never reaches its exit.
    RestartLater,
}

/// Classify the Apple Event being handled. `event` is its (class, id), `None`
/// when there is none; `reason` is its `kAEQuitReason`, as an attribute or a
/// parameter; `sender` is who sent it ([`sender_of`]).
pub fn classify(event: Option<(u32, u32)>, reason: Option<u32>, sender: Sender) -> QuitIntent {
    if event != Some((CORE_EVENT_CLASS, QUIT_APPLICATION)) {
        return QuitIntent::NotAQuitEvent;
    }
    match sender {
        Sender::Loginwindow => QuitIntent::SessionEnding(reason.unwrap_or(0)),
        Sender::Other => QuitIntent::Quit(reason),
        Sender::Unknown => match reason {
            Some(r) if SESSION_ENDING_REASONS.contains(&r) => QuitIntent::SessionEnding(r),
            other => QuitIntent::Quit(other),
        },
    }
}

/// The whole policy. `arm_restart` is asked only for a [`QuitIntent::Quit`]:
/// it arms the daemon's graceful restart and says whether it did. It does not
/// when nothing would relaunch the process (no supervisor, or no daemon in this
/// process at all), and then the quit is a stop, as before.
pub fn decide(intent: QuitIntent, arm_restart: impl FnOnce() -> bool) -> QuitReply {
    match intent {
        QuitIntent::Quit(_) if arm_restart() => QuitReply::RestartLater,
        _ => QuitReply::TerminateNow,
    }
}

/// A four-character code as text, for a log line: `'logo'`, or the number
/// when it is not printable.
pub fn fourcc_text(code: u32) -> String {
    let b = code.to_be_bytes();
    if b.iter().all(|c| c.is_ascii_graphic() || *c == b' ') {
        format!("'{}'", String::from_utf8_lossy(&b))
    } else {
        format!("{code:#010x}")
    }
}

/// Did LaunchServices start this process as the `bundle_id` application?
/// LaunchServices runs an app it launches as the launchd job
/// `application.<bundle id>.<number>.<number>` and sets `XPC_SERVICE_NAME` to
/// that label. The `.` after the bundle id is required, so another bundle whose
/// id merely begins the same way (`com.roomler.agent-tray`) never matches, and
/// neither does a shell under Terminal (`application.com.apple.Terminal.…`),
/// nor our own launchd jobs (`com.roomler.agent`).
pub fn launched_by_launchservices(xpc_service_name: Option<&str>, bundle_id: &str) -> bool {
    xpc_service_name
        .and_then(|label| label.strip_prefix("application."))
        .and_then(|rest| rest.strip_prefix(bundle_id))
        .is_some_and(|rest| rest.starts_with('.'))
}

/// #3164 — what the daemon registers for a Quit AppleEvent to call: arm its
/// graceful restart and start it, returning the exit code it will leave with,
/// or `None` when it armed nothing. Registered once by `run` on macOS; absent
/// in every other process (a CLI call, a probe), where a quit stays AppKit's.
type QuitHook = Box<dyn Fn(&str) -> Option<i32> + Send + Sync>;

static QUIT_HOOK: std::sync::OnceLock<QuitHook> = std::sync::OnceLock::new();

/// Register the daemon's answer to a Quit AppleEvent. Only the first call
/// counts.
pub fn set_quit_hook(hook: impl Fn(&str) -> Option<i32> + Send + Sync + 'static) {
    let _ = QUIT_HOOK.set(Box::new(hook));
}

/// Ask the registered hook to restart the daemon for a quit: the exit code it
/// leaves with, or `None` when no hook is registered or it armed nothing.
pub fn arm_restart_for_quit(reason: &str) -> Option<i32> {
    QUIT_HOOK.get().and_then(|hook| hook(reason))
}

/// How long the graceful restart a quit started may take before the process
/// leaves anyway, with the same code. The graceful path is bounded already (the
/// runtime's shutdown budget); this only makes "the person quit it" end in a
/// relaunch even if something on that path hangs, while AppKit waits on
/// `NSTerminateLater` and would otherwise wait forever.
pub const QUIT_RESTART_BACKSTOP: std::time::Duration = std::time::Duration::from_secs(20);

#[cfg(test)]
mod tests {
    use super::*;

    const QUIT: Option<(u32, u32)> = Some((CORE_EVENT_CLASS, QUIT_APPLICATION));

    /// The codes are the SDK's, byte for byte: a typo here would turn a
    /// log-out into a restart (or System Settings' quit into a stop) and
    /// nothing would say so.
    #[test]
    fn the_codes_are_the_sdks() {
        assert_eq!(CORE_EVENT_CLASS, 0x6165_7674);
        assert_eq!(QUIT_APPLICATION, 0x7175_6974);
        assert_eq!(QUIT_REASON_KEYWORD, 0x7768_793f);
        assert_eq!(SENDER_PID_KEYWORD, fourcc(b"spid"));
        // The four `AERegistry.h` lists as a quit's possible reasons, with the
        // hex values the header prints beside three of them.
        assert!(SESSION_ENDING_REASONS.contains(&0x7175_6961)); // kAEQuitAll 'quia'
        assert!(SESSION_ENDING_REASONS.contains(&0x7265_7374)); // kAERestart 'rest'
        assert!(SESSION_ENDING_REASONS.contains(&0x7368_7574)); // kAEShutDown 'shut'
        assert!(SESSION_ENDING_REASONS.contains(&fourcc(b"rlgo"))); // kAEReallyLogOut
        assert!(!SESSION_ENDING_REASONS.contains(&fourcc(b"slep"))); // kAESleep is no quit
    }

    const RLGO: u32 = fourcc(b"rlgo");

    /// The field case first: System Settings' "Quit & Reopen" quits with
    /// `kAEQuitReason = 'rlgo'`, a log-out's reason, from a sender that is not
    /// loginwindow (macOS 15.7, 2026-10-30). It must restart. A classifier by
    /// reason (this branch's first draft, and every reading of Apple's samples)
    /// called it a session end and left the agent down exactly as before.
    #[test]
    fn system_settings_quit_and_reopen_is_a_restart_although_it_says_rlgo() {
        let intent = classify(QUIT, Some(RLGO), Sender::Other);
        assert_eq!(intent, QuitIntent::Quit(Some(RLGO)));
        assert_eq!(decide(intent, || true), QuitReply::RestartLater);
        // A script: no reason at all.
        assert_eq!(
            decide(classify(QUIT, None, Sender::Other), || true),
            QuitReply::RestartLater
        );
    }

    /// loginwindow quits a session's apps only when the session, or the
    /// machine, is going away: AppKit's own exit, whatever the reason says,
    /// and the daemon is never asked to arm a restart.
    #[test]
    fn loginwindow_s_quit_is_a_session_end_whatever_its_reason() {
        for reason in [
            Some(RLGO),
            Some(fourcc(b"shut")),
            Some(fourcc(b"rest")),
            None,
        ] {
            let intent = classify(QUIT, reason, Sender::Loginwindow);
            assert_eq!(intent, QuitIntent::SessionEnding(reason.unwrap_or(0)));
            assert_eq!(
                decide(intent, || panic!("a session end must never arm a restart")),
                QuitReply::TerminateNow,
                "{reason:?}"
            );
        }
    }

    /// With no sender to tell by, the reason decides, as Apple's samples have
    /// it: every session-ending reason is AppKit's exit, none is a restart, and
    /// a reason this module does not know is still a quit.
    #[test]
    fn with_no_sender_the_reason_decides() {
        for code in SESSION_ENDING_REASONS {
            assert_eq!(
                classify(QUIT, Some(code), Sender::Unknown),
                QuitIntent::SessionEnding(code),
                "{}",
                fourcc_text(code)
            );
        }
        assert_eq!(
            classify(QUIT, None, Sender::Unknown),
            QuitIntent::Quit(None)
        );
        let odd = fourcc(b"zzzz");
        assert_eq!(
            classify(QUIT, Some(odd), Sender::Unknown),
            QuitIntent::Quit(Some(odd))
        );
    }

    /// loginwindow by its file name exactly; any other executable is someone
    /// else; no path is unknown.
    #[test]
    fn the_sender_is_loginwindow_by_its_exact_name() {
        assert_eq!(
            sender_of(Some(
                "/System/Library/CoreServices/loginwindow.app/Contents/MacOS/loginwindow"
            )),
            Sender::Loginwindow
        );
        for other in [
            "/System/Applications/System Settings.app/Contents/MacOS/System Settings",
            "/System/Library/CoreServices/Dock.app/Contents/MacOS/Dock",
            "/usr/bin/osascript",
            "/tmp/loginwindow-lookalike",
            "/tmp/notloginwindow",
        ] {
            assert_eq!(sender_of(Some(other)), Sender::Other, "{other}");
        }
        assert_eq!(sender_of(Some("")), Sender::Unknown);
        assert_eq!(sender_of(None), Sender::Unknown);
    }

    /// No supervisor, or no daemon in this process: the hook arms nothing, and
    /// the quit is AppKit's stop, as it was before #3164.
    #[test]
    fn a_quit_nothing_would_relaunch_stays_a_stop() {
        assert_eq!(
            decide(classify(QUIT, Some(RLGO), Sender::Other), || false),
            QuitReply::TerminateNow
        );
    }

    /// Anything that is not the core suite's quit leaves AppKit alone, and asks
    /// the daemon nothing.
    #[test]
    fn only_the_quit_event_is_ours() {
        for event in [
            None,
            Some((CORE_EVENT_CLASS, fourcc(b"rapp"))), // reopen
            Some((CORE_EVENT_CLASS, fourcc(b"odoc"))), // open documents
            Some((fourcc(b"misc"), QUIT_APPLICATION)),
        ] {
            for sender in [Sender::Loginwindow, Sender::Other, Sender::Unknown] {
                assert_eq!(
                    classify(event, None, sender),
                    QuitIntent::NotAQuitEvent,
                    "{event:?} {sender:?}"
                );
                assert_eq!(
                    decide(classify(event, None, sender), || panic!(
                        "asked for {event:?}"
                    )),
                    QuitReply::TerminateNow
                );
            }
        }
    }

    #[test]
    fn a_code_reads_as_its_letters() {
        assert_eq!(fourcc_text(fourcc(b"logo")), "'logo'");
        assert_eq!(fourcc_text(fourcc(b"why?")), "'why?'");
        assert_eq!(fourcc_text(0), "0x00000000");
    }

    /// LaunchServices' launch of OUR bundle, and nothing else: our own launchd
    /// jobs, a shell under Terminal, the companion's bundle (whose id begins
    /// with ours), and no label at all are not it.
    #[test]
    fn only_launchservices_own_launch_of_the_bundle_matches() {
        for yes in [
            "application.com.roomler.agent.2231343.2231349",
            "application.com.roomler.agent.1",
        ] {
            assert!(launched_by_launchservices(Some(yes), BUNDLE_ID), "{yes}");
        }
        for no in [
            "com.roomler.agent",
            "com.roomler.daemon",
            "application.com.apple.Terminal.123.456",
            "application.com.roomler.agent-tray.1.2",
            "application.ai.roomler.agent-tray.1.2",
            "application.com.roomler.agent",
            "application.",
            "0",
            "",
        ] {
            assert!(!launched_by_launchservices(Some(no), BUNDLE_ID), "{no:?}");
        }
        assert!(!launched_by_launchservices(None, BUNDLE_ID));
    }
}
