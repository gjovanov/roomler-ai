// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-108 — is someone using this screen?
//!
//! Display match may switch the host's display only where nobody is using it.
//! "Nobody" means what FR-99's `lock_on_end` means by it: no local input for
//! [`crate::lock_on_end::UNATTENDED_AFTER`]. It is read FR-92's way, with no
//! hook, no raw input and no key polling: the session's idle clock
//! (`GetLastInputInfo`). That clock moves for every input, the person's and the
//! machine's alike (FR-108 P0 measured both), so this module follows it and
//! keeps the last move that the machine does not explain:
//!
//! - the controller's injections (`keep_busy::note_remote_input`, noted by the
//!   input arbiter for every injected event);
//! - keep busy's own pointer moves (`keep_busy::note_own_move`).
//!
//! What it answers ([`Presence`]):
//! - **Locked:** the secure desktop is up. The worker cannot switch it anyway
//!   (P0: `ChangeDisplaySettingsExW` returned `-1` there).
//! - **Attended:** unlocked, and a person touched the input within the window.
//! - **Unattended:** unlocked, and nobody has, for at least that long.
//!
//! Before the first reading nothing is known, and an unknown is never read as
//! "nobody": the first reading counts its last input as a person's.

#[cfg(target_os = "windows")]
use std::sync::Mutex;
use std::time::Duration;

/// A move of the idle clock within this of a machine input's tick is that
/// input. Both are the same `GetTickCount` clock; the slack covers the system
/// timer (10–16 ms) and the time between the injector noting an input and the
/// OS recording it.
pub const MACHINE_SLACK_MS: u32 = 250;

/// How often the sampler reads the idle clock.
pub const SAMPLE_EVERY: Duration = Duration::from_millis(500);

/// Who is at the screen, as display match needs to know it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presence {
    /// The secure desktop is up: locked, or the logon screen.
    Locked,
    /// A person touched the input within the window.
    Attended,
    /// Nobody has, for at least the window.
    Unattended,
}

/// The decision. `person_ms_ago` is `None` when no person's input is known,
/// which only happens where the idle clock cannot be read; that is never
/// taken for an empty chair.
pub fn decide(locked: bool, person_ms_ago: Option<u64>, window: Duration) -> Presence {
    if locked {
        return Presence::Locked;
    }
    match person_ms_ago {
        Some(ago) if ago >= window.as_millis() as u64 => Presence::Unattended,
        _ => Presence::Attended,
    }
}

/// `a - b` on the wrapping 32-bit tick clock, signed.
fn tick_delta(a: u32, b: u32) -> i64 {
    i64::from(a.wrapping_sub(b) as i32)
}

/// Follows the idle clock and keeps the last move that was a person's.
#[derive(Debug, Default)]
pub struct Tracker {
    last_input: Option<u32>,
    last_person: Option<u32>,
}

impl Tracker {
    /// One reading of the idle clock (`input`), with the ticks of the latest
    /// machine inputs (the controller's injection, keep busy's move). The first
    /// reading is a person's whatever it is: nothing proves otherwise.
    pub fn observe(&mut self, input: u32, machine: &[Option<u32>]) {
        let first = self.last_input.is_none();
        if self.last_input == Some(input) {
            return;
        }
        self.last_input = Some(input);
        let explained = machine
            .iter()
            .flatten()
            .any(|&m| tick_delta(input, m).abs() <= i64::from(MACHINE_SLACK_MS));
        if first || !explained {
            self.last_person = Some(input);
        }
    }

    /// How long ago the last person's input was, at `now`.
    pub fn person_ms_ago(&self, now: u32) -> Option<u64> {
        self.last_person.map(|p| tick_delta(now, p).max(0) as u64)
    }
}

#[cfg(target_os = "windows")]
static TRACKER: Mutex<Tracker> = Mutex::new(Tracker {
    last_input: None,
    last_person: None,
});

/// The OS input clock: Windows' `GetTickCount`, the clock `GetLastInputInfo`
/// reports in. Elsewhere `0`, which nothing here reads.
pub fn os_tick() -> u32 {
    #[cfg(target_os = "windows")]
    {
        // SAFETY: no arguments, no failure mode.
        unsafe { windows_sys::Win32::System::SystemInformation::GetTickCount() }
    }
    #[cfg(not(target_os = "windows"))]
    {
        0
    }
}

#[cfg(target_os = "windows")]
fn last_input_tick() -> Option<u32> {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
    let mut lii = LASTINPUTINFO {
        cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
        dwTime: 0,
    };
    // SAFETY: a stack LASTINPUTINFO with cbSize set, as the API requires.
    (unsafe { GetLastInputInfo(&mut lii) } != 0).then_some(lii.dwTime)
}

/// One reading into the tracker.
#[cfg(target_os = "windows")]
fn sample() {
    if let Some(input) = last_input_tick() {
        let machine = [
            crate::keep_busy::last_remote_input_tick(),
            crate::keep_busy::last_own_move_tick(),
        ];
        TRACKER
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .observe(input, &machine);
    }
}

/// Start the sampler (once per process, Windows only): from here on every
/// move of the idle clock is classified. Called when the worker starts, so a
/// request mid-session can tell the controller's input from a person's.
pub fn start() {
    #[cfg(target_os = "windows")]
    {
        static STARTED: std::sync::Once = std::sync::Once::new();
        STARTED.call_once(|| {
            sample();
            let spawned = std::thread::Builder::new()
                .name("presence".into())
                .spawn(|| {
                    loop {
                        std::thread::sleep(SAMPLE_EVERY);
                        sample();
                    }
                });
            if let Err(e) = spawned {
                tracing::warn!(%e, "presence: the sampler could not start; display match reads every screen as attended");
            }
        });
    }
}

/// Who is at the screen now.
pub fn now() -> Presence {
    #[cfg(target_os = "windows")]
    {
        start();
        sample();
        let locked = crate::lock_state::probe_lock_state() == crate::lock_state::LockState::Locked;
        let ago = TRACKER
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .person_ms_ago(os_tick());
        decide(locked, ago, crate::lock_on_end::UNATTENDED_AFTER)
    }
    #[cfg(not(target_os = "windows"))]
    {
        // Display match is Windows-only; nothing here is read as empty.
        Presence::Attended
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOW: Duration = Duration::from_secs(300);

    #[test]
    fn locked_wins_over_everything() {
        assert_eq!(decide(true, Some(0), WINDOW), Presence::Locked);
        assert_eq!(decide(true, Some(10_000_000), WINDOW), Presence::Locked);
        assert_eq!(decide(true, None, WINDOW), Presence::Locked);
    }

    #[test]
    fn a_person_inside_the_window_is_attended_and_outside_it_is_not() {
        assert_eq!(decide(false, Some(0), WINDOW), Presence::Attended);
        assert_eq!(decide(false, Some(299_999), WINDOW), Presence::Attended);
        assert_eq!(decide(false, Some(300_000), WINDOW), Presence::Unattended);
        assert_eq!(decide(false, Some(3_600_000), WINDOW), Presence::Unattended);
    }

    #[test]
    fn an_unknown_is_never_an_empty_chair() {
        assert_eq!(decide(false, None, WINDOW), Presence::Attended);
    }

    #[test]
    fn the_first_reading_is_a_persons() {
        let mut t = Tracker::default();
        // Even if it sits right on a controller injection: nothing proves it
        // was the machine's before the tracker existed.
        t.observe(1_000, &[Some(1_000), None]);
        assert_eq!(t.person_ms_ago(301_000), Some(300_000));
    }

    #[test]
    fn machine_input_does_not_move_the_person() {
        let mut t = Tracker::default();
        t.observe(1_000, &[None, None]);
        // The controller injects at 500_000, keep busy moves at 600_000.
        t.observe(500_010, &[Some(500_000), None]);
        t.observe(600_100, &[Some(500_000), Some(600_000)]);
        assert_eq!(
            t.person_ms_ago(700_000),
            Some(699_000),
            "the person's input is still the one at 1_000"
        );
        assert_eq!(
            decide(false, t.person_ms_ago(700_000), WINDOW),
            Presence::Unattended
        );
    }

    #[test]
    fn a_person_after_the_machine_is_seen() {
        let mut t = Tracker::default();
        t.observe(1_000, &[None, None]);
        t.observe(500_010, &[Some(500_000), None]);
        // Well past the controller's last injection: a person.
        t.observe(500_600, &[Some(500_000), None]);
        assert_eq!(t.person_ms_ago(501_000), Some(400));
        assert_eq!(
            decide(false, t.person_ms_ago(501_000), WINDOW),
            Presence::Attended
        );
    }

    #[test]
    fn the_slack_is_inclusive_and_symmetric() {
        let mut t = Tracker::default();
        t.observe(1_000, &[None, None]);
        // The OS may record the input a little before or after the note.
        t.observe(400_000 - MACHINE_SLACK_MS, &[Some(400_000), None]);
        t.observe(400_000 + MACHINE_SLACK_MS, &[Some(400_000), None]);
        assert_eq!(t.person_ms_ago(400_250), Some(399_250));
        t.observe(400_000 + MACHINE_SLACK_MS + 1, &[Some(400_000), None]);
        assert_eq!(t.person_ms_ago(400_251), Some(0));
    }

    #[test]
    fn the_tick_clock_wraps() {
        let mut t = Tracker::default();
        let before_wrap = u32::MAX - 1_000;
        t.observe(before_wrap, &[None, None]);
        // 2_000 ms later the clock has wrapped past zero.
        let now = before_wrap.wrapping_add(2_000);
        assert_eq!(t.person_ms_ago(now), Some(2_000));
        // A controller injection across the wrap still explains its input.
        let inj = before_wrap.wrapping_add(1_500);
        t.observe(inj.wrapping_add(20), &[Some(inj), None]);
        assert_eq!(t.person_ms_ago(now), Some(2_000));
    }

    #[test]
    fn a_repeated_reading_changes_nothing() {
        let mut t = Tracker::default();
        t.observe(1_000, &[None, None]);
        t.observe(1_000, &[Some(1_000), None]);
        assert_eq!(t.person_ms_ago(2_000), Some(1_000));
    }

    fn code_of(line: &str) -> &str {
        line.split("//").next().unwrap_or("")
    }

    /// FR-108 AC6 — display match tells a person from the controller WITHOUT a
    /// hook, raw input or a keyboard poll, FR-92's rule: each is what a
    /// keylogger looks like to EDR, and a GPO-locked desktop running EDR is in
    /// the acceptance bar. The banned names are assembled at run time, so this
    /// file cannot trip its own scan.
    #[test]
    fn no_keylogger_shaped_api_in_display_match() {
        let sources = [
            ("presence.rs", include_str!("presence.rs")),
            ("display_match.rs", include_str!("display_match.rs")),
        ];
        let banned: Vec<String> = [
            ["SetWindows", "HookEx"],
            ["RegisterRaw", "InputDevices"],
            ["GetRaw", "InputData"],
            ["GetKeyboard", "State"],
            ["GetAsync", "KeyState"],
        ]
        .iter()
        .map(|p| p.concat())
        .collect();
        for (name, src) in sources {
            for (i, line) in src.lines().enumerate() {
                let code = code_of(line);
                for b in &banned {
                    assert!(!code.contains(b.as_str()), "{name}:{}: uses {b}", i + 1);
                }
            }
        }
    }
}
