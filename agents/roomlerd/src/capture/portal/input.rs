// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-45 P4 — input through the portal's **RemoteDesktop** interface.
//!
//! The pair to [`super::backend`]'s capture half: on a host whose desktop only
//! the portal can see, the portal is also the only thing that can *touch* it.
//! Measured on WSL2 (FR-45 field log, 2026-08-31): uinput happily creates a
//! device and libinput enumerates it, but a nested compositor reads its
//! PARENT, not evdev — the writer works and the reader is missing. So capture
//! without this is a read-only session on exactly the hosts FR-45 exists for.
//!
//! ## Where this runs
//!
//! In the **helper**, which holds the portal session. The daemon's input
//! arbiter forwards [`InputMsg`] values as JSON lines on the helper's stdin
//! (see [`super::helper`]); this module maps each to `Notify*` calls on the
//! session. Same binary on both ends, so the line format cannot skew.
//!
//! ## The mapping is a pure function
//!
//! [`plan`] turns an `InputMsg` into [`PortalCall`]s with no D-Bus in sight,
//! and [`InputContext`] executes them. The split is what makes the mapping
//! testable — the tests pin the coordinate scaling, the button codes, the
//! wheel-detent accumulation and the keysym rules without a portal anywhere.
//!
//! ## Conventions worth pinning
//!
//! - `NotifyKeyboardKeycode` takes **evdev** keycodes (KEY_A = 30) — not
//!   X keycodes (evdev+8), not keysyms. gnome-remote-desktop sends exactly
//!   these; the shared [`hid_to_evdev`] table already speaks them.
//! - `KeyText` goes through **keysyms**, where Unicode makes it layout-proof:
//!   keysym = codepoint for U+0020..U+00FF, codepoint + 0x0100_0000 above.
//!   The compositor resolves the keysym against its own keymap, so this types
//!   correctly on layouts the uinput backend refuses (its table is physical).
//! - Axis sign follows libinput: **positive is down/right**, matching the
//!   browser's wheel delta directly. ⚠️ evdev `REL_WHEEL` is the opposite —
//!   copying the uinput backend's inversion here would scroll backwards.

use crate::input::hid_evdev::hid_to_evdev;
use crate::input::{Button, InputMsg, WheelMode};

/// `NotifyPointerButton` wants Linux `BTN_*` codes.
fn button_code(btn: Button) -> i32 {
    match btn {
        Button::Left => 0x110,
        Button::Right => 0x111,
        Button::Middle => 0x112,
        Button::Back => 0x113,
        Button::Forward => 0x114,
    }
}

/// The keysym for one typed character, or `None` for a control character we
/// cannot honestly express.
///
/// The rules are X11's: Latin-1 (U+0020..=U+00FF) IS the keysym range
/// 0x20..=0xFF; everything above rides the Unicode escape 0x0100_0000 + cp.
/// Return and Tab are the two C0 controls a text field actually receives.
fn keysym_of(ch: char) -> Option<u32> {
    match ch {
        '\n' | '\r' => Some(0xFF0D), // XK_Return
        '\t' => Some(0xFF09),        // XK_Tab
        c => {
            let cp = c as u32;
            match cp {
                0x20..=0xFF => Some(cp),
                0x100.. => Some(0x0100_0000 + cp),
                _ => None,
            }
        }
    }
}

/// One portal call, as data. `plan` produces these; `InputContext` executes
/// them; the tests read them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PortalCall {
    /// `NotifyPointerMotionAbsolute` — coordinates in the STREAM's logical
    /// space, which is why planning needs the logical size.
    MotionAbs { x: f64, y: f64 },
    /// `NotifyPointerButton` — `code` is a Linux `BTN_*`.
    Button { code: i32, down: bool },
    /// `NotifyPointerAxis` — smooth scroll, positive down/right.
    Axis { dx: f64, dy: f64 },
    /// `NotifyPointerAxisDiscrete` — `axis` 0 = vertical, 1 = horizontal.
    AxisDiscrete { axis: u32, steps: i32 },
    /// `NotifyKeyboardKeycode` — evdev code.
    Keycode { code: i32, down: bool },
    /// `NotifyKeyboardKeysym`.
    Keysym { sym: i32, down: bool },
}

/// Carries the fractional wheel remainder between events, exactly as the
/// uinput backend does — without it, slow trackpad scrolling in `Line` mode
/// never accumulates to a detent and simply does nothing.
#[derive(Debug, Default)]
pub struct WheelAccum {
    rx: f64,
    ry: f64,
}

/// Map one wire event onto portal calls.
///
/// `logical` is the stream's logical size — the portal's own coordinate space
/// for `NotifyPointerMotionAbsolute`, NOT the pixel size (they differ under a
/// HiDPI scale factor). The wire's normalised 0..1 makes the conversion a
/// single multiply.
pub fn plan(msg: &InputMsg, logical: (f64, f64), wheel: &mut WheelAccum) -> Vec<PortalCall> {
    let to_abs = |x: f32, y: f32| PortalCall::MotionAbs {
        x: f64::from(x.clamp(0.0, 1.0)) * logical.0,
        y: f64::from(y.clamp(0.0, 1.0)) * logical.1,
    };
    match msg {
        InputMsg::MouseMove { x, y, .. } => vec![to_abs(*x, *y)],
        InputMsg::MouseButton {
            btn, down, x, y, ..
        } => {
            // Position first, then the button — same rule as the uinput
            // backend: a click whose move arrives separately can land at the
            // old position if the compositor samples between the two.
            vec![
                to_abs(*x, *y),
                PortalCall::Button {
                    code: button_code(*btn),
                    down: *down,
                },
            ]
        }
        InputMsg::MouseWheel { dx, dy, mode } => match mode {
            // Smooth deltas pass through: the browser's pixels are already
            // libinput's "scroll distance", positive down/right on both ends.
            WheelMode::Pixel => vec![PortalCall::Axis {
                dx: f64::from(*dx),
                dy: f64::from(*dy),
            }],
            // Lines and pages become discrete detents, with the remainder
            // carried. A page is a burst of detents rather than its own axis
            // — the uinput backend's convention, kept so the two paths scroll
            // the same distance for the same wire event.
            WheelMode::Line | WheelMode::Page => {
                let per = if *mode == WheelMode::Page { 3.0 } else { 1.0 };
                wheel.rx += f64::from(*dx) * per;
                wheel.ry += f64::from(*dy) * per;
                let (cx, cy) = (wheel.rx.trunc() as i32, wheel.ry.trunc() as i32);
                wheel.rx -= f64::from(cx);
                wheel.ry -= f64::from(cy);
                let mut out = Vec::new();
                if cy != 0 {
                    out.push(PortalCall::AxisDiscrete { axis: 0, steps: cy });
                }
                if cx != 0 {
                    out.push(PortalCall::AxisDiscrete { axis: 1, steps: cx });
                }
                out
            }
        },
        InputMsg::Key { code, down, .. } => match hid_to_evdev(*code) {
            Some(k) => vec![PortalCall::Keycode {
                code: i32::from(k),
                down: *down,
            }],
            // FR-13's contract, shared with every other backend: an unmapped
            // HID usage is DROPPED, never guessed at.
            None => vec![],
        },
        InputMsg::KeyText { text } => {
            let mut out = Vec::new();
            for ch in text.chars() {
                if let Some(sym) = keysym_of(ch) {
                    let sym = sym as i32;
                    out.push(PortalCall::Keysym { sym, down: true });
                    out.push(PortalCall::Keysym { sym, down: false });
                }
            }
            out
        }
        // Same as enigo and uinput: no touch injection yet. The portal has
        // NotifyTouch*, so this is a follow-up, not a wall.
        InputMsg::Touch { .. } => vec![],
        InputMsg::Heartbeat { .. } => vec![],
    }
}

// ── the D-Bus half ──────────────────────────────────────────────────────

/// Everything needed to execute [`PortalCall`]s against a live RemoteDesktop
/// session. Built in `run_stream` once the stream is negotiated, handed to
/// the stdin pump thread.
pub struct InputContext {
    proxy: zbus::blocking::Proxy<'static>,
    session: zbus::zvariant::OwnedObjectPath,
    /// The PipeWire node whose logical space `MotionAbs` coordinates are in.
    node_id: u32,
    pub logical: (f64, f64),
}

impl InputContext {
    pub fn new(
        conn: &zbus::blocking::Connection,
        session: zbus::zvariant::OwnedObjectPath,
        node_id: u32,
        logical: (f64, f64),
    ) -> Result<Self, zbus::Error> {
        let proxy = zbus::blocking::Proxy::new(
            conn,
            super::PORTAL_BUS,
            super::PORTAL_PATH,
            super::REMOTE_DESKTOP_IFACE,
        )?;
        Ok(Self {
            proxy,
            session,
            node_id,
            logical,
        })
    }

    /// Execute one call. Errors are returned, not logged — the pump decides
    /// how loud to be, and it deliberately keeps going: one refused event
    /// must not end input for the session.
    fn execute(&self, call: PortalCall) -> Result<(), zbus::Error> {
        use std::collections::HashMap;
        let opts: HashMap<&str, zbus::zvariant::Value<'_>> = HashMap::new();
        let sess = &self.session;
        match call {
            PortalCall::MotionAbs { x, y } => self
                .proxy
                .call_method(
                    "NotifyPointerMotionAbsolute",
                    &(sess, opts, self.node_id, x, y),
                )
                .map(drop),
            PortalCall::Button { code, down } => self
                .proxy
                .call_method("NotifyPointerButton", &(sess, opts, code, u32::from(down)))
                .map(drop),
            PortalCall::Axis { dx, dy } => self
                .proxy
                .call_method("NotifyPointerAxis", &(sess, opts, dx, dy))
                .map(drop),
            PortalCall::AxisDiscrete { axis, steps } => self
                .proxy
                .call_method("NotifyPointerAxisDiscrete", &(sess, opts, axis, steps))
                .map(drop),
            PortalCall::Keycode { code, down } => self
                .proxy
                .call_method(
                    "NotifyKeyboardKeycode",
                    &(sess, opts, code, u32::from(down)),
                )
                .map(drop),
            PortalCall::Keysym { sym, down } => self
                .proxy
                .call_method("NotifyKeyboardKeysym", &(sess, opts, sym, u32::from(down)))
                .map(drop),
        }
    }
}

/// The helper's stdin pump: one `InputMsg` JSON line per event, EOF when the
/// daemon is done with us. Runs on its own thread for the life of the stream.
///
/// A line that does not parse is SKIPPED, not fatal: normally both ends are
/// the same binary, but an update can leave a stale helper on disk for one
/// session, and a wire variant it does not know must cost that event only.
pub fn run_pump(ctx: InputContext, stdin: std::io::Stdin) {
    use std::io::BufRead;
    let mut wheel = WheelAccum::default();
    let mut dropped_parse = 0u64;
    let mut failed_calls = 0u64;
    let mut executed = 0u64;
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let msg: InputMsg = match serde_json::from_str(trimmed) {
            Ok(m) => m,
            Err(e) => {
                dropped_parse += 1;
                if dropped_parse <= 3 {
                    eprintln!("portal-helper: unparseable input line dropped: {e}");
                }
                continue;
            }
        };
        for call in plan(&msg, ctx.logical, &mut wheel) {
            match ctx.execute(call) {
                Ok(()) => executed += 1,
                Err(e) => {
                    failed_calls += 1;
                    // The first few loudly — a systematically refused call
                    // (revoked session, missing device grant) should be
                    // diagnosable from the daemon log, not invisible.
                    if failed_calls <= 3 {
                        eprintln!("portal-helper: input call failed: {e}");
                    }
                }
            }
        }
    }
    eprintln!(
        "portal-helper: input pump ended (executed={executed} failed={failed_calls} \
         unparseable={dropped_parse})"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOGICAL: (f64, f64) = (1920.0, 1080.0);

    fn plan1(msg: InputMsg) -> Vec<PortalCall> {
        plan(&msg, LOGICAL, &mut WheelAccum::default())
    }

    /// Normalised wire coordinates land in the stream's LOGICAL space — the
    /// single multiply this backend's coordinate story rests on.
    #[test]
    fn moves_scale_into_logical_space() {
        let calls = plan1(InputMsg::MouseMove {
            x: 0.5,
            y: 0.25,
            mon: 0,
        });
        assert_eq!(calls, vec![PortalCall::MotionAbs { x: 960.0, y: 270.0 }]);
    }

    /// Out-of-range coordinates clamp rather than leaving the stream — a
    /// negative or >1 value would ask the compositor to move outside the
    /// surface it granted.
    #[test]
    fn moves_clamp_to_the_stream() {
        let calls = plan1(InputMsg::MouseMove {
            x: -0.5,
            y: 1.5,
            mon: 0,
        });
        assert_eq!(calls, vec![PortalCall::MotionAbs { x: 0.0, y: 1080.0 }]);
    }

    /// Clicks position first, then press — same rule as uinput, same reason.
    #[test]
    fn clicks_position_then_press() {
        let calls = plan1(InputMsg::MouseButton {
            btn: Button::Right,
            down: true,
            x: 1.0,
            y: 0.0,
            mon: 0,
        });
        assert_eq!(
            calls,
            vec![
                PortalCall::MotionAbs { x: 1920.0, y: 0.0 },
                PortalCall::Button {
                    code: 0x111,
                    down: true
                },
            ]
        );
    }

    /// Pixel wheel passes through as smooth axis — positive stays positive.
    /// ⚠️ The sign is the libinput convention (positive = down), NOT evdev's
    /// REL_WHEEL (positive = up). Copying the uinput inversion here scrolls
    /// backwards; this test is what keeps that from "looking obviously right"
    /// to a future editor.
    #[test]
    fn pixel_wheel_is_smooth_and_uninverted() {
        let calls = plan1(InputMsg::MouseWheel {
            dx: 0.0,
            dy: 106.0,
            mode: WheelMode::Pixel,
        });
        assert_eq!(calls, vec![PortalCall::Axis { dx: 0.0, dy: 106.0 }]);
    }

    /// Line mode accumulates fractional detents across events — three thirds
    /// make a step, not zero steps three times.
    #[test]
    fn line_wheel_accumulates_detents() {
        let mut wheel = WheelAccum::default();
        let m = InputMsg::MouseWheel {
            dx: 0.0,
            dy: 0.4,
            mode: WheelMode::Line,
        };
        assert!(plan(&m, LOGICAL, &mut wheel).is_empty());
        assert!(plan(&m, LOGICAL, &mut wheel).is_empty());
        assert_eq!(
            plan(&m, LOGICAL, &mut wheel),
            vec![PortalCall::AxisDiscrete { axis: 0, steps: 1 }]
        );
    }

    /// A page is a burst of detents on the SAME axis, mirroring uinput.
    #[test]
    fn page_wheel_is_three_detents() {
        let calls = plan1(InputMsg::MouseWheel {
            dx: 0.0,
            dy: 1.0,
            mode: WheelMode::Page,
        });
        assert_eq!(calls, vec![PortalCall::AxisDiscrete { axis: 0, steps: 3 }]);
    }

    /// Keys ride the shared HID→evdev table; an unmapped usage is dropped,
    /// never guessed (FR-13's contract, kept across a third backend).
    #[test]
    fn keys_are_evdev_codes_and_unmapped_drops() {
        let calls = plan1(InputMsg::Key {
            code: 0x04,
            down: true,
            mods: 0,
        });
        assert_eq!(
            calls,
            vec![PortalCall::Keycode {
                code: 30,
                down: true
            }]
        );
        assert!(
            plan1(InputMsg::Key {
                code: 0xffff,
                down: true,
                mods: 0
            })
            .is_empty()
        );
    }

    /// The keysym rules: Latin-1 is identity, Unicode rides the escape plane,
    /// Return and Tab are the two honest control characters. This is what
    /// makes KeyText layout-proof — the exact property the uinput backend
    /// cannot offer (its table is physical keys).
    #[test]
    fn keysyms_follow_the_x11_unicode_rules() {
        assert_eq!(keysym_of('a'), Some(0x61));
        assert_eq!(keysym_of('é'), Some(0xE9), "Latin-1 is the identity zone");
        assert_eq!(keysym_of('€'), Some(0x0100_20AC), "Unicode escape plane");
        assert_eq!(keysym_of('\n'), Some(0xFF0D));
        assert_eq!(keysym_of('\t'), Some(0xFF09));
        assert_eq!(keysym_of('\u{7}'), None, "other C0 controls are dropped");
    }

    /// Typed text becomes press+release pairs, in order.
    #[test]
    fn key_text_is_press_release_pairs() {
        let calls = plan1(InputMsg::KeyText { text: "hé".into() });
        assert_eq!(
            calls,
            vec![
                PortalCall::Keysym {
                    sym: 0x68,
                    down: true
                },
                PortalCall::Keysym {
                    sym: 0x68,
                    down: false
                },
                PortalCall::Keysym {
                    sym: 0xE9,
                    down: true
                },
                PortalCall::Keysym {
                    sym: 0xE9,
                    down: false
                },
            ]
        );
    }
}
