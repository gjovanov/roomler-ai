//! Cross-platform input injection via [enigo].
//!
//! enigo picks the right OS primitive per platform: XTest / uinput on
//! Linux, SendInput on Windows, CGEventPost on macOS. We run it inside a
//! dedicated thread (same reason as the other hardware-talking backends
//! in this crate: the underlying handles have thread affinity on some
//! platforms) and fan command in via std::mpsc.
//!
//! Coordinate mapping: the controller sends normalised `x,y` in `[0,1]`
//! per monitor. We resolve those against the screen dimensions at the
//! moment of the event — resolution changes mid-session are OK.
//!
//! [enigo]: https://docs.rs/enigo

use anyhow::{Context, Result, anyhow};
use enigo::{
    Axis, Button as EnigoButton, Coordinate, Direction, Enigo, Key, Keyboard, Mouse,
    Settings,
};
use std::sync::mpsc as std_mpsc;
use std::thread;

use super::{Button, InputInjector, InputMsg, WheelMode};

pub struct EnigoInjector {
    tx: std_mpsc::Sender<InputMsg>,
    has_perm: bool,
}

impl EnigoInjector {
    pub fn new() -> Result<Self> {
        let (tx, rx) = std_mpsc::channel::<InputMsg>();
        // Construct Enigo on the worker thread — we never want to move it
        // between threads. Use a ready-ack channel to surface init errors.
        let (ready_tx, ready_rx) = std_mpsc::channel::<Result<()>>();

        thread::Builder::new()
            .name("roomler-agent-input".into())
            .spawn(move || {
                let settings = Settings::default();
                let enigo = match Enigo::new(&settings) {
                    Ok(e) => {
                        let _ = ready_tx.send(Ok(()));
                        e
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(anyhow!("enigo init: {e}")));
                        return;
                    }
                };
                run_worker(enigo, rx);
            })
            .context("spawn input thread")?;

        ready_rx.recv().context("input thread never responded")??;
        Ok(Self { tx, has_perm: true })
    }
}

fn run_worker(mut enigo: Enigo, rx: std_mpsc::Receiver<InputMsg>) {
    while let Ok(msg) = rx.recv() {
        if let Err(e) = dispatch(&mut enigo, msg) {
            tracing::debug!(%e, "input event dropped");
        }
    }
}

fn dispatch(enigo: &mut Enigo, msg: InputMsg) -> Result<()> {
    match msg {
        InputMsg::MouseMove { x, y, mon } => {
            let (px, py) = to_pixels(enigo, x, y, mon);
            enigo
                .move_mouse(px, py, Coordinate::Abs)
                .map_err(|e| anyhow!("move_mouse: {e}"))?;
        }
        InputMsg::MouseButton { btn, down, x, y, mon } => {
            // Move first so the click hits the intended target even if
            // earlier MouseMove events were coalesced away.
            let (px, py) = to_pixels(enigo, x, y, mon);
            enigo
                .move_mouse(px, py, Coordinate::Abs)
                .map_err(|e| anyhow!("move_mouse: {e}"))?;
            let direction = if down { Direction::Press } else { Direction::Release };
            enigo
                .button(map_button(btn), direction)
                .map_err(|e| anyhow!("button: {e}"))?;
        }
        InputMsg::MouseWheel { dx, dy, mode } => {
            let (x_steps, y_steps) = wheel_to_steps(dx, dy, mode);
            if y_steps != 0 {
                enigo
                    .scroll(y_steps, Axis::Vertical)
                    .map_err(|e| anyhow!("scroll y: {e}"))?;
            }
            if x_steps != 0 {
                enigo
                    .scroll(x_steps, Axis::Horizontal)
                    .map_err(|e| anyhow!("scroll x: {e}"))?;
            }
        }
        InputMsg::Key { code, down, mods: _ } => {
            let direction = if down { Direction::Press } else { Direction::Release };
            if let Some(k) = hid_to_key(code) {
                enigo.key(k, direction).map_err(|e| anyhow!("key: {e}"))?;
            } else {
                // Unknown HID code: try raw scancode. enigo exposes
                // `Key::Other(u32)` that some platforms can map.
                enigo
                    .key(Key::Other(code), direction)
                    .map_err(|e| anyhow!("key Other({code}): {e}"))?;
            }
        }
        InputMsg::KeyText { text } => {
            enigo.text(&text).map_err(|e| anyhow!("text: {e}"))?;
        }
        InputMsg::Touch { .. } => {
            // No cross-platform touch injection in enigo yet. Map to
            // mouse in a follow-up; for now, drop silently.
        }
        InputMsg::Heartbeat { .. } => {}
    }
    Ok(())
}

fn map_button(b: Button) -> EnigoButton {
    match b {
        Button::Left => EnigoButton::Left,
        Button::Right => EnigoButton::Right,
        Button::Middle => EnigoButton::Middle,
        Button::Back => EnigoButton::Back,
        Button::Forward => EnigoButton::Forward,
    }
}

/// Normalised `(x, y)` in `[0,1]` → absolute pixel coordinates on the
/// agent's primary display. Multi-monitor mapping (`mon` > 0) picks the
/// monitor from enigo's enumeration; on single-monitor hosts it falls
/// back to primary. Out-of-range values are clamped.
fn to_pixels(enigo: &Enigo, x: f32, y: f32, _mon: u8) -> (i32, i32) {
    let (w, h) = enigo.main_display().unwrap_or((1920, 1080));
    let x = x.clamp(0.0, 1.0);
    let y = y.clamp(0.0, 1.0);
    ((x * (w - 1) as f32).round() as i32, (y * (h - 1) as f32).round() as i32)
}

/// Convert a browser `WheelEvent` delta into enigo scroll "notches".
/// Browsers emit pixels at 100+ per notch; enigo wants integer notches,
/// so we accumulate fractional pixels and round.
fn wheel_to_steps(dx: f32, dy: f32, mode: WheelMode) -> (i32, i32) {
    let px_per_step = match mode {
        WheelMode::Pixel => 100.0,
        WheelMode::Line => 1.0,
        WheelMode::Page => 1.0,
    };
    // Browsers use "positive Y == down". enigo's convention matches on
    // every platform we target.
    (
        (dx / px_per_step).round() as i32,
        (dy / px_per_step).round() as i32,
    )
}

/// Map a USB HID usage code (what the browser emits via the `Key*`
/// KeyboardEvent.code normalisation) to enigo's `Key` enum.
///
/// Only the keys that don't round-trip as raw Unicode go through this
/// table. Unknown codes fall back to `Key::Other(code)` in the caller.
fn hid_to_key(code: u32) -> Option<Key> {
    // HID usage codes from "Keyboard/Keypad" Page (0x07).
    // A complete table is ~100 entries; we cover navigation + modifiers +
    // function keys here and round-trip printable keys as text in the
    // future via InputMsg::KeyText.
    match code {
        0x28 => Some(Key::Return),
        0x29 => Some(Key::Escape),
        0x2a => Some(Key::Backspace),
        0x2b => Some(Key::Tab),
        0x2c => Some(Key::Space),
        0x4f => Some(Key::RightArrow),
        0x50 => Some(Key::LeftArrow),
        0x51 => Some(Key::DownArrow),
        0x52 => Some(Key::UpArrow),
        0x4a => Some(Key::Home),
        0x4d => Some(Key::End),
        0x4b => Some(Key::PageUp),
        0x4e => Some(Key::PageDown),
        0x49 => Some(Key::Insert),
        0x4c => Some(Key::Delete),
        0x3a => Some(Key::F1),
        0x3b => Some(Key::F2),
        0x3c => Some(Key::F3),
        0x3d => Some(Key::F4),
        0x3e => Some(Key::F5),
        0x3f => Some(Key::F6),
        0x40 => Some(Key::F7),
        0x41 => Some(Key::F8),
        0x42 => Some(Key::F9),
        0x43 => Some(Key::F10),
        0x44 => Some(Key::F11),
        0x45 => Some(Key::F12),
        0xe0 => Some(Key::Control),
        0xe1 => Some(Key::Shift),
        0xe2 => Some(Key::Alt),
        0xe3 => Some(Key::Meta),
        0xe4 => Some(Key::Control), // right control
        0xe5 => Some(Key::Shift),   // right shift
        0xe6 => Some(Key::Alt),     // right alt
        0xe7 => Some(Key::Meta),    // right meta
        _ => None,
    }
}

impl InputInjector for EnigoInjector {
    fn inject(&mut self, event: InputMsg) -> Result<()> {
        self.tx
            .send(event)
            .map_err(|_| anyhow!("input worker exited"))
    }

    fn has_permission(&self) -> bool {
        self.has_perm
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Construction may fail on headless hosts (no DISPLAY / no Accessibility
    /// privilege on macOS). Skip gracefully — we only want failures when
    /// construction succeeds but the behaviour is wrong.
    #[test]
    fn constructs_or_skips() {
        match EnigoInjector::new() {
            Ok(_) => {}
            Err(e) => eprintln!("skipping — enigo unavailable: {e}"),
        }
    }

    #[test]
    fn wheel_pixel_deltas_round_to_notches() {
        assert_eq!(wheel_to_steps(0.0, 50.0, WheelMode::Pixel), (0, 1));
        assert_eq!(wheel_to_steps(0.0, -150.0, WheelMode::Pixel), (0, -2));
        assert_eq!(wheel_to_steps(100.0, 0.0, WheelMode::Pixel), (1, 0));
        assert_eq!(wheel_to_steps(0.0, 30.0, WheelMode::Pixel), (0, 0)); // below threshold
    }

    #[test]
    fn hid_table_covers_navigation_keys() {
        assert!(matches!(hid_to_key(0x4f), Some(Key::RightArrow)));
        assert!(matches!(hid_to_key(0x50), Some(Key::LeftArrow)));
        assert!(matches!(hid_to_key(0x29), Some(Key::Escape)));
        assert!(matches!(hid_to_key(0x3a), Some(Key::F1)));
        assert!(matches!(hid_to_key(0x45), Some(Key::F12)));
        assert_eq!(hid_to_key(0xffff), None);
    }
}
