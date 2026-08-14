//! macOS TCC (privacy-permission) probes.
//!
//! macOS never *errors* when a permission is missing — CGDisplayStream
//! happily delivers wallpaper-only frames without Screen Recording, and
//! CGEventPost silently swallows events without Accessibility. Both read
//! as product bugs ("black screen", "input does nothing") with nothing in
//! the logs. These probes exist so the agent SAYS what the OS will not.
//!
//! The grants are keyed on the binary's code signature (an ad-hoc cdhash
//! for the unsigned builds), so a binary update can invalidate them — on
//! the next start these fire again and name the fix.

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGRequestScreenCaptureAccess() -> bool;
}

// AXIsProcessTrusted returns a Carbon `Boolean` (unsigned char), not a
// C99 `_Bool` — declare it as u8, or the ABI is technically wrong.
#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> u8;
}

/// Is Screen Recording granted to this process?
pub fn screen_recording_granted() -> bool {
    unsafe { CGPreflightScreenCaptureAccess() }
}

/// Pop the one-time system prompt. Also registers the app in the Screen
/// Recording pane so the operator only has to flip a toggle instead of
/// hunting for a "+" button. Returns the current grant state — the user's
/// answer lands asynchronously, after a restart.
pub fn request_screen_recording() -> bool {
    unsafe { CGRequestScreenCaptureAccess() }
}

/// Is this process trusted for Accessibility (input injection)?
pub fn accessibility_trusted() -> bool {
    unsafe { AXIsProcessTrusted() != 0 }
}
