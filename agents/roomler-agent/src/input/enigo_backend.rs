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
    Axis, Button as EnigoButton, Coordinate, Direction, Enigo, Key, Keyboard, Mouse, Settings,
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

/// Dispatch one [`InputMsg`] through enigo. Public to sibling backends
/// in the `input/` module (specifically [`super::system_context_backend`])
/// so the M3 A1 SYSTEM-context worker can reuse the exact same HID-to-
/// VK mapping + wheel/mouse semantics behind a different per-event
/// preamble (`SetThreadDesktop` rebind). Not exposed outside `input/`.
///
/// Compiled only when both `system-context` and `enigo-input` features
/// are on AND the target is Windows — i.e. the only configuration
/// where `system_context_backend` itself compiles.
#[cfg(all(
    feature = "system-context",
    target_os = "windows",
    feature = "enigo-input"
))]
pub(super) fn dispatch_for_external(enigo: &mut Enigo, msg: InputMsg) -> Result<()> {
    dispatch(enigo, msg)
}

fn dispatch(enigo: &mut Enigo, msg: InputMsg) -> Result<()> {
    match msg {
        InputMsg::MouseMove { x, y, mon } => {
            let (px, py) = to_pixels(enigo, x, y, mon);
            enigo
                .move_mouse(px, py, Coordinate::Abs)
                .map_err(|e| anyhow!("move_mouse: {e}"))?;
        }
        InputMsg::MouseButton {
            btn,
            down,
            x,
            y,
            mon,
        } => {
            // Move first so the click hits the intended target even if
            // earlier MouseMove events were coalesced away.
            let (px, py) = to_pixels(enigo, x, y, mon);
            enigo
                .move_mouse(px, py, Coordinate::Abs)
                .map_err(|e| anyhow!("move_mouse: {e}"))?;
            let direction = if down {
                Direction::Press
            } else {
                Direction::Release
            };
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
        InputMsg::Key {
            code,
            down,
            mods: _,
        } => {
            let direction = if down {
                Direction::Press
            } else {
                Direction::Release
            };
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

/// Whether the virtual-screen-aware `to_pixels` path is enabled.
///
/// Opt-in via `ROOMLER_AGENT_VIRTUAL_SCREEN=1` (also accepts `true`,
/// `yes`, `on` — case-insensitive). Default off — the legacy
/// `enigo.main_display()` path is preserved verbatim.
///
/// rc.54 — addresses the "orthogonal mouse-offset" field bug surfaced
/// in `79d6dee` and instrumented by the rc.48 `monitor_diag` log. When
/// the primary monitor's virtual-screen origin is non-zero (multi-
/// monitor host where the user has dragged the primary off `(0,0)`),
/// `SetCursorPos` interprets coordinates in the virtual-desktop space,
/// not the primary-local space; without the origin offset, clicks
/// land on the wrong monitor or shifted by the offset magnitude.
///
/// Read once at process startup via [`std::sync::LazyLock`]; the
/// resulting `bool` is then cheap to read from the hot input path.
fn virtual_screen_enabled() -> bool {
    use std::sync::LazyLock;
    static FLAG: LazyLock<bool> = LazyLock::new(|| {
        super::parse_virtual_screen_flag(
            std::env::var("ROOMLER_AGENT_VIRTUAL_SCREEN")
                .ok()
                .as_deref(),
        )
    });
    *FLAG
}

/// Map a normalised `(x, y)` in `[0,1]` plus a target monitor rect
/// `(origin_x, origin_y, w, h)` (all in physical pixels on the virtual
/// desktop) to absolute virtual-screen pixel coordinates suitable for
/// `SetCursorPos` (Windows) / `XTest` (X11) / `CGEventPost` (macOS).
///
/// Out-of-range normalised values are clamped to `[0,1]`; degenerate
/// `w` / `h` ≤ 1 still produce a well-defined origin-anchored point.
pub(crate) fn map_normalised_to_virtual(
    x: f32,
    y: f32,
    origin_x: i32,
    origin_y: i32,
    w: i32,
    h: i32,
) -> (i32, i32) {
    let x_clamped = x.clamp(0.0, 1.0);
    let y_clamped = y.clamp(0.0, 1.0);
    // Clamp `(w-1)` / `(h-1)` to non-negative so a degenerate enumeration
    // (0×0 monitor on a headless host) maps to (origin_x, origin_y)
    // instead of an underflow-induced negative pixel value.
    let span_w = (w - 1).max(0) as f32;
    let span_h = (h - 1).max(0) as f32;
    let local_px = (x_clamped * span_w).round() as i32;
    let local_py = (y_clamped * span_h).round() as i32;
    (origin_x + local_px, origin_y + local_py)
}

/// Resolve the monitor we map normalised `(x, y)` against. Returns
/// `(origin_x, origin_y, width_px, height_px)`.
///
/// Path resolution (when `ROOMLER_AGENT_VIRTUAL_SCREEN=1`):
///   1. `win32_monitors::target_monitor(mon)` — looks up the cached
///      enumerate() result by browser-supplied index, falls back to
///      the primary monitor when out of range. Today's browser sends
///      `mon=0` (only primary monitor streams), so this matches the
///      `MonitorInfo` for the primary on every event. rc.55 wires
///      the per-index lookup so the agent is ready when multi-stream
///      lands.
///
/// Legacy path (default, gate off): returns `enigo.main_display()`
/// (i.e. origin (0,0) + primary dims). Math is identical to pre-rc.54.
fn resolve_target_monitor(enigo: &Enigo, mon: u8) -> (i32, i32, i32, i32) {
    if virtual_screen_enabled() {
        #[cfg(target_os = "windows")]
        {
            if let Some(m) = crate::win32_monitors::target_monitor(mon) {
                return (m.origin_x, m.origin_y, m.width_px, m.height_px);
            }
        }
    }
    // Legacy path / non-Windows / Win32 enumeration miss → enigo's view.
    // `mon` is silently dropped here — single-monitor + non-Windows
    // hosts only have one display to map against.
    let _ = mon;
    let (w, h) = enigo.main_display().unwrap_or((1920, 1080));
    (0, 0, w, h)
}

/// Normalised `(x, y)` in `[0,1]` → absolute pixel coordinates on the
/// agent's primary display. Multi-monitor mapping (`mon` > 0) picks the
/// monitor from enigo's enumeration; on single-monitor hosts it falls
/// back to primary. Out-of-range values are clamped.
///
/// rc.39 — rate-limited diagnostic logging at INFO level for the first
/// 50 dispatches per process. the field-test host / a second field-test host field test 2026-05-17
/// shows mouse positioned wrong even after rc.38's aspect-preserving
/// downscale + skip-first-frame fixes. Suspect a coord-system mismatch
/// between enigo.main_display() (returns OS-reported logical pixels,
/// possibly DPI-virtualised) and the capture surface (device pixels).
/// The first 50 events surface the actual numbers in agent logs;
/// remaining events drop to debug level to avoid spam.
///
/// rc.54 — when `ROOMLER_AGENT_VIRTUAL_SCREEN=1` is set, the new path
/// queries `win32_monitors::primary()` for the OS-designated primary
/// monitor's virtual-screen origin and applies the offset to the
/// computed pixel coords. Closes the orthogonal mouse-offset bug on
/// multi-monitor hosts where the primary monitor isn't at (0,0).
/// Default off; flip the default in a later rc once the field-test
/// host smoke confirms the path on rc.54.
fn to_pixels(enigo: &Enigo, x: f32, y: f32, mon: u8) -> (i32, i32) {
    use std::sync::atomic::Ordering;
    const DIAG_INFO_LIMIT: u32 = 50;

    let (origin_x, origin_y, w, h) = resolve_target_monitor(enigo, mon);
    let (px, py) = map_normalised_to_virtual(x, y, origin_x, origin_y, w, h);
    let count = super::INPUT_DIAG_COUNT.fetch_add(1, Ordering::Relaxed);
    let vscreen = virtual_screen_enabled();
    if count < DIAG_INFO_LIMIT {
        tracing::info!(
            norm_x = x.clamp(0.0, 1.0),
            norm_y = y.clamp(0.0, 1.0),
            display_w = w,
            display_h = h,
            origin_x,
            origin_y,
            px,
            py,
            mon,
            virtual_screen = vscreen,
            seq = count,
            "input dispatch — diagnostic (first 50 events)"
        );
    } else {
        tracing::debug!(
            norm_x = x.clamp(0.0, 1.0),
            norm_y = y.clamp(0.0, 1.0),
            display_w = w,
            display_h = h,
            origin_x,
            origin_y,
            px,
            py,
            mon,
            virtual_screen = vscreen,
            "input dispatch"
        );
    }
    (px, py)
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
/// On Windows, letters and digits route through `Key::Other(VK_*)`
/// rather than `Key::Unicode(c)`. Reason: enigo's Unicode path sets
/// `KEYEVENTF_SCANCODE` on Windows, which makes the OS rely on the
/// current keyboard layout to map scan → character. On non-US layouts
/// (German, International) this can drop or reassign control-sequence
/// characters — user reported `Ctrl+C → ©` and `Backspace → ^H` in
/// pwsh / Windows Terminal on 0.1.33. The VK path injects the virtual
/// key directly so modifiers (VK_CONTROL, VK_SHIFT, VK_MENU) combine
/// with the letter exactly as if typed on a physical keyboard,
/// regardless of layout.
///
/// Unknown codes fall back to `Key::Other(code)` in the caller.
fn hid_to_key(code: u32) -> Option<Key> {
    // HID usage codes from "Keyboard/Keypad" Page (0x07).
    match code {
        // Letters: HID 0x04..=0x1d → 'a'..='z'.
        // Windows: VK_A..VK_Z = 0x41..0x5A — inject as virtual-key so
        // Ctrl/Alt/Shift combine correctly regardless of keyboard
        // layout. Other platforms: Key::Unicode is honoured by
        // XTest / CGEventPost and combines with modifiers there.
        #[cfg(target_os = "windows")]
        0x04..=0x1d => Some(Key::Other(0x41 + (code - 0x04))),
        #[cfg(not(target_os = "windows"))]
        0x04..=0x1d => char::from_u32(u32::from(b'a') + (code - 0x04)).map(Key::Unicode),
        // Digits: HID 0x1e..=0x26 → '1'..='9', 0x27 → '0'.
        // Windows VK_0 = 0x30, VK_1..VK_9 = 0x31..0x39.
        #[cfg(target_os = "windows")]
        0x1e..=0x26 => Some(Key::Other(0x31 + (code - 0x1e))),
        #[cfg(target_os = "windows")]
        0x27 => Some(Key::Other(0x30)),
        #[cfg(not(target_os = "windows"))]
        0x1e..=0x26 => char::from_u32(u32::from(b'1') + (code - 0x1e)).map(Key::Unicode),
        #[cfg(not(target_os = "windows"))]
        0x27 => Some(Key::Unicode('0')),
        // Common punctuation (US layout HID usages). Keeping Unicode
        // here even on Windows — the scancode route is layout-dependent
        // but punctuation is less frequently combined with modifiers
        // than letters, and the VK codes for these vary dramatically
        // by layout (VK_OEM_1 etc.). Layouts other than US will be
        // addressed in a follow-up when we wire a layout-aware mapper.
        0x2d => Some(Key::Unicode('-')),
        0x2e => Some(Key::Unicode('=')),
        0x2f => Some(Key::Unicode('[')),
        0x30 => Some(Key::Unicode(']')),
        0x31 => Some(Key::Unicode('\\')),
        0x33 => Some(Key::Unicode(';')),
        0x34 => Some(Key::Unicode('\'')),
        0x35 => Some(Key::Unicode('`')),
        0x36 => Some(Key::Unicode(',')),
        0x37 => Some(Key::Unicode('.')),
        0x38 => Some(Key::Unicode('/')),
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
        // `Key::Insert` only exists on Linux + Windows builds of enigo;
        // macOS keyboards have no Insert key, so enigo omits the variant.
        // Fall through to None on macOS — the browser-side composable can
        // retry via InputMsg::KeyText for the rare caller that needs it.
        #[cfg(not(target_os = "macos"))]
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

    /// rc.54 — virtual-screen-aware mapping math. Verifies the
    /// `map_normalised_to_virtual` helper that produces the absolute
    /// pixel coordinates passed to `SetCursorPos` / equivalents.
    #[test]
    fn map_zero_zero_lands_at_origin() {
        // Origin (0, 0), single-monitor 1920×1200.
        assert_eq!(
            map_normalised_to_virtual(0.0, 0.0, 0, 0, 1920, 1200),
            (0, 0)
        );
        // Non-zero origin (multi-monitor layout): (0,0) should land at
        // the monitor origin, NOT at the virtual desktop origin.
        assert_eq!(
            map_normalised_to_virtual(0.0, 0.0, 1920, 0, 1920, 1200),
            (1920, 0)
        );
        // Negative origin (primary to right of secondary).
        assert_eq!(
            map_normalised_to_virtual(0.0, 0.0, -1920, 0, 1920, 1200),
            (-1920, 0)
        );
    }

    #[test]
    fn map_one_one_lands_at_far_corner() {
        // Single-monitor 1920×1200: (1,1) → (1919, 1199) (w-1, h-1).
        assert_eq!(
            map_normalised_to_virtual(1.0, 1.0, 0, 0, 1920, 1200),
            (1919, 1199)
        );
        // Non-zero origin: far corner is shifted by origin.
        assert_eq!(
            map_normalised_to_virtual(1.0, 1.0, 1920, 0, 1920, 1200),
            (1920 + 1919, 1199)
        );
    }

    #[test]
    fn map_centre_lands_at_centre() {
        // 1920×1200 centre at origin: (960, 600).
        assert_eq!(
            map_normalised_to_virtual(0.5, 0.5, 0, 0, 1920, 1200),
            (960, 600)
        );
        // With origin offset (1920, 0): centre shifts by origin.
        assert_eq!(
            map_normalised_to_virtual(0.5, 0.5, 1920, 0, 1920, 1200),
            (1920 + 960, 600)
        );
    }

    #[test]
    fn map_clamps_out_of_range_normalised_values() {
        // Negative norm → clamped to 0 → at origin.
        assert_eq!(
            map_normalised_to_virtual(-0.5, -0.5, 0, 0, 1920, 1200),
            (0, 0)
        );
        // > 1 → clamped to 1 → at far corner.
        assert_eq!(
            map_normalised_to_virtual(1.5, 1.5, 0, 0, 1920, 1200),
            (1919, 1199)
        );
    }

    #[test]
    fn map_handles_degenerate_dims() {
        // 0×0 monitor (e.g. enumeration anomaly on a headless host):
        // result anchors at origin without underflow.
        assert_eq!(
            map_normalised_to_virtual(0.5, 0.5, 100, 200, 0, 0),
            (100, 200)
        );
        // 1×1 (singular dim): span_w / span_h are 0 → result at origin.
        assert_eq!(
            map_normalised_to_virtual(0.5, 0.5, 100, 200, 1, 1),
            (100, 200)
        );
    }

    /// PC50045 layout regression check — primary monitor at virtual
    /// origin (1920, 0): a click at centre-screen (0.5, 0.5) must land
    /// inside the primary monitor's virtual rect, not on the secondary
    /// monitor sitting at (0, 0).
    #[test]
    fn pc50045_primary_offset_layout_lands_correct_monitor() {
        // Secondary 1920×1080 at (0, 0); primary 1920×1200 at (1920, 0).
        // We map against primary's MonitorInfo.
        let (px, py) = map_normalised_to_virtual(0.5, 0.5, 1920, 0, 1920, 1200);
        // Must be inside primary's [1920, 3840) × [0, 1200) rect.
        assert!(
            px >= 1920 && px < 3840,
            "centre x should be inside primary's x-range, got {px}"
        );
        assert!(
            py >= 0 && py < 1200,
            "centre y should be inside primary's y-range, got {py}"
        );
        // Pre-rc.54 would have produced (960, 600) — that's on the
        // SECONDARY monitor, hence the field-bug.
        assert_ne!((px, py), (960, 600));
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

    /// On Windows, letters route through Key::Other(VK_*) so Ctrl/Alt
    /// modifiers combine with them. On other platforms the same input
    /// routes through Key::Unicode which the XTest / CGEventPost
    /// backends honour natively.
    #[test]
    #[cfg(target_os = "windows")]
    fn hid_letters_map_to_virtual_keycodes_on_windows() {
        // HID 0x04 = 'a' → VK_A (0x41)
        assert!(matches!(hid_to_key(0x04), Some(Key::Other(0x41))));
        // HID 0x06 = 'c' → VK_C (0x43) — Ctrl+C path
        assert!(matches!(hid_to_key(0x06), Some(Key::Other(0x43))));
        // HID 0x1d = 'z' → VK_Z (0x5a)
        assert!(matches!(hid_to_key(0x1d), Some(Key::Other(0x5a))));
        // HID 0x1e = '1' → VK_1 (0x31)
        assert!(matches!(hid_to_key(0x1e), Some(Key::Other(0x31))));
        // HID 0x27 = '0' → VK_0 (0x30)
        assert!(matches!(hid_to_key(0x27), Some(Key::Other(0x30))));
    }

    #[test]
    #[cfg(not(target_os = "windows"))]
    fn hid_letters_map_to_unicode_on_non_windows() {
        assert!(matches!(hid_to_key(0x04), Some(Key::Unicode('a'))));
        assert!(matches!(hid_to_key(0x06), Some(Key::Unicode('c'))));
    }
}
