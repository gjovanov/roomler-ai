// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-92 — keep busy: synthetic mouse patterns that keep a host active, and
//! that any real user takes over at once.
//!
//! An `INPUT` session turns it on from the viewer; the agent then moves the
//! pointer in a pattern so the OS idle clock never runs out (no screensaver,
//! no idle lock, no "Away"). Any real input — the person at the machine or a
//! controller — pauses it; it resumes after `resume_after` of quiet, and
//! stays on across disconnects, self-updates and reboots until switched off.
//!
//! Layout:
//! * [`patterns`] — the shapes, pure geometry;
//! * [`engine`] — the state machine, pure (a `Host` trait carries every OS
//!   fact, so it is unit-tested with a scripted fake);
//! * [`wire`] — `rc:keep-busy.*` on the control data channel;
//! * [`state`] — the per-person store;
//! * this file — the engine's thread and its process-global handle.
//!
//! The engine runs on its own thread and asks the input arbiter — the ONE OS
//! injector — to read and move the cursor. It is never a second injector.
//! See `docs/fr/FR-92-keep-busy.md`.

pub mod engine;
pub mod patterns;
pub mod state;
pub mod wire;

#[cfg(all(target_os = "windows", feature = "enigo-input"))]
mod host_win;

pub use wire::SetRequest;

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bson::oid::ObjectId;

use engine::{Engine, Host, Reason, Settings, Snapshot};

/// Bumped by the input arbiter on every controller-caused injection, so the
/// engine knows — exactly — that a remote person is active.
static REMOTE_INPUT_EPOCH: AtomicU64 = AtomicU64::new(0);

/// A controller's input reached the OS. Called by the arbiter for every
/// injected session event (never a heartbeat, never a denied event).
pub fn note_remote_input() {
    REMOTE_INPUT_EPOCH.fetch_add(1, Ordering::Relaxed);
}

/// The current remote-input epoch.
pub fn remote_input_epoch() -> u64 {
    REMOTE_INPUT_EPOCH.load(Ordering::Relaxed)
}

/// Is keep-busy built for this host at all? v1 is Windows (FR-92 P1);
/// macOS and X11 follow in P3, Wayland later. The cap word also needs the
/// device owner's `keep_busy_enabled`.
pub fn supported_here() -> bool {
    cfg!(all(target_os = "windows", feature = "enigo-input"))
}

/// What the engine thread is asked to do.
#[derive(Debug)]
pub enum Command {
    Set {
        session: Option<ObjectId>,
        set_by: String,
        settings: Settings,
    },
    Off {
        why: Reason,
    },
    DeviceEnabled(bool),
    OrgDenied(bool),
}

struct Service {
    tx: SyncSender<Command>,
    state: tokio::sync::watch::Receiver<Snapshot>,
}

static SERVICE: OnceLock<Service> = OnceLock::new();
/// The device owner's `keep_busy_enabled`, mirrored for the cap word.
static DEVICE_ENABLED: AtomicBool = AtomicBool::new(true);

pub(crate) fn wall_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

/// Start the engine thread — once per process, from inside the tokio
/// runtime. `device_enabled` is the owner's `keep_busy_enabled`.
///
/// ⚠️ It initialises the input arbiter FIRST: `arbiter::global()` captures
/// the tokio handle at its first call, and the engine thread (a plain std
/// thread) must never be that first caller.
pub fn start(device_enabled: bool) {
    DEVICE_ENABLED.store(device_enabled, Ordering::Relaxed);
    if !supported_here() {
        return;
    }
    SERVICE.get_or_init(|| {
        let _ = crate::input::arbiter::global();
        let (tx, rx) = sync_channel::<Command>(64);
        let now = Instant::now();
        let mut engine = Engine::new(wall_ms() ^ u64::from(std::process::id()));
        engine.set_device_enabled(device_enabled, now);
        let (out, state) = tokio::sync::watch::channel(engine.snapshot(now));
        std::thread::Builder::new()
            .name("keep-busy".into())
            .spawn(move || run(engine, rx, out))
            .expect("spawn keep-busy thread");
        Service { tx, state }
    });
}

fn submit(cmd: Command) {
    if let Some(s) = SERVICE.get()
        && s.tx.try_send(cmd).is_err()
    {
        tracing::warn!("keep-busy: command queue full — command dropped");
    }
}

/// A controller's `rc:keep-busy.set`, already authorised by the arbiter
/// (the session holds INPUT, the floor, and its org allows it). `false`
/// when this host cannot run keep-busy — the arbiter then answers that
/// viewer with the reason, since no engine broadcast will.
pub fn submit_from_session(session: ObjectId, name: String, req: SetRequest) -> bool {
    if !supported_here() || SERVICE.get().is_none() {
        tracing::info!(%session, "keep-busy: set from a session on a host that cannot run it");
        return false;
    }
    submit(if req.on {
        Command::Set {
            session: Some(session),
            set_by: name,
            settings: req.settings,
        }
    } else {
        Command::Off {
            why: Reason::StoppedByController,
        }
    });
    true
}

/// The person at the machine stops it (LocalAPI, tray, CLI).
pub fn stop_locally() {
    submit(Command::Off {
        why: Reason::StoppedLocally,
    });
}

/// The owner's `keep_busy_enabled` changed (it is a live key).
pub fn set_device_enabled(enabled: bool) {
    DEVICE_ENABLED.store(enabled, Ordering::Relaxed);
    submit(Command::DeviceEnabled(enabled));
}

/// The org's deny (strictest of every enrolled org).
pub fn set_org_denied(denied: bool) {
    submit(Command::OrgDenied(denied));
}

/// Should `AgentCaps.input` carry `keep-busy`? Built for this host, the
/// engine running, and the device owner allows it. An org deny does NOT
/// remove the word: the viewer shows the control disabled with the reason,
/// which says more than a control that silently vanished.
pub fn advertised() -> bool {
    supported_here() && SERVICE.get().is_some() && DEVICE_ENABLED.load(Ordering::Relaxed)
}

/// The current public state (a "not here" state when the engine is not
/// running on this host).
pub fn snapshot() -> Snapshot {
    match SERVICE.get() {
        Some(s) => s.state.borrow().clone(),
        None => Snapshot {
            rev: 0,
            available: false,
            on: false,
            phase: engine::Phase::Unavailable,
            reason: Some(Reason::Unsupported),
            paused_by: None,
            resumes_in: None,
            settings: Settings::default(),
            set_by: None,
            set_at_ms: None,
            detector: "none",
            warn: Vec::new(),
        },
    }
}

/// A watch of the public state, for the per-session emitters and the
/// LocalAPI. `None` when the engine is not running on this host.
pub fn subscribe() -> Option<tokio::sync::watch::Receiver<Snapshot>> {
    SERVICE.get().map(|s| s.state.clone())
}

/// `rc:keep-busy.state` as JSON text. `refused` names why the receiving
/// viewer's own request was not applied.
pub fn state_json(refused: Option<&str>) -> String {
    wire::state_payload(&snapshot(), refused).to_string()
}

/// How often host warnings (focus-follows-mouse, …) are re-read.
const WARNINGS_EVERY: Duration = Duration::from_secs(300);

fn run(engine: Engine, rx: Receiver<Command>, out: tokio::sync::watch::Sender<Snapshot>) {
    #[cfg(all(target_os = "windows", feature = "enigo-input"))]
    {
        run_with(
            engine,
            rx,
            out,
            host_win::WinHost::new(),
            host_win::warnings,
        );
    }
    #[cfg(not(all(target_os = "windows", feature = "enigo-input")))]
    {
        let _ = (engine, rx, out);
    }
}

#[cfg_attr(
    not(all(target_os = "windows", feature = "enigo-input")),
    allow(dead_code)
)]
fn run_with<H: Host>(
    mut engine: Engine,
    rx: Receiver<Command>,
    out: tokio::sync::watch::Sender<Snapshot>,
    mut host: H,
    warnings: fn() -> Vec<&'static str>,
) {
    let path = state::default_path();
    let mut stored = path.as_deref().map(state::load_from).unwrap_or_default();
    let start = Instant::now();
    if stored.org_denied.values().any(|d| *d) {
        engine.set_org_denied(true, start);
    }
    if let Some(act) = stored.activation(wall_ms()) {
        tracing::info!(
            pattern = act.settings.pattern.wire(),
            set_by = %act.set_by,
            "keep-busy: restored from the per-person store — waiting for an unlocked session"
        );
        engine.restore(act, start);
    }
    engine.set_warnings(warnings());
    let mut warnings_at = start + WARNINGS_EVERY;
    let mut last_act = engine.activation().cloned();
    let mut published = 0u64;
    let mut last_logged = (engine.snapshot(start).phase, engine.snapshot(start).reason);

    loop {
        let msg = match engine.next_wake() {
            Some(w) => rx.recv_timeout(w.saturating_duration_since(Instant::now())),
            None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        let now = Instant::now();
        match msg {
            Ok(cmd) => apply(&mut engine, cmd, now),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        // Deadline-driven: a steady stream of commands must not starve the
        // ticks (a `recv_timeout` that keeps returning early never times out).
        if engine.next_wake().is_some_and(|w| Instant::now() >= w) {
            engine.tick(&mut host);
        }
        if now >= warnings_at {
            engine.set_warnings(warnings());
            warnings_at = now + WARNINGS_EVERY;
        }
        // Persist on/off edges only — never pause/resume.
        if engine.activation() != last_act.as_ref() {
            last_act = engine.activation().cloned();
            stored = stored.with_activation(last_act.as_ref());
            if let Some(p) = path.as_deref()
                && let Err(e) = state::save_to(p, &stored)
            {
                tracing::warn!(path = %p.display(), error = %e, "keep-busy: could not save the store");
            }
        }
        if engine.rev() != published {
            published = engine.rev();
            let snap = engine.snapshot(Instant::now());
            if (snap.phase, snap.reason) != last_logged {
                last_logged = (snap.phase, snap.reason);
                tracing::info!(
                    on = snap.on,
                    phase = snap.phase.wire(),
                    reason = snap.reason.map(Reason::wire).unwrap_or("-"),
                    paused_by = snap.paused_by.map(engine::Source::wire).unwrap_or("-"),
                    "keep-busy: state"
                );
            }
            let _ = out.send(snap);
        }
    }
    tracing::info!("keep-busy: engine thread exiting");
}

fn apply(engine: &mut Engine, cmd: Command, now: Instant) {
    match cmd {
        Command::Set {
            session,
            set_by,
            settings,
        } => {
            let pattern = settings.pattern.wire();
            match engine.set_on(settings, set_by.clone(), wall_ms(), now) {
                Ok(()) => tracing::info!(
                    session = ?session.map(|s| s.to_hex()),
                    %set_by,
                    pattern,
                    "keep-busy: turned on"
                ),
                Err(r) => {
                    tracing::info!(%set_by, reason = r.wire(), "keep-busy: turn-on refused");
                    // Publish anyway: the asking viewer must see why.
                    engine.touch();
                }
            }
        }
        Command::Off { why } => {
            tracing::info!(reason = why.wire(), "keep-busy: turned off");
            engine.turn_off(why, now);
        }
        Command::DeviceEnabled(b) => engine.set_device_enabled(b, now),
        Command::OrgDenied(b) => engine.set_org_denied(b, now),
    }
}

/// Virtual-screen pixel → the `0..=65535` range Windows'
/// `MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK` wants, rounded to
/// nearest, for a virtual screen at `(vx, vy)` of `vw × vh`. Pure and
/// ungated so the mapping is tested on every lane, not only where the
/// Windows input backend compiles.
pub fn normalise_virtual(x: i32, y: i32, vx: i32, vy: i32, vw: i32, vh: i32) -> Option<(i32, i32)> {
    if vw < 2 || vh < 2 {
        return None;
    }
    let map = |p: i32, origin: i32, span: i32| -> i32 {
        let local = (p - origin).clamp(0, span - 1) as i64;
        ((local * 65535 + (span as i64 - 1) / 2) / (span as i64 - 1)) as i32
    };
    Some((map(x, vx, vw), map(y, vy, vh)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Inverse of what Windows does with an absolute VIRTUALDESK move:
    /// `pixel = origin + round(norm * (span - 1) / 65535)`.
    fn back(n: i32, origin: i32, span: i32) -> i32 {
        origin + ((n as f64) * (span - 1) as f64 / 65535.0).round() as i32
    }

    #[test]
    fn virtual_desk_mapping_round_trips_every_pixel_on_odd_layouts() {
        // A primary at (0,0) and a second monitor to its LEFT and above: the
        // virtual screen starts at a negative origin.
        for (vx, vy, vw, vh) in [
            (0, 0, 1920, 1080),
            (-2560, -360, 4480, 1440),
            (0, 0, 7680, 2160),
        ] {
            for x in (vx..vx + vw).step_by(7) {
                let (nx, _) = normalise_virtual(x, vy, vx, vy, vw, vh).unwrap();
                assert!((0..=65535).contains(&nx));
                assert_eq!(back(nx, vx, vw), x, "x={x} on {vw}@{vx}");
            }
            for y in (vy..vy + vh).step_by(5) {
                let (_, ny) = normalise_virtual(vx, y, vx, vy, vw, vh).unwrap();
                assert_eq!(back(ny, vy, vh), y, "y={y} on {vh}@{vy}");
            }
        }
    }

    #[test]
    fn virtual_desk_mapping_clamps_and_refuses_a_degenerate_screen() {
        assert_eq!(normalise_virtual(-10, -10, 0, 0, 1920, 1080), Some((0, 0)));
        assert_eq!(
            normalise_virtual(99_999, 99_999, 0, 0, 1920, 1080),
            Some((65535, 65535))
        );
        assert_eq!(normalise_virtual(0, 0, 0, 0, 1, 1080), None);
    }

    #[test]
    fn the_remote_epoch_only_moves_forward() {
        let a = remote_input_epoch();
        note_remote_input();
        assert!(remote_input_epoch() > a);
    }

    /// Off this host's build (no Windows input backend) nothing is
    /// advertised and the state says why.
    #[test]
    fn without_the_engine_nothing_is_advertised_and_the_state_says_why() {
        if supported_here() {
            return;
        }
        assert!(!advertised());
        let s = snapshot();
        assert!(!s.available);
        assert_eq!(s.reason, Some(Reason::Unsupported));
        let v: serde_json::Value = serde_json::from_str(&state_json(None)).unwrap();
        assert_eq!(v["reason"], "unsupported");
    }
}
