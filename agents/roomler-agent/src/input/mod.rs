//! OS input injection abstraction.
//!
//! The browser controller emits `InputMsg` values (mouse move / click /
//! wheel / key / touch — see docs/remote-control.md §6); the agent maps
//! them to OS-native input events via this trait.
//!
//! Backends:
//! - [`enigo_backend::EnigoInjector`] (feature `enigo-input`) — uses
//!   enigo which dispatches to XTest/uinput on Linux, SendInput on
//!   Windows, CGEventPost on macOS.
//! - [`NoopInjector`] — fallback when no backend feature is enabled.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::sync::atomic::AtomicU32;

/// Process-global counter for `to_pixels` diagnostic logging. The first
/// 50 increments are logged at INFO level; subsequent events drop to
/// DEBUG. [`reset_input_diag_counter`] is called from
/// `peer::attach_input_handler` so EVERY session gets the first-50
/// INFO sample, not just the first session after process start.
///
/// rc.57 — promoted from a function-local static in `enigo_backend.rs`
/// to a module-level static so peer.rs can reset it from outside the
/// `enigo-input` feature gate. The field log from rc.55 ran out of
/// INFO samples after session 1 — the subsequent Crystal-Clear-OFF
/// session (where the misposition reproduces) had only DEBUG dispatch
/// lines, hiding the per-event norm/px math.
pub(crate) static INPUT_DIAG_COUNT: AtomicU32 = AtomicU32::new(0);

/// Reset the per-process `to_pixels` diagnostic counter so the next 50
/// input events are logged at INFO. Called from
/// `peer::attach_input_handler` each time the `input` DC opens (once
/// per session).
pub fn reset_input_diag_counter() {
    INPUT_DIAG_COUNT.store(0, std::sync::atomic::Ordering::Relaxed);
}

/// Parse the `ROOMLER_AGENT_VIRTUAL_SCREEN` env-var value into a
/// `bool`. Accepts `1`, `true`, `yes`, `on` (case-insensitive, trimmed)
/// as truthy; anything else (including `None`) is false.
///
/// Lives here (in `input/mod.rs`) rather than `enigo_backend.rs` so
/// it's reachable from `main.rs` regardless of whether the
/// `enigo-input` feature is on for this build — the startup log line
/// surfaces the gate value even on signalling-only builds.
///
/// rc.54 — gate for the virtual-screen-aware `to_pixels` path.
pub fn parse_virtual_screen_flag(value: Option<&str>) -> bool {
    match value {
        Some(v) => matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        ),
        None => false,
    }
}

#[cfg(feature = "enigo-input")]
pub mod enigo_backend;

#[cfg(all(
    feature = "system-context",
    target_os = "windows",
    feature = "enigo-input"
))]
pub mod system_context_backend;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Button {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WheelMode {
    Pixel,
    Line,
    Page,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TouchPhase {
    Start,
    Move,
    End,
    Cancel,
}

/// Input event from the controller. Coordinates are normalised 0..1 per
/// monitor so the agent's resolution can change mid-session without
/// needing a resync.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum InputMsg {
    MouseMove {
        x: f32,
        y: f32,
        mon: u8,
    },
    MouseButton {
        btn: Button,
        down: bool,
        x: f32,
        y: f32,
        mon: u8,
    },
    MouseWheel {
        dx: f32,
        dy: f32,
        mode: WheelMode,
    },
    Key {
        code: u32,
        down: bool,
        mods: u8,
    },
    KeyText {
        text: String,
    },
    Touch {
        id: u32,
        phase: TouchPhase,
        x: f32,
        y: f32,
        mon: u8,
    },
    Heartbeat {
        seq: u64,
        ts_ms: u64,
    },
}

pub trait InputInjector: Send {
    fn inject(&mut self, event: InputMsg) -> Result<()>;
    /// Whether the backend currently has the OS permission to inject.
    /// On macOS this maps to the Accessibility privilege; on Wayland to
    /// membership in the `input` group / uinput permission.
    fn has_permission(&self) -> bool;
}

pub struct NoopInjector;

impl InputInjector for NoopInjector {
    fn inject(&mut self, _event: InputMsg) -> Result<()> {
        Ok(())
    }
    fn has_permission(&self) -> bool {
        false
    }
}

/// Open the best-available input backend for the current host. Falls
/// back to [`NoopInjector`] when enigo-input is off or init fails.
///
/// M3 A1: when the agent is built with the `system-context` feature
/// AND the worker probes as `WorkerRole::SystemContext` at startup
/// (i.e. it was spawned by the SCM service via
/// `winlogon_token::spawn_system_in_session`), route to
/// [`system_context_backend::SystemContextInjector`] which adds a
/// per-event `SetThreadDesktop` rebind preamble. User-context workers
/// fall through to [`enigo_backend::EnigoInjector`] with no behaviour
/// change.
pub fn open_default() -> Box<dyn InputInjector + Send> {
    #[cfg(all(
        feature = "system-context",
        target_os = "windows",
        feature = "enigo-input"
    ))]
    {
        use crate::system_context::worker_role::{WorkerRole, probe_self};
        match probe_self() {
            Ok(WorkerRole::SystemContext) => {
                match system_context_backend::SystemContextInjector::new() {
                    Ok(e) => {
                        tracing::info!(
                            "input: backend=system-context (enigo with SetThreadDesktop rebind)"
                        );
                        return Box::new(e);
                    }
                    Err(e) => {
                        tracing::warn!(
                            %e,
                            "system-context input init failed — falling through to standard enigo backend"
                        );
                    }
                }
            }
            Ok(WorkerRole::User) => {
                // Normal path. Fall through to the user-context
                // EnigoInjector below.
            }
            Err(e) => {
                tracing::warn!(
                    %e,
                    "worker_role::probe_self in input::open_default failed — assuming user-context"
                );
            }
        }
    }
    #[cfg(feature = "enigo-input")]
    {
        match enigo_backend::EnigoInjector::new() {
            Ok(e) => return Box::new(e),
            Err(e) => {
                tracing::warn!(%e, "enigo init failed — input injection disabled");
            }
        }
    }
    #[cfg(not(feature = "enigo-input"))]
    {
        tracing::info!(
            "built without enigo-input feature — input events will be dropped. \
             Rebuild with `--features enigo-input` (or `--features full`)."
        );
    }
    Box::new(NoopInjector)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// rc.54 — verify the env-var parse rules. Covers truthy variants
    /// (case + whitespace), explicit false values, missing var, and
    /// unrecognised values (which must default to false to keep the
    /// gate fail-closed).
    #[test]
    fn parse_virtual_screen_flag_accepts_truthy_values() {
        assert!(parse_virtual_screen_flag(Some("1")));
        assert!(parse_virtual_screen_flag(Some("true")));
        assert!(parse_virtual_screen_flag(Some("True")));
        assert!(parse_virtual_screen_flag(Some("TRUE")));
        assert!(parse_virtual_screen_flag(Some("yes")));
        assert!(parse_virtual_screen_flag(Some("on")));
        assert!(parse_virtual_screen_flag(Some("  1  ")));
        assert!(parse_virtual_screen_flag(Some("\tTrue\n")));
    }

    /// rc.57 — verify the diagnostic counter actually resets. Cargo
    /// runs tests in parallel within a binary by default, but only one
    /// test below mutates `INPUT_DIAG_COUNT` so there's no inter-test
    /// race. If another test ever touches the counter, gate this with
    /// `#[serial_test::serial]` or move both to a single test.
    #[test]
    fn reset_input_diag_counter_zeroes_the_counter() {
        use std::sync::atomic::Ordering;
        // Set to non-zero to prove the reset has work to do.
        INPUT_DIAG_COUNT.store(42, Ordering::Relaxed);
        assert_eq!(INPUT_DIAG_COUNT.load(Ordering::Relaxed), 42);
        reset_input_diag_counter();
        assert_eq!(INPUT_DIAG_COUNT.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn parse_virtual_screen_flag_rejects_falsy_values() {
        assert!(!parse_virtual_screen_flag(None));
        assert!(!parse_virtual_screen_flag(Some("")));
        assert!(!parse_virtual_screen_flag(Some("0")));
        assert!(!parse_virtual_screen_flag(Some("false")));
        assert!(!parse_virtual_screen_flag(Some("no")));
        assert!(!parse_virtual_screen_flag(Some("off")));
        // Unrecognised values must be false — fail-closed gate.
        assert!(!parse_virtual_screen_flag(Some("maybe")));
        assert!(!parse_virtual_screen_flag(Some("2")));
        assert!(!parse_virtual_screen_flag(Some("enabled")));
    }
}
