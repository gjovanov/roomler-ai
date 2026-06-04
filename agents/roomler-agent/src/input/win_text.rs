//! Windows real-virtual-key text injection (rc.122).
//!
//! The browser sends each typed printable character as a `KeyText` message.
//! The previous path called `enigo.text()`, which on Windows injects every
//! character via `KEYEVENTF_UNICODE` (VK_PACKET). The **legacy Windows console
//! host** (Windows PowerShell 5.1 / cmd.exe in conhost) silently DROPS
//! VK_PACKET-injected characters — field-confirmed on REGAL-112500982: typed
//! letters never appeared in an elevated *Windows PowerShell*, but DID appear
//! in `pwsh` 7 / Windows Terminal (which accept VK_PACKET), while Enter /
//! Backspace (real virtual keys) worked everywhere. (rc.120 already proved this
//! is NOT a UIPI integrity block: worker = System 0x4000 > foreground
//! powershell.exe High 0x3000.)
//!
//! This module injects each character as a **real virtual key + scancode**
//! (`KEYEVENTF_SCANCODE`), resolved through the foreground window's active
//! keyboard layout via `VkKeyScanExW`, pressing Shift/Ctrl/Alt exactly as the
//! layout requires. Real key events are accepted by the legacy console.
//! Characters not present in the active layout (Latin under a Cyrillic-only
//! layout, emoji, CJK) fall back to `KEYEVENTF_UNICODE` — identical to the old
//! behaviour, so those cases are no worse than before (and still work in modern
//! terminals / GUI apps).
//!
//! ## Why not enigo
//!
//! enigo's `text()` is VK_PACKET-only. enigo's `key(Key::Unicode(c))` calls
//! `VkKeyScanExW` but keeps only the VK low byte and NEVER presses Shift
//! (keycodes.rs:1073 `VIRTUAL_KEY(vk as u16)`), so capitals and shifted symbols
//! ('A', '(', '!') would mis-type. Neither is usable, hence this hand-rolled path.
//!
//! ## Kill switch
//!
//! `ROOMLER_AGENT_UNICODE_TEXT=1` reverts to the old `enigo.text()`
//! (KEYEVENTF_UNICODE) path without a redeploy, in case the real-VK path
//! regresses on some host.

#![cfg(all(target_os = "windows", feature = "enigo-input"))]

use windows_sys::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, GetKeyboardLayout, HKL, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT,
    KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, KEYEVENTF_UNICODE, MAPVK_VK_TO_VSC, MapVirtualKeyExW,
    SendInput, VK_CAPITAL, VK_CONTROL, VK_MENU, VK_RETURN, VK_SHIFT, VK_TAB, VkKeyScanExW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

/// Whether the operator has forced the old KEYEVENTF_UNICODE-only path.
pub(super) fn unicode_only() -> bool {
    matches!(
        std::env::var("ROOMLER_AGENT_UNICODE_TEXT")
            .ok()
            .map(|s| s.trim().to_ascii_lowercase()),
        Some(v) if v == "1" || v == "true" || v == "yes" || v == "on"
    )
}

/// Build one keyboard `INPUT` record.
fn kbd(vk: u16, scan: u16, flags: u32) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn send(inputs: &[INPUT]) {
    if inputs.is_empty() {
        return;
    }
    // SAFETY: `inputs` is a valid contiguous slice of INPUT; cbSize is the
    // element size. SendInput copies the records; no aliasing concerns.
    unsafe {
        SendInput(
            inputs.len() as u32,
            inputs.as_ptr(),
            std::mem::size_of::<INPUT>() as i32,
        );
    }
}

/// Decompose a `VkKeyScanExW` result into `(vk, shift, ctrl, alt)`, or `None`
/// when the character isn't reachable on the layout (`-1`) or maps to no key.
/// Pulled out as a pure fn so the bit math is unit-testable without the OS.
fn decode_vk_scan(res: i16) -> Option<(u16, bool, bool, bool)> {
    if res == -1 {
        return None;
    }
    let vk = (res & 0x00ff) as u16;
    if vk == 0 || vk == 0x00ff {
        return None;
    }
    let shift_state = ((res >> 8) & 0xff) as u8;
    Some((
        vk,
        shift_state & 0x01 != 0,
        shift_state & 0x02 != 0,
        shift_state & 0x04 != 0,
    ))
}

/// Tap a real virtual key (down+up) with the required modifier presses, using
/// the scancode so the legacy console accepts it. `hkl` is the active layout.
fn tap_vk(vk: u16, shift: bool, ctrl: bool, alt: bool, hkl: HKL) {
    // SAFETY: MapVirtualKeyExW with a valid VK + layout handle; returns 0 when
    // there's no scancode mapping, which we handle below.
    let scan = unsafe { MapVirtualKeyExW(vk as u32, MAPVK_VK_TO_VSC, hkl) } as u16;
    let mut inputs: Vec<INPUT> = Vec::with_capacity(8);
    if ctrl {
        inputs.push(kbd(VK_CONTROL, 0, 0));
    }
    if alt {
        inputs.push(kbd(VK_MENU, 0, 0));
    }
    if shift {
        inputs.push(kbd(VK_SHIFT, 0, 0));
    }
    if scan != 0 {
        inputs.push(kbd(0, scan, KEYEVENTF_SCANCODE));
        inputs.push(kbd(0, scan, KEYEVENTF_SCANCODE | KEYEVENTF_KEYUP));
    } else {
        // No scancode mapping — fall back to a plain virtual-key tap.
        inputs.push(kbd(vk, 0, 0));
        inputs.push(kbd(vk, 0, KEYEVENTF_KEYUP));
    }
    if shift {
        inputs.push(kbd(VK_SHIFT, 0, KEYEVENTF_KEYUP));
    }
    if alt {
        inputs.push(kbd(VK_MENU, 0, KEYEVENTF_KEYUP));
    }
    if ctrl {
        inputs.push(kbd(VK_CONTROL, 0, KEYEVENTF_KEYUP));
    }
    send(&inputs);
}

/// Inject a single character via `KEYEVENTF_UNICODE` (VK_PACKET). Layout-
/// independent but dropped by the legacy console — the last-resort fallback.
fn send_unicode(c: char) {
    let mut buf = [0u16; 2];
    let units = c.encode_utf16(&mut buf);
    let mut inputs: Vec<INPUT> = Vec::with_capacity(units.len() * 2);
    for &u in units.iter() {
        inputs.push(kbd(0, u, KEYEVENTF_UNICODE));
        inputs.push(kbd(0, u, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP));
    }
    send(&inputs);
}

/// Read the target's CapsLock toggle state. `GetKeyState`'s toggle bit is
/// per-thread-input-queue, and the SYSTEM-context worker doesn't pump messages,
/// so we briefly `AttachThreadInput` to the foreground thread to share its key
/// state for an accurate read. Best-effort: a failed attach falls back to the
/// worker's own state (CapsLock treated as off → no compensation).
fn capslock_on(fg_tid: u32) -> bool {
    // SAFETY: Attach/Detach are paired; GetKeyState reads thread-queue state.
    unsafe {
        let our_tid = GetCurrentThreadId();
        let attach = fg_tid != 0 && fg_tid != our_tid;
        if attach {
            AttachThreadInput(our_tid, fg_tid, 1);
        }
        let on = (GetKeyState(VK_CAPITAL as i32) & 0x0001) != 0;
        if attach {
            AttachThreadInput(our_tid, fg_tid, 0);
        }
        on
    }
}

/// Type `text` into the foreground window. Per character: real VK+scancode when
/// the active layout can produce it (legacy-console-compatible), else Unicode.
pub(super) fn type_text(text: &str) {
    // The active layout is the FOREGROUND thread's — that's what interprets the
    // injected scancodes. Read it (and the thread id) once per call.
    // SAFETY: GetForegroundWindow may return null (no foreground); GetKeyboard-
    // Layout(0) then returns the calling thread's layout, a safe default.
    let (hkl, fg_tid): (HKL, u32) = unsafe {
        let tid = GetWindowThreadProcessId(GetForegroundWindow(), std::ptr::null_mut());
        (GetKeyboardLayout(tid), tid)
    };
    // rc.123 — scancode injection is subject to the TARGET's CapsLock (unlike the
    // old KEYEVENTF_UNICODE path, which ignored it). REGAL-112500982 had CapsLock
    // toggled ON → every injected letter came out with inverted case. VkKeyScanExW
    // computes the shift state assuming CapsLock OFF, so when it's ON we flip the
    // shift bit for ALPHABETIC chars (CapsLock only affects letters). Non-letters
    // and the Unicode fallback are unaffected. Hosts with CapsLock off (e.g.
    // PC50045) read `false` here → no change.
    let caps = capslock_on(fg_tid);
    for c in text.chars() {
        match c {
            '\n' | '\r' => tap_vk(VK_RETURN, false, false, false, hkl),
            '\t' => tap_vk(VK_TAB, false, false, false, hkl),
            '\0' => {}
            _ => {
                let mut buf = [0u16; 2];
                let units = c.encode_utf16(&mut buf);
                let decoded = if units.len() == 1 {
                    // SAFETY: single UTF-16 unit + valid layout handle.
                    decode_vk_scan(unsafe { VkKeyScanExW(units[0], hkl) })
                } else {
                    None // astral (emoji) — never a single VK
                };
                match decoded {
                    Some((vk, shift, ctrl, alt)) => {
                        let shift = if caps && c.is_alphabetic() {
                            !shift
                        } else {
                            shift
                        };
                        tap_vk(vk, shift, ctrl, alt, hkl);
                    }
                    None => send_unicode(c),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_unreachable_is_none() {
        assert!(decode_vk_scan(-1).is_none());
    }

    #[test]
    fn decode_plain_letter() {
        // Vk 0x41 (VK_A), no modifiers.
        assert_eq!(decode_vk_scan(0x0041), Some((0x41, false, false, false)));
    }

    #[test]
    fn decode_shifted_symbol() {
        // VkKeyScan for '(' on US = VK_9 (0x39) + shift (high byte 0x01) = 0x0139.
        assert_eq!(decode_vk_scan(0x0139), Some((0x39, true, false, false)));
    }

    #[test]
    fn decode_altgr_combo() {
        // Ctrl+Alt (AltGr) state in the high byte (0x06) over VK_Q (0x51).
        assert_eq!(decode_vk_scan(0x0651), Some((0x51, false, true, true)));
    }

    #[test]
    fn decode_zero_vk_is_none() {
        assert!(decode_vk_scan(0x0100).is_none()); // shift set but VK == 0
    }
}
