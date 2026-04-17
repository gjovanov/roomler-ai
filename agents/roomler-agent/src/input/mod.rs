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

#[cfg(feature = "enigo-input")]
pub mod enigo_backend;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Button { Left, Right, Middle, Back, Forward }

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WheelMode { Pixel, Line, Page }

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TouchPhase { Start, Move, End, Cancel }

/// Input event from the controller. Coordinates are normalised 0..1 per
/// monitor so the agent's resolution can change mid-session without
/// needing a resync.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum InputMsg {
    MouseMove { x: f32, y: f32, mon: u8 },
    MouseButton { btn: Button, down: bool, x: f32, y: f32, mon: u8 },
    MouseWheel { dx: f32, dy: f32, mode: WheelMode },
    Key { code: u32, down: bool, mods: u8 },
    KeyText { text: String },
    Touch { id: u32, phase: TouchPhase, x: f32, y: f32, mon: u8 },
    Heartbeat { seq: u64, ts_ms: u64 },
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
    fn has_permission(&self) -> bool { false }
}

/// Open the best-available input backend for the current host. Falls
/// back to [`NoopInjector`] when enigo-input is off or init fails.
pub fn open_default() -> Box<dyn InputInjector + Send> {
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
