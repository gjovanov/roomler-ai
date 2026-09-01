// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-36 P4 — input injection through `/dev/uinput`, below the compositor.
//!
//! The companion to `capture::drm_backend`. Capture without input is a
//! read-only session, and the X11 injector cannot help here: `XTest` reaches
//! Xwayland clients only, so on a native Wayland desktop it silently does
//! nothing for everything else — and at the login greeter there is no X server
//! of ours to talk to at all.
//!
//! `uinput` sidesteps that the same way DRM capture does: it creates a virtual
//! **kernel input device**, so events enter through evdev underneath the
//! display server. libinput picks it up like any USB keyboard or tablet, which
//! means one code path for GNOME, KDE, XFCE, X11 and the greeter.
//!
//! ## Design notes worth knowing before editing
//!
//! - **Absolute pointer, not relative.** The wire carries normalised `0..1`
//!   coordinates precisely so the agent's resolution can change mid-session.
//!   A relative device would force us to track the pointer and guess at
//!   acceleration; an absolute one (`ABS_X`/`ABS_Y` over a fixed 0..=32767
//!   range, the convention for tablets) lands exactly where asked, and the
//!   compositor maps that onto whatever the screen currently is.
//! - **HID usage → Linux keycode is an explicit table.** It cannot be
//!   arithmetic: HID orders letters alphabetically (`0x04` = A) while Linux
//!   keycodes follow the physical QWERTY row order (`KEY_Q` = 16, `KEY_A` = 30).
//!   The table mirrors the kernel's own `hid_keyboard[]`.
//! - ⚠️ **The ioctl numbers are computed, and the computation is tested.**
//!   `_IOW` encoding is easy to get subtly wrong and a wrong number is not a
//!   compile error — it is a runtime `EINVAL` on a device that then silently
//!   does nothing. [`ioc`] is checked against published constants in the tests.
//! - ⚠️ **A newly created uinput device is not immediately usable.** udev and
//!   libinput enumerate it asynchronously; events written in the first few tens
//!   of milliseconds are delivered to nobody. We settle after `UI_DEV_CREATE`,
//!   which is why the first click after a session opens is not lost.

use anyhow::{Result, anyhow, bail};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::time::Duration;
use tracing::{info, warn};

use super::hid_evdev::{KEYMAP, hid_to_evdev};
use super::{InputInjector, InputMsg, WheelMode, button_code};

const UINPUT_PATH: &str = "/dev/uinput";

// --- evdev event types / codes (linux/input-event-codes.h) ---
const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;
const EV_ABS: u16 = 0x03;
const SYN_REPORT: u16 = 0;
const REL_WHEEL: u16 = 0x08;
const REL_HWHEEL: u16 = 0x06;
const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
const BTN_LEFT: u16 = 0x110;
const BTN_RIGHT: u16 = 0x111;
const BTN_MIDDLE: u16 = 0x112;
const BTN_SIDE: u16 = 0x113;
const BTN_EXTRA: u16 = 0x114;
/// `KEY_LEFTSHIFT` — needed directly by the KeyText path, which presses shift
/// around a character rather than going through the HID table for it.
const KEY_LEFTSHIFT: u16 = 42;

/// Absolute axis range. 0..=32767 is the conventional span for an absolute
/// pointing device; the compositor scales it to the current screen, so this
/// stays correct across a resolution change without any resync.
const ABS_MAX: i32 = 32767;

/// Pixels of wheel delta per detent when the controller reports `Pixel` mode.
/// Browsers report roughly this per notch; evdev `REL_WHEEL` counts detents,
/// so a fractional remainder is carried rather than dropped — otherwise slow
/// trackpad scrolling would never accumulate to a click and simply do nothing.
const WHEEL_PIXELS_PER_DETENT: f32 = 53.0;

/// How long to let udev/libinput enumerate the new device before trusting it
/// to deliver events. Without this the first events after open go nowhere.
const SETTLE: Duration = Duration::from_millis(300);

// --- ioctl plumbing ---

/// The kernel's `_IOC` encoding: `dir << 30 | size << 16 | type << 8 | nr`.
///
/// Hand-rolled rather than pulled from a crate because the surface is four
/// constants, and because a wrong value fails at RUNTIME with `EINVAL` on a
/// device that then silently injects nothing — so it is worth having under a
/// test that checks it against the published numbers.
const fn ioc(dir: u32, ty: u32, nr: u32, size: u32) -> libc::c_ulong {
    ((dir << 30) | (size << 16) | (ty << 8) | nr) as libc::c_ulong
}
const IOC_NONE: u32 = 0;
const IOC_WRITE: u32 = 1;
const UINPUT_IOCTL_BASE: u32 = b'U' as u32;

fn ui_dev_create() -> libc::c_ulong {
    ioc(IOC_NONE, UINPUT_IOCTL_BASE, 1, 0)
}
fn ui_dev_destroy() -> libc::c_ulong {
    ioc(IOC_NONE, UINPUT_IOCTL_BASE, 2, 0)
}
fn ui_dev_setup() -> libc::c_ulong {
    ioc(
        IOC_WRITE,
        UINPUT_IOCTL_BASE,
        3,
        std::mem::size_of::<UinputSetup>() as u32,
    )
}
fn ui_abs_setup() -> libc::c_ulong {
    ioc(
        IOC_WRITE,
        UINPUT_IOCTL_BASE,
        4,
        std::mem::size_of::<UinputAbsSetup>() as u32,
    )
}
fn ui_set_evbit() -> libc::c_ulong {
    ioc(IOC_WRITE, UINPUT_IOCTL_BASE, 100, 4)
}
fn ui_set_keybit() -> libc::c_ulong {
    ioc(IOC_WRITE, UINPUT_IOCTL_BASE, 101, 4)
}
fn ui_set_relbit() -> libc::c_ulong {
    ioc(IOC_WRITE, UINPUT_IOCTL_BASE, 102, 4)
}
fn ui_set_absbit() -> libc::c_ulong {
    ioc(IOC_WRITE, UINPUT_IOCTL_BASE, 103, 4)
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct InputId {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
}

#[repr(C)]
struct UinputSetup {
    id: InputId,
    name: [u8; 80],
    ff_effects_max: u32,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct InputAbsinfo {
    value: i32,
    minimum: i32,
    maximum: i32,
    fuzz: i32,
    flat: i32,
    resolution: i32,
}

#[repr(C)]
struct UinputAbsSetup {
    code: u16,
    absinfo: InputAbsinfo,
}

#[repr(C)]
struct InputEvent {
    time: libc::timeval,
    type_: u16,
    code: u16,
    value: i32,
}

/// Runtime gate. **Default OFF**, matching `drm_backend`: a virtual input
/// device is host-global and visible to every application, so a host opts in
/// (`ROOMLERD_UINPUT=1`) rather than having one appear because it upgraded.
/// Pair it with `ROOMLERD_DRM_CAPTURE=1` on a Wayland host.
pub fn env_enabled() -> bool {
    tunnel_core::env::flag("UINPUT", false)
}

/// Is `/dev/uinput` present and writable by this process?
pub fn available() -> bool {
    OpenOptions::new().write(true).open(UINPUT_PATH).is_ok()
}

pub struct UinputInjector {
    dev: File,
    /// Carried wheel remainder, so sub-detent scrolling accumulates instead of
    /// being truncated to nothing on every event.
    wheel_rem: (f32, f32),
    /// Rate-limit for the "this event kind is not implemented" notice, so an
    /// unsupported stream cannot flood the daemon log.
    warned_text: bool,
    warned_touch: bool,
    /// FR-36 P4b — the verified keyboard layout for this host, if we have a
    /// table for it, plus the raw name that was detected. The name is kept even
    /// when unsupported so the refusal can SAY what it found: "unsupported
    /// layout" is not actionable, "unsupported layout `de`" is.
    layout: Option<super::keylayout::Layout>,
    layout_name: String,
}

impl UinputInjector {
    pub fn new() -> Result<Self> {
        let dev = OpenOptions::new()
            .write(true)
            .open(UINPUT_PATH)
            .map_err(|e| anyhow!("open {UINPUT_PATH}: {e} (needs root or the `input` group)"))?;
        let fd = dev.as_raw_fd();

        // SAFETY: every ioctl below is a documented uinput setup call on a fd
        // we just opened; the argument is a plain integer or a #[repr(C)]
        // struct matching the kernel's, and each return is checked.
        unsafe {
            for ev in [EV_KEY, EV_REL, EV_ABS, EV_SYN] {
                ck(
                    libc::ioctl(fd, ui_set_evbit(), ev as libc::c_int),
                    "SET_EVBIT",
                )?;
            }
            // Every key this backend can ever emit must be declared up front —
            // an undeclared code is dropped by the kernel, silently.
            for code in KEYMAP.iter().map(|&(_, k)| k) {
                ck(
                    libc::ioctl(fd, ui_set_keybit(), code as libc::c_int),
                    "SET_KEYBIT",
                )?;
            }
            for btn in [BTN_LEFT, BTN_RIGHT, BTN_MIDDLE, BTN_SIDE, BTN_EXTRA] {
                ck(
                    libc::ioctl(fd, ui_set_keybit(), btn as libc::c_int),
                    "SET_KEYBIT btn",
                )?;
            }
            for rel in [REL_WHEEL, REL_HWHEEL] {
                ck(
                    libc::ioctl(fd, ui_set_relbit(), rel as libc::c_int),
                    "SET_RELBIT",
                )?;
            }
            for abs in [ABS_X, ABS_Y] {
                ck(
                    libc::ioctl(fd, ui_set_absbit(), abs as libc::c_int),
                    "SET_ABSBIT",
                )?;
                let setup = UinputAbsSetup {
                    code: abs,
                    absinfo: InputAbsinfo {
                        minimum: 0,
                        maximum: ABS_MAX,
                        ..Default::default()
                    },
                };
                ck(libc::ioctl(fd, ui_abs_setup(), &setup), "ABS_SETUP")?;
            }

            let mut name = [0u8; 80];
            let label = b"Roomler Virtual Input";
            name[..label.len()].copy_from_slice(label);
            let setup = UinputSetup {
                id: InputId {
                    bustype: 0x03, // BUS_USB — the value libinput expects to see
                    vendor: 0x1209,
                    product: 0x0001,
                    version: 1,
                },
                name,
                ff_effects_max: 0,
            };
            ck(libc::ioctl(fd, ui_dev_setup(), &setup), "DEV_SETUP")?;
            ck(libc::ioctl(fd, ui_dev_create()), "DEV_CREATE")?;
        }

        // ⚠️ Load-bearing. udev and libinput enumerate the new device
        // asynchronously; anything written before they do reaches nobody, so
        // without this the first click of a session is simply lost.
        std::thread::sleep(SETTLE);
        info!("input: backend=uinput (virtual kernel device, below the compositor)");

        let (layout, layout_name) = super::keylayout::detect();
        match layout {
            Some(l) => info!(
                layout = l.as_str(),
                detected = layout_name.as_str(),
                "uinput: typed text will be sent as physical keys for this layout"
            ),
            None => warn!(
                detected = layout_name.as_str(),
                "uinput: no verified key table for this keyboard layout — typed text will NOT \
                 arrive (pointer and non-text keys are unaffected)"
            ),
        }

        Ok(Self {
            dev,
            wheel_rem: (0.0, 0.0),
            warned_text: false,
            warned_touch: false,
            layout,
            layout_name,
        })
    }

    fn emit(&mut self, type_: u16, code: u16, value: i32) -> Result<()> {
        let ev = InputEvent {
            time: libc::timeval {
                tv_sec: 0,
                tv_usec: 0,
            },
            type_,
            code,
            value,
        };
        // SAFETY: `InputEvent` is #[repr(C)] and layout-compatible with the
        // kernel's `struct input_event`; we write exactly its size.
        let bytes = unsafe {
            std::slice::from_raw_parts(
                &ev as *const InputEvent as *const u8,
                std::mem::size_of::<InputEvent>(),
            )
        };
        self.dev
            .write_all(bytes)
            .map_err(|e| anyhow!("uinput write: {e}"))
    }

    /// Publish the batch. evdev consumers act on the SYN_REPORT, not on the
    /// individual events, so a missing sync means the input simply never
    /// happens — it does not merely arrive late.
    fn sync(&mut self) -> Result<()> {
        self.emit(EV_SYN, SYN_REPORT, 0)
    }

    fn move_abs(&mut self, x: f32, y: f32) -> Result<()> {
        let clamp = |v: f32| (v.clamp(0.0, 1.0) * ABS_MAX as f32).round() as i32;
        self.emit(EV_ABS, ABS_X, clamp(x))?;
        self.emit(EV_ABS, ABS_Y, clamp(y))
    }
}

impl Drop for UinputInjector {
    fn drop(&mut self) {
        // SAFETY: destroying the device we created, on our own fd.
        unsafe {
            libc::ioctl(self.dev.as_raw_fd(), ui_dev_destroy());
        }
    }
}

fn ck(rc: libc::c_int, what: &str) -> Result<()> {
    if rc < 0 {
        bail!("uinput {what}: {}", std::io::Error::last_os_error());
    }
    Ok(())
}

impl InputInjector for UinputInjector {
    fn inject(&mut self, event: InputMsg) -> Result<()> {
        match event {
            InputMsg::MouseMove { x, y, .. } => {
                self.move_abs(x, y)?;
                self.sync()
            }
            InputMsg::MouseButton {
                btn, down, x, y, ..
            } => {
                // Position first, then the button, in ONE report: a click that
                // syncs its move separately can land at the old position if the
                // compositor samples between the two.
                self.move_abs(x, y)?;
                self.emit(EV_KEY, button_code(btn), i32::from(down))?;
                self.sync()
            }
            InputMsg::MouseWheel { dx, dy, mode } => {
                let scale = match mode {
                    WheelMode::Pixel => WHEEL_PIXELS_PER_DETENT,
                    WheelMode::Line => 1.0,
                    // A page is a large jump; treat it as a burst of detents
                    // rather than inventing a separate axis.
                    WheelMode::Page => 1.0 / 3.0,
                };
                let (mut rx, mut ry) = self.wheel_rem;
                rx += dx / scale;
                ry += dy / scale;
                let (cx, cy) = (rx.trunc() as i32, ry.trunc() as i32);
                self.wheel_rem = (rx - cx as f32, ry - cy as f32);
                if cx == 0 && cy == 0 {
                    return Ok(());
                }
                if cx != 0 {
                    self.emit(EV_REL, REL_HWHEEL, cx)?;
                }
                if cy != 0 {
                    // Browser dy is positive DOWN; evdev REL_WHEEL is positive UP.
                    self.emit(EV_REL, REL_WHEEL, -cy)?;
                }
                self.sync()
            }
            InputMsg::Key { code, down, .. } => {
                let Some(key) = hid_to_evdev(code) else {
                    // Same contract as the enigo backend (FR-13): an unmapped
                    // HID usage is DROPPED, never guessed at.
                    return Ok(());
                };
                self.emit(EV_KEY, key, i32::from(down))?;
                self.sync()
            }
            InputMsg::KeyText { text } => {
                // FR-36 P4b. evdev carries PHYSICAL keys, so typing text means
                // pressing the key that produces each character *under this
                // host's layout*. We do that only for a layout we have a
                // verified table for; anything else refuses loudly rather than
                // typing the US mapping at a keyboard whose keys are elsewhere.
                let Some(layout) = self.layout else {
                    if !self.warned_text {
                        self.warned_text = true;
                        warn!(
                            detected = self.layout_name.as_str(),
                            "uinput: typed text dropped — no verified key table for this layout. \
                             Pointer and non-text keys still work."
                        );
                    }
                    return Ok(());
                };
                let mut skipped = 0usize;
                for ch in text.chars() {
                    let Some(k) = layout.stroke(ch) else {
                        // A character this layout cannot reach (accented, CJK,
                        // or needing a dead-key sequence). Skip it rather than
                        // substituting something wrong.
                        skipped += 1;
                        continue;
                    };
                    if k.shift {
                        self.emit(EV_KEY, KEY_LEFTSHIFT, 1)?;
                    }
                    let Some(code) = hid_to_evdev(k.hid) else {
                        skipped += 1;
                        if k.shift {
                            self.emit(EV_KEY, KEY_LEFTSHIFT, 0)?;
                        }
                        continue;
                    };
                    self.emit(EV_KEY, code, 1)?;
                    self.emit(EV_KEY, code, 0)?;
                    if k.shift {
                        self.emit(EV_KEY, KEY_LEFTSHIFT, 0)?;
                    }
                    // One report per character: a compositor coalescing an
                    // entire string into a single frame can drop repeats of the
                    // same key, which is exactly what typing "ll" looks like.
                    self.sync()?;
                }
                if skipped > 0 && !self.warned_text {
                    self.warned_text = true;
                    warn!(
                        layout = layout.as_str(),
                        skipped, "uinput: some characters are not typeable under this layout"
                    );
                }
                Ok(())
            }
            InputMsg::Touch { .. } => {
                if !self.warned_touch {
                    self.warned_touch = true;
                    warn!(
                        "uinput: touch events are not implemented (needs a multitouch ABS_MT device)"
                    );
                }
                Ok(())
            }
            InputMsg::Heartbeat { .. } => Ok(()),
        }
    }

    fn has_permission(&self) -> bool {
        true // constructing the device proved it
    }
}

// The HID usage → evdev keycode table lives in `super::hid_evdev` — FR-45 P4's
// portal input path sends the same codes through NotifyKeyboardKeycode, and the
// two consumers are behind different features.

#[cfg(test)]
mod tests {
    use super::*;

    /// A wrong ioctl number is not a compile error — it is a runtime EINVAL on
    /// a device that then silently injects nothing. Pin the encoding against
    /// the published constants.
    #[test]
    fn ioctl_numbers_match_the_published_values() {
        assert_eq!(ui_dev_create(), 0x5501);
        assert_eq!(ui_dev_destroy(), 0x5502);
        assert_eq!(ui_set_evbit(), 0x4004_5564);
        assert_eq!(ui_set_keybit(), 0x4004_5565);
        assert_eq!(ui_set_relbit(), 0x4004_5566);
        assert_eq!(ui_set_absbit(), 0x4004_5567);
    }

    /// The kernel structs are an ABI. A size change means we would hand the
    /// kernel a differently-shaped struct under the same ioctl number.
    #[test]
    fn kernel_struct_sizes_are_the_abi() {
        assert_eq!(std::mem::size_of::<InputId>(), 8);
        assert_eq!(std::mem::size_of::<UinputSetup>(), 92);
        assert_eq!(std::mem::size_of::<InputAbsinfo>(), 24);
        assert_eq!(std::mem::size_of::<UinputAbsSetup>(), 28);
        // timeval (16 on 64-bit) + u16 + u16 + i32
        assert_eq!(std::mem::size_of::<InputEvent>(), 24);
    }

    // The HID→evdev table tests moved to `super::super::hid_evdev` with the
    // table itself.
}
