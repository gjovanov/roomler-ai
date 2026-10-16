// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! rc.191 — match the remote display mode to the viewer ("display match").
//!
//! The field sharpness saga (2026-07-16) proved the only truly crisp remote
//! text chain is 1:1 END-TO-END: the host renders its desktop at (or below)
//! the viewer's stage size, the stream carries those pixels untouched, and
//! the viewer paints them 1:1. Any resample anywhere — the agent's downscale,
//! the browser's contain-scale — smears ClearType. RDP solves this by
//! CHANGING the session resolution; this module is our equivalent (RustDesk
//! calls it "adjust resolution"):
//!
//! * Browser (opt-in toggle) sends `rc:display-match {width, height}` — the
//!   viewer's stage size in physical pixels.
//! * The agent picks the LARGEST supported display mode that FITS WITHIN the
//!   request (so the mode maps ≤1:1 into the stage; combined with the
//!   rc.191 snap-to-native the whole chain then runs 1:1) and switches the
//!   primary display to it.
//! * `{enable: false}` — or the control channel closing, or the agent
//!   process exiting — restores the original mode.
//!
//! Windows-only v1. The switch uses `CDS_FULLSCREEN`, which Windows defines
//! as a TEMPORARY mode change: if the agent process dies without restoring,
//! the OS reverts on its own — crash-safe by construction. The explicit
//! restore (`ChangeDisplaySettingsExW(NULL, NULL, …)`) covers the orderly
//! paths (toggle off, session end).
//!
//! FR-108 — **never on a screen someone is using.** The viewer's toggle is ON
//! by default, so every request is decided by the device ([`request_for`]):
//! the owner's `display_match` (`auto` | `always` | `never`), and under `auto`
//! who is at the screen ([`crate::presence`]). Every request gets one answer,
//! `rc:display-match.state` ([`state_json`]), so the viewer can say what
//! happened instead of guessing.

use crate::config::DisplayMatchPolicy;
use crate::presence::Presence;

/// Pick the largest mode (by area) that fits entirely within the requested
/// box; ties broken toward the mode closest to the request's aspect. Returns
/// `None` when no mode fits (pathologically small request) — callers should
/// then leave the display alone rather than switch to something tiny.
///
/// Pure + platform-independent so the policy is unit-tested everywhere.
pub fn pick_display_mode(modes: &[(u32, u32)], req_w: u32, req_h: u32) -> Option<(u32, u32)> {
    let mut best: Option<(u32, u32)> = None;
    let mut best_key = (0u64, f64::MAX);
    let req_aspect = if req_h > 0 {
        req_w as f64 / req_h as f64
    } else {
        16.0 / 9.0
    };
    for &(w, h) in modes {
        if w == 0 || h == 0 || w > req_w || h > req_h {
            continue;
        }
        let area = (w as u64) * (h as u64);
        let aspect_gap = (w as f64 / h as f64 - req_aspect).abs();
        // Larger area wins; equal areas prefer the aspect closest to the
        // stage (letterboxes less).
        if area > best_key.0 || (area == best_key.0 && aspect_gap < best_key.1) {
            best_key = (area, aspect_gap);
            best = Some((w, h));
        }
    }
    best
}

#[cfg(target_os = "windows")]
mod win {
    use std::sync::atomic::{AtomicBool, Ordering};

    use windows_sys::Win32::Graphics::Gdi::{
        CDS_FULLSCREEN, ChangeDisplaySettingsExW, DEVMODEW, DISP_CHANGE_SUCCESSFUL,
        ENUM_CURRENT_SETTINGS, EnumDisplaySettingsW,
    };

    /// Whether WE changed the mode (so restore is only attempted when
    /// there's something to undo). Process-global: one primary display,
    /// and concurrent sessions on one host are already serialised by the
    /// hub's single-session policy.
    static CHANGED: AtomicBool = AtomicBool::new(false);

    fn zeroed_devmode() -> DEVMODEW {
        let mut dm: DEVMODEW = unsafe { std::mem::zeroed() };
        dm.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
        dm
    }

    /// Enumerate the primary display's supported mode list (dedup'd WxH).
    pub fn supported_modes() -> Vec<(u32, u32)> {
        let mut out: Vec<(u32, u32)> = Vec::new();
        let mut i = 0u32;
        loop {
            let mut dm = zeroed_devmode();
            let ok = unsafe { EnumDisplaySettingsW(std::ptr::null(), i, &mut dm) };
            if ok == 0 {
                break;
            }
            let wh = (dm.dmPelsWidth, dm.dmPelsHeight);
            if !out.contains(&wh) {
                out.push(wh);
            }
            i += 1;
            if i > 4096 {
                break; // defensive bound; real lists are < 200 entries
            }
        }
        out
    }

    /// Current primary-display mode.
    pub fn current_mode() -> Option<(u32, u32)> {
        let mut dm = zeroed_devmode();
        let ok = unsafe { EnumDisplaySettingsW(std::ptr::null(), ENUM_CURRENT_SETTINGS, &mut dm) };
        if ok == 0 {
            return None;
        }
        Some((dm.dmPelsWidth, dm.dmPelsHeight))
    }

    /// Switch the primary display to `w×h` (temporary — `CDS_FULLSCREEN`).
    pub fn apply_mode(w: u32, h: u32) -> Result<(), i32> {
        // Find a full DEVMODEW for the target (frequency/bpp fields matter
        // to some drivers) — walk the list and use the highest-frequency
        // entry at that size.
        let mut chosen: Option<DEVMODEW> = None;
        let mut i = 0u32;
        loop {
            let mut dm = zeroed_devmode();
            let ok = unsafe { EnumDisplaySettingsW(std::ptr::null(), i, &mut dm) };
            if ok == 0 {
                break;
            }
            if dm.dmPelsWidth == w && dm.dmPelsHeight == h {
                let better = match &chosen {
                    None => true,
                    Some(c) => dm.dmDisplayFrequency > c.dmDisplayFrequency,
                };
                if better {
                    chosen = Some(dm);
                }
            }
            i += 1;
            if i > 4096 {
                break;
            }
        }
        let Some(dm) = chosen else {
            return Err(-100); // no such mode (raced a monitor change)
        };
        let rc = unsafe {
            ChangeDisplaySettingsExW(
                std::ptr::null(),
                &dm,
                std::ptr::null_mut(),
                CDS_FULLSCREEN,
                std::ptr::null(),
            )
        };
        if rc == DISP_CHANGE_SUCCESSFUL {
            CHANGED.store(true, Ordering::SeqCst);
            Ok(())
        } else {
            Err(rc)
        }
    }

    /// Restore the pre-change mode (no-op when we never changed it).
    pub fn restore() {
        if !CHANGED.swap(false, Ordering::SeqCst) {
            return;
        }
        // NULL devmode = "revert to the registry (persisted) mode" — the
        // documented undo for a CDS_FULLSCREEN temporary change.
        let rc = unsafe {
            ChangeDisplaySettingsExW(
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
            )
        };
        if rc != DISP_CHANGE_SUCCESSFUL {
            tracing::warn!(rc, "display-match: restore ChangeDisplaySettingsExW failed");
        }
    }
}

/// Why a switch did not happen (FR-108: each maps to the word the viewer is
/// told). `Display` keeps rc.191's log texts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyError {
    /// No mode the display offers fits inside the request.
    NoFittingMode {
        req_w: u32,
        req_h: u32,
        modes: usize,
    },
    /// `ChangeDisplaySettingsExW` refused (P0: `-1` on the lock screen).
    SwitchFailed(i32),
    /// Not a platform display match runs on.
    Unsupported,
}

impl std::fmt::Display for ApplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoFittingMode {
                req_w,
                req_h,
                modes,
            } => write!(
                f,
                "no supported mode fits within {req_w}x{req_h} ({modes} modes)"
            ),
            Self::SwitchFailed(rc) => write!(f, "ChangeDisplaySettingsExW failed rc={rc}"),
            Self::Unsupported => write!(f, "display-match is Windows-only in v1"),
        }
    }
}

/// Apply a display-match request: pick the best-fitting mode for the
/// viewer's stage and switch to it. Returns the mode picked (for the log)
/// or why not. Non-Windows: unsupported.
pub fn apply(req_w: u32, req_h: u32) -> Result<(u32, u32), ApplyError> {
    #[cfg(target_os = "windows")]
    {
        let modes = win::supported_modes();
        let Some((w, h)) = pick_display_mode(&modes, req_w, req_h) else {
            return Err(ApplyError::NoFittingMode {
                req_w,
                req_h,
                modes: modes.len(),
            });
        };
        if win::current_mode() == Some((w, h)) {
            return Ok((w, h)); // already there — nothing to change/restore
        }
        win::apply_mode(w, h)
            .map(|_| (w, h))
            .map_err(ApplyError::SwitchFailed)
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (req_w, req_h);
        Err(ApplyError::Unsupported)
    }
}

/// Restore the original display mode (safe to call when nothing changed).
pub fn restore() {
    #[cfg(target_os = "windows")]
    win::restore();
}

// ── Multi-user P3: per-session ownership ────────────────────────────────────
//
// The host display mode is GLOBAL, but with N concurrent sessions the
// restore must not be: pre-P3, ANY session's control-DC close (or explicit
// `{enable:false}`) called `restore()` — a watcher tab closing reverted the
// mode the driving session was actively using. The owner slot records WHICH
// session last applied a mode; only that session's disable/close restores
// (a new apply by another session is a takeover — last request wins, same
// as the underlying `rc:display-match` semantics). The CDS_FULLSCREEN
// process-exit auto-restore is untouched.

static DM_OWNER: std::sync::Mutex<Option<bson::oid::ObjectId>> = std::sync::Mutex::new(None);

/// [`apply`] with ownership: on success the session becomes the owner.
pub fn apply_for(
    session: bson::oid::ObjectId,
    req_w: u32,
    req_h: u32,
) -> Result<(u32, u32), ApplyError> {
    let picked = apply(req_w, req_h)?;
    *DM_OWNER.lock().unwrap_or_else(|e| e.into_inner()) = Some(session);
    Ok(picked)
}

/// [`restore`] gated on ownership: restores (and clears the owner) only when
/// `session` is the current owner. `true` when a restore actually ran.
pub fn restore_for(session: bson::oid::ObjectId) -> bool {
    let mut owner = DM_OWNER.lock().unwrap_or_else(|e| e.into_inner());
    if *owner == Some(session) {
        *owner = None;
        drop(owner);
        restore();
        true
    } else {
        false
    }
}

// ── FR-108: the device decides, and answers ─────────────────────────────────

/// The owner's `display_match`, live (a local `ConfigSet` re-seeds it; the
/// server cannot reach it). 0 = auto, 1 = always, 2 = never.
static DEVICE_POLICY: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

fn policy_byte(p: DisplayMatchPolicy) -> u8 {
    match p {
        DisplayMatchPolicy::Auto => 0,
        DisplayMatchPolicy::Always => 1,
        DisplayMatchPolicy::Never => 2,
    }
}

/// The owner set `display_match` (at start, and on every local change).
pub fn set_device_policy(p: DisplayMatchPolicy) {
    let was = DEVICE_POLICY.swap(policy_byte(p), std::sync::atomic::Ordering::Relaxed);
    if was != policy_byte(p) {
        tracing::info!(policy = ?p, "display-match: the owner's policy");
    }
}

/// The owner's `display_match` now.
pub fn device_policy() -> DisplayMatchPolicy {
    match DEVICE_POLICY.load(std::sync::atomic::Ordering::Relaxed) {
        1 => DisplayMatchPolicy::Always,
        2 => DisplayMatchPolicy::Never,
        _ => DisplayMatchPolicy::Auto,
    }
}

/// Why a request was not switched, in the word the viewer is told.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// Someone is using this screen.
    Attended,
    /// The screen is locked; the worker cannot switch it (P0).
    Locked,
    /// The owner set `display_match = never`.
    DisabledOnDevice,
    /// No mode the display offers fits the viewer's window.
    NoFittingMode,
    /// The switch itself failed.
    SwitchFailed,
    /// Not a platform display match runs on.
    Unsupported,
}

impl Refusal {
    pub fn word(self) -> &'static str {
        match self {
            Self::Attended => "attended",
            Self::Locked => "locked",
            Self::DisabledOnDevice => "disabled_on_device",
            Self::NoFittingMode => "no_fitting_mode",
            Self::SwitchFailed => "switch_failed",
            Self::Unsupported => "unsupported",
        }
    }
}

impl From<ApplyError> for Refusal {
    fn from(e: ApplyError) -> Self {
        match e {
            ApplyError::NoFittingMode { .. } => Self::NoFittingMode,
            ApplyError::SwitchFailed(_) => Self::SwitchFailed,
            ApplyError::Unsupported => Self::Unsupported,
        }
    }
}

/// What the device answers a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    /// The display runs at this mode for the session.
    On { w: u32, h: u32 },
    /// Display match is off for this session (its viewer turned it off).
    Off,
    /// Not switched, and why.
    Refused(Refusal),
}

/// May a switch be tried? The owner's policy, and under `auto` who is at the
/// screen. `presence` is only read under `auto`, so `never` and `always` cost
/// no OS call.
pub fn may_switch(
    policy: DisplayMatchPolicy,
    presence: impl FnOnce() -> Presence,
) -> Result<(), Refusal> {
    match policy {
        DisplayMatchPolicy::Never => Err(Refusal::DisabledOnDevice),
        DisplayMatchPolicy::Always => Ok(()),
        DisplayMatchPolicy::Auto => match presence() {
            Presence::Locked => Err(Refusal::Locked),
            Presence::Attended => Err(Refusal::Attended),
            Presence::Unattended => Ok(()),
        },
    }
}

/// A refusal that hands the screen back: the person is using it, or its owner
/// turned display match off. A mode this session applied earlier is restored.
/// (Not for `locked`: the worker cannot touch the secure desktop, P0.)
fn hands_the_screen_back(r: Refusal) -> bool {
    matches!(r, Refusal::Attended | Refusal::DisabledOnDevice)
}

/// FR-108 — a viewer asked to match `req_w × req_h`. Blocks on OS calls: run
/// it off the async threads. A request answered `locked` is kept for the
/// session's unlock (P1b, [`on_unlock`]); any other answer drops it.
pub fn request_for(session: bson::oid::ObjectId, req_w: u32, req_h: u32) -> Answer {
    let answer = decide_request(session, req_w, req_h);
    if answer == Answer::Refused(Refusal::Locked) {
        pending_set(session, req_w, req_h);
    } else {
        pending_forget(session);
    }
    answer
}

fn decide_request(session: bson::oid::ObjectId, req_w: u32, req_h: u32) -> Answer {
    if !cfg!(target_os = "windows") {
        return Answer::Refused(Refusal::Unsupported);
    }
    if let Err(r) = may_switch(device_policy(), crate::presence::now) {
        if hands_the_screen_back(r) && restore_for(session) {
            tracing::info!(%session, why = r.word(), "display-match: restored this session's mode");
        }
        tracing::info!(req_w, req_h, why = r.word(), "display-match: not switched");
        return Answer::Refused(r);
    }
    match apply_for(session, req_w, req_h) {
        Ok((w, h)) => {
            tracing::info!(
                req_w,
                req_h,
                mode_w = w,
                mode_h = h,
                "display-match: switched host display mode"
            );
            Answer::On { w, h }
        }
        Err(e) => {
            tracing::warn!(req_w, req_h, %e, "display-match: could not switch mode");
            Answer::Refused(e.into())
        }
    }
}

// ── FR-108 P1b: the locked host at the unlock, the person coming back ──────

/// Requests answered `locked`, kept per session for its unlock.
static PENDING: std::sync::Mutex<Vec<(bson::oid::ObjectId, u32, u32)>> =
    std::sync::Mutex::new(Vec::new());

fn pending_set(session: bson::oid::ObjectId, w: u32, h: u32) {
    let mut p = PENDING.lock().unwrap_or_else(|e| e.into_inner());
    p.retain(|(s, _, _)| *s != session);
    p.push((session, w, h));
}

fn pending_take(session: bson::oid::ObjectId) -> Option<(u32, u32)> {
    let mut p = PENDING.lock().unwrap_or_else(|e| e.into_inner());
    let i = p.iter().position(|(s, _, _)| *s == session)?;
    let (_, w, h) = p.remove(i);
    Some((w, h))
}

fn pending_forget(session: bson::oid::ObjectId) {
    PENDING
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|(s, _, _)| *s != session);
}

/// P1b — the session's screen unlocked: the request it was answered `locked`
/// for is decided again, now. Unlocked by the controller (machine input) with
/// no person since the lock, that switches; unlocked by the person, that is
/// `attended`. `None` when nothing was waiting. Blocks on OS calls.
pub fn on_unlock(session: bson::oid::ObjectId) -> Option<Answer> {
    let (w, h) = pending_take(session)?;
    Some(request_for(session, w, h))
}

/// P1b — does this session hold the mode it applied?
pub fn owns_mode(session: bson::oid::ObjectId) -> bool {
    *DM_OWNER.lock().unwrap_or_else(|e| e.into_inner()) == Some(session)
}

/// P1b — should a mode this session holds go back now, and why? Pure. The
/// owner turning display match off, or (under `auto`) a person at the screen.
/// Under `always` the owner said to keep it. `presence` is read only under
/// `auto`.
pub fn hand_back(
    owns: bool,
    policy: DisplayMatchPolicy,
    presence: impl FnOnce() -> Presence,
) -> Option<Refusal> {
    if !owns {
        return None;
    }
    match policy {
        DisplayMatchPolicy::Never => Some(Refusal::DisabledOnDevice),
        DisplayMatchPolicy::Always => None,
        DisplayMatchPolicy::Auto => (presence() == Presence::Attended).then_some(Refusal::Attended),
    }
}

/// P1b — the session's watcher, every quarter second while it holds a mode:
/// a person who came back, or the owner's `never`, gets the screen back at
/// once, and the viewer is told. It is not re-applied until the viewer asks
/// again. Blocks on OS calls.
pub fn on_tick(session: bson::oid::ObjectId) -> Option<Answer> {
    let why = hand_back(owns_mode(session), device_policy(), crate::presence::now)?;
    if !restore_for(session) {
        return None;
    }
    tracing::info!(%session, why = why.word(), "display-match: the screen is handed back");
    Some(Answer::Refused(why))
}

/// P1b — the session ended: drop what it was waiting for.
pub fn forget(session: bson::oid::ObjectId) {
    pending_forget(session);
}

/// The answer, as it goes to the viewer on the control channel.
pub fn state_json(a: Answer) -> String {
    let v = match a {
        Answer::On { w, h } => serde_json::json!({
            "t": "rc:display-match.state",
            "on": true,
            "mode": { "w": w, "h": h },
        }),
        Answer::Off => serde_json::json!({ "t": "rc:display-match.state", "on": false }),
        Answer::Refused(r) => serde_json::json!({
            "t": "rc:display-match.state",
            "on": false,
            "refused": r.word(),
        }),
    };
    v.to_string()
}

#[cfg(test)]
mod fr108_tests {
    use super::*;

    #[test]
    fn never_refuses_without_asking_who_is_there() {
        let r = may_switch(DisplayMatchPolicy::Never, || {
            panic!("presence must not be read under never")
        });
        assert_eq!(r, Err(Refusal::DisabledOnDevice));
    }

    #[test]
    fn always_switches_without_asking_who_is_there() {
        let r = may_switch(DisplayMatchPolicy::Always, || {
            panic!("presence must not be read under always")
        });
        assert_eq!(r, Ok(()));
    }

    #[test]
    fn auto_switches_only_an_unattended_screen() {
        assert_eq!(
            may_switch(DisplayMatchPolicy::Auto, || Presence::Unattended),
            Ok(())
        );
        assert_eq!(
            may_switch(DisplayMatchPolicy::Auto, || Presence::Attended),
            Err(Refusal::Attended)
        );
        assert_eq!(
            may_switch(DisplayMatchPolicy::Auto, || Presence::Locked),
            Err(Refusal::Locked)
        );
    }

    #[test]
    fn only_the_person_and_the_owner_take_the_screen_back() {
        assert!(hands_the_screen_back(Refusal::Attended));
        assert!(hands_the_screen_back(Refusal::DisabledOnDevice));
        for r in [
            Refusal::Locked,
            Refusal::NoFittingMode,
            Refusal::SwitchFailed,
            Refusal::Unsupported,
        ] {
            assert!(!hands_the_screen_back(r), "{r:?}");
        }
    }

    #[test]
    fn every_switch_failure_has_its_word() {
        let cases = [
            (
                ApplyError::NoFittingMode {
                    req_w: 946,
                    req_h: 598,
                    modes: 1,
                },
                "no_fitting_mode",
            ),
            (ApplyError::SwitchFailed(-1), "switch_failed"),
            (ApplyError::Unsupported, "unsupported"),
        ];
        for (e, word) in cases {
            assert_eq!(Refusal::from(e).word(), word, "{e}");
        }
        // rc.191's log text is unchanged: the field logs are grepped for it.
        assert_eq!(
            ApplyError::NoFittingMode {
                req_w: 946,
                req_h: 598,
                modes: 1
            }
            .to_string(),
            "no supported mode fits within 946x598 (1 modes)"
        );
        assert_eq!(
            ApplyError::SwitchFailed(-1).to_string(),
            "ChangeDisplaySettingsExW failed rc=-1"
        );
    }

    #[test]
    fn the_answer_on_the_wire() {
        let on: serde_json::Value =
            serde_json::from_str(&state_json(Answer::On { w: 1600, h: 900 })).unwrap();
        assert_eq!(
            on,
            serde_json::json!({"t": "rc:display-match.state", "on": true, "mode": {"w": 1600, "h": 900}})
        );
        let off: serde_json::Value = serde_json::from_str(&state_json(Answer::Off)).unwrap();
        assert_eq!(
            off,
            serde_json::json!({"t": "rc:display-match.state", "on": false})
        );
        let refused: serde_json::Value =
            serde_json::from_str(&state_json(Answer::Refused(Refusal::Attended))).unwrap();
        assert_eq!(
            refused,
            serde_json::json!({"t": "rc:display-match.state", "on": false, "refused": "attended"})
        );
    }

    #[test]
    fn the_policy_round_trips_through_the_static() {
        let was = device_policy();
        for p in [
            DisplayMatchPolicy::Never,
            DisplayMatchPolicy::Always,
            DisplayMatchPolicy::Auto,
        ] {
            set_device_policy(p);
            assert_eq!(device_policy(), p);
        }
        set_device_policy(was);
    }

    /// P1b — the screen goes back only from a session that holds it: to the
    /// owner's `never` at once, to a person under `auto`, never under `always`.
    #[test]
    fn hand_back_follows_the_owner_and_the_person() {
        let never_read = || -> Presence { panic!("presence must not be read here") };
        assert_eq!(hand_back(false, DisplayMatchPolicy::Auto, never_read), None);
        assert_eq!(
            hand_back(false, DisplayMatchPolicy::Never, never_read),
            None
        );
        assert_eq!(
            hand_back(true, DisplayMatchPolicy::Never, never_read),
            Some(Refusal::DisabledOnDevice)
        );
        assert_eq!(
            hand_back(true, DisplayMatchPolicy::Always, never_read),
            None
        );
        assert_eq!(
            hand_back(true, DisplayMatchPolicy::Auto, || Presence::Attended),
            Some(Refusal::Attended)
        );
        assert_eq!(
            hand_back(true, DisplayMatchPolicy::Auto, || Presence::Unattended),
            None
        );
        // A locked screen is not a person who came back (and the worker could
        // not restore through the secure desktop anyway).
        assert_eq!(
            hand_back(true, DisplayMatchPolicy::Auto, || Presence::Locked),
            None
        );
    }

    /// P1b — a request answered `locked` waits for its session's unlock: the
    /// latest one per session, taken once, and dropped when the session ends.
    #[test]
    fn a_locked_request_waits_for_its_session() {
        let a = bson::oid::ObjectId::new();
        let b = bson::oid::ObjectId::new();
        pending_set(a, 946, 598);
        pending_set(a, 1600, 900);
        pending_set(b, 800, 600);
        assert_eq!(pending_take(a), Some((1600, 900)), "the latest request");
        assert_eq!(pending_take(a), None, "taken once");
        forget(b);
        assert_eq!(pending_take(b), None, "the session ended");
        assert_eq!(on_unlock(a), None, "nothing waiting, nothing decided");
    }

    #[test]
    #[cfg(not(target_os = "windows"))]
    fn elsewhere_every_request_is_unsupported() {
        assert_eq!(
            request_for(bson::oid::ObjectId::new(), 1600, 900),
            Answer::Refused(Refusal::Unsupported)
        );
    }
}

#[cfg(test)]
mod ownership_tests {
    use super::*;

    /// P3 — the owner slot: apply_for records the owner; only the owner's
    /// restore_for runs (and clears the slot); a takeover moves ownership.
    /// Off-Windows `apply` errs, so drive the slot directly where needed.
    #[test]
    fn restore_is_owner_gated_and_takeover_moves_ownership() {
        let a = bson::oid::ObjectId::new();
        let b = bson::oid::ObjectId::new();
        // Seed ownership as a successful apply_for(a) would.
        *DM_OWNER.lock().unwrap() = Some(a);
        assert!(!restore_for(b), "non-owner must not restore");
        assert_eq!(*DM_OWNER.lock().unwrap(), Some(a), "owner unchanged");
        // Takeover: B applies (simulate the post-apply ownership write).
        *DM_OWNER.lock().unwrap() = Some(b);
        assert!(!restore_for(a), "displaced owner must not restore");
        assert!(restore_for(b), "current owner restores");
        assert_eq!(*DM_OWNER.lock().unwrap(), None, "slot cleared");
        assert!(!restore_for(b), "idempotent once cleared");
    }
}

#[cfg(test)]
mod tests {
    use super::pick_display_mode;

    const MODES: &[(u32, u32)] = &[
        (800, 600),
        (1024, 768),
        (1280, 720),
        (1280, 800),
        (1366, 768),
        (1600, 900),
        (1920, 1080),
        (2560, 1440),
        (3840, 2160),
    ];

    #[test]
    fn picks_largest_mode_fitting_the_stage() {
        // The field case: 1672×818 stage on a 4K panel → 1366×768 is the
        // largest mode that fits (1600×900 is too tall).
        assert_eq!(pick_display_mode(MODES, 1672, 818), Some((1366, 768)));
    }

    #[test]
    fn fullscreen_1200p_stage_gets_1080p_mode() {
        assert_eq!(pick_display_mode(MODES, 1920, 1200), Some((1920, 1080)));
    }

    #[test]
    fn exact_mode_match_is_used_verbatim() {
        assert_eq!(pick_display_mode(MODES, 1920, 1080), Some((1920, 1080)));
    }

    #[test]
    fn nothing_fits_returns_none() {
        assert_eq!(pick_display_mode(MODES, 640, 400), None);
    }

    #[test]
    fn aspect_breaks_area_ties() {
        // Two equal-area modes; the one whose aspect is closest to the
        // stage (1300/1010 ≈ 1.29) wins — 1024/1000 ≈ 1.02 beats
        // 1280/800 = 1.60.
        let modes = [(1280, 800), (1024, 1000)];
        assert_eq!(pick_display_mode(&modes, 1300, 1010), Some((1024, 1000)));
    }
}
