// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD

//! #4419 — the main window's full-screen behaviour, made explicit before
//! its first show (macOS).
//!
//! AppKit reads whether a titled window may go full screen, and be tiled,
//! from the window's `collectionBehavior`. While none of those bits is set
//! (tao sets none, and this app set none before #4419), AppKit works the
//! answer out again each time it asks, and the answer can change between two
//! passes of one display cycle. When it does, AppKit adds the full-screen
//! button to the title bar (`-[NSThemeFrame _updateButtons]`) while the same
//! cycle's vibrancy pass walks the title bar's subviews:
//! `NSViewUpdateVibrancyForSubtree` raises "Collection was mutated while
//! being enumerated", nothing catches an exception there, and the process
//! ends (`-[NSApplication _crashOnException:]`). The companion ended this way
//! on a Mac (0.4.236, macOS 15.7.7) seconds after a change to the menu bar,
//! with this window on the screen and no frame of its own code on the stack.
//! Firefox traced the same frames to the same guess and ended them the same
//! way, by setting the bits when the window is made
//! (<https://bugzilla.mozilla.org/show_bug.cgi?id=2031249>).
//!
//! The bits set are the ones Firefox's fix sets: a resizable window may go
//! full screen and be tiled, a fixed-size one neither. A choice the window
//! already holds is kept. The main window is the only titled window the
//! companion has on a Mac: its panels and the red frame are borderless, with
//! no title bar.

/// AppKit's `NSWindowCollectionBehavior` full-screen bits (`NSWindow.h`).
/// The macOS build checks each one against objc2-app-kit's.
#[cfg(any(target_os = "macos", test))]
const PRIMARY: usize = 1 << 7;
#[cfg(any(target_os = "macos", test))]
const AUXILIARY: usize = 1 << 8;
#[cfg(any(target_os = "macos", test))]
const NONE: usize = 1 << 9;
#[cfg(any(target_os = "macos", test))]
const ALLOWS_TILING: usize = 1 << 11;
#[cfg(any(target_os = "macos", test))]
const DISALLOWS_TILING: usize = 1 << 12;

#[cfg(target_os = "macos")]
const _: () = {
    use objc2_app_kit::NSWindowCollectionBehavior as B;
    assert!(PRIMARY == B::FullScreenPrimary.bits());
    assert!(AUXILIARY == B::FullScreenAuxiliary.bits());
    assert!(NONE == B::FullScreenNone.bits());
    assert!(ALLOWS_TILING == B::FullScreenAllowsTiling.bits());
    assert!(DISALLOWS_TILING == B::FullScreenDisallowsTiling.bits());
};

/// #4419 — the collection behaviour a titled window is given: `current`
/// with a full-screen choice and a tiling choice where it holds none. A
/// resizable window may go full screen and be tiled; a fixed-size one may do
/// neither. A choice `current` already holds is kept, and so is every other
/// bit: AppKit takes at most one of primary, auxiliary and none, and at most
/// one of the two tiling bits, and raises an exception of its own at two.
#[cfg(any(target_os = "macos", test))]
fn explicit_full_screen(current: usize, resizable: bool) -> usize {
    let mut behavior = current;
    if behavior & (PRIMARY | AUXILIARY | NONE) == 0 {
        behavior |= if resizable { PRIMARY } else { NONE };
    }
    if behavior & (ALLOWS_TILING | DISALLOWS_TILING) == 0 {
        behavior |= if resizable {
            ALLOWS_TILING
        } else {
            DISALLOWS_TILING
        };
    }
    behavior
}

/// #4419 — give the main window an explicit full-screen behaviour. Called
/// once at start-up, before the window is first shown: from then on AppKit
/// has nothing to guess. On the main thread (`setup` runs there) it acts at
/// once; from another thread it is queued ahead of any later show.
#[cfg(target_os = "macos")]
pub fn make_explicit<R: tauri::Runtime>(app: &tauri::AppHandle<R>) {
    use objc2_app_kit::{NSWindow, NSWindowCollectionBehavior, NSWindowStyleMask};
    use tauri::Manager;

    let Some(window) = app.get_webview_window("main") else {
        tracing::warn!("no main window to give a full-screen behaviour (#4419)");
        return;
    };
    let w = window.clone();
    let queued = window.run_on_main_thread(move || {
        let Ok(ptr) = w.ns_window() else {
            tracing::warn!(
                "the main window has no NSWindow to give a full-screen behaviour (#4419)"
            );
            return;
        };
        // SAFETY: `ns_window` is this live window's own `NSWindow`, and this
        // runs on the main thread, where AppKit must be called.
        let ns = unsafe { &*ptr.cast::<NSWindow>() };
        let resizable = ns.styleMask().contains(NSWindowStyleMask::Resizable);
        let before = ns.collectionBehavior();
        let after = NSWindowCollectionBehavior(explicit_full_screen(before.0, resizable));
        if after != before {
            ns.setCollectionBehavior(after);
        }
        tracing::info!(
            before = before.0,
            after = after.0,
            resizable,
            "the main window's full-screen behaviour is explicit (#4419)"
        );
    });
    if let Err(e) = queued {
        tracing::warn!(%e, "could not give the main window a full-screen behaviour (#4419)");
    }
}

/// Elsewhere there is nothing to make explicit.
#[cfg(not(target_os = "macos"))]
pub fn make_explicit<R: tauri::Runtime>(_app: &tauri::AppHandle<R>) {}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a window's full-screen choice and tiling choice are: the bits
    /// of each that are set.
    fn choices(behavior: usize) -> (usize, usize) {
        (
            behavior & (PRIMARY | AUXILIARY | NONE),
            behavior & (ALLOWS_TILING | DISALLOWS_TILING),
        )
    }

    /// #4419 — a window made with no full-screen bit (tao sets none) is
    /// given exactly one full-screen choice and exactly one tiling choice,
    /// so AppKit has nothing left to guess. Before #4419 the main window
    /// kept the behaviour it was made with, 0, and AppKit guessed both.
    #[test]
    fn a_window_made_with_no_choice_is_given_one_of_each() {
        for resizable in [true, false] {
            let (full_screen, tiling) = choices(explicit_full_screen(0, resizable));
            assert_eq!(
                full_screen.count_ones(),
                1,
                "resizable={resizable}: the full-screen choice is left to AppKit's guess"
            );
            assert_eq!(
                tiling.count_ones(),
                1,
                "resizable={resizable}: the tiling choice is left to AppKit's guess"
            );
        }
    }

    /// #4419 — the main window is resizable: it may go full screen and be
    /// tiled. 0x880, the value Firefox's windows are made with since the
    /// same fix.
    #[test]
    fn a_resizable_window_may_go_full_screen_and_be_tiled() {
        assert_eq!(explicit_full_screen(0, true), PRIMARY | ALLOWS_TILING);
        assert_eq!(PRIMARY | ALLOWS_TILING, 0x880);
    }

    /// #4419 — a fixed-size window may neither go full screen nor be tiled.
    #[test]
    fn a_fixed_size_window_may_neither_go_full_screen_nor_be_tiled() {
        assert_eq!(explicit_full_screen(0, false), NONE | DISALLOWS_TILING);
    }

    /// #4419 — a choice the window already holds is kept, never joined by a
    /// second one (two raise AppKit's own exception), and every other bit
    /// stays as it was.
    #[test]
    fn a_choice_already_made_and_every_other_bit_are_kept() {
        // CanJoinAllSpaces and Managed: not full-screen bits.
        const CAN_JOIN_ALL_SPACES: usize = 1;
        const MANAGED: usize = 1 << 2;
        let other = CAN_JOIN_ALL_SPACES | MANAGED;
        assert_eq!(
            explicit_full_screen(other, true),
            other | PRIMARY | ALLOWS_TILING
        );
        for resizable in [true, false] {
            for held in [PRIMARY, AUXILIARY, NONE] {
                let given = explicit_full_screen(held, resizable);
                assert_eq!(choices(given).0, held, "resizable={resizable}");
            }
            for held in [ALLOWS_TILING, DISALLOWS_TILING] {
                let given = explicit_full_screen(held, resizable);
                assert_eq!(choices(given).1, held, "resizable={resizable}");
            }
            let both = AUXILIARY | DISALLOWS_TILING;
            assert_eq!(explicit_full_screen(both, resizable), both);
        }
    }
}
