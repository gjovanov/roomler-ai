//! Windows DPI awareness.
//!
//! The agent captures pixels via WGC / DXGI / scrap (always physical-
//! pixel surfaces) and injects mouse positions via `enigo` (which uses
//! `SetCursorPos` / `SendInput` on Windows). Both APIs are sensitive
//! to the calling process's DPI awareness mode:
//!
//!   - In **DPI-unaware** or **system-DPI-aware** mode, `enigo.
//!     main_display()` returns *logical* pixels (e.g. 1536×960 on a
//!     1920×1200 panel at 125% scale) and `SetCursorPos` interprets
//!     coordinates as logical too — but capture frames are still
//!     physical 1920×1200. Multiplying a normalised [0,1] mouse
//!     position by the logical width drops the cursor at logical
//!     coordinates that map (via the OS DPI virtualisation) to
//!     physical position **left + above** of where the user clicked
//!     in the captured frame. This was the field bug reported on
//!     PC50045 (1920×1200) on 2026-05-01.
//!
//!   - In **per-monitor-aware-V2** mode (Win10 1703+), `main_display()`
//!     returns physical pixels and `SetCursorPos` interprets
//!     coordinates as physical — matching the capture surface.
//!
//! [`set_per_monitor_aware`] sets the process to per-monitor-V2 mode.
//! Idempotent: a second call is a no-op (the OS rejects with E_ACCESSDENIED
//! once a mode is set; we tolerate that). Call early — before any GUI
//! subsystem touches DPI, before `Enigo::new`, and before opening the
//! capture pipeline. `main()` calls this on Windows; the unit-test
//! environment + integration tests do not.

#![cfg(target_os = "windows")]

use windows_sys::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};

/// Try to enable per-monitor-V2 DPI awareness for the current process.
/// Returns `true` if we set it; `false` if the OS refused (typically
/// because the mode is already set — which is fine).
pub fn set_per_monitor_aware() -> bool {
    // SAFETY: SetProcessDpiAwarenessContext takes a static-lifetime
    // pointer to a sentinel constant from windows-sys; it doesn't
    // dereference Rust-owned memory. Thread-safe per MSDN.
    let ok = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    ok != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test only — under cargo test the call may succeed (first
    /// call in this process) or refuse (test runner already initialised
    /// DPI in some other test). Either is acceptable; we just want to
    /// confirm the FFI call doesn't crash.
    #[test]
    fn set_per_monitor_aware_does_not_panic() {
        let _ = set_per_monitor_aware();
    }
}
