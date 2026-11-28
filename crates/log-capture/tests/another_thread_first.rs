// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! The capture helper's probe (#3403): another thread reaches a fresh line
//! first, and the capture's own thread still sees it.
//!
//! ⚠️ This is the ONLY test in this binary, on purpose. A test beside it that
//! made a capture of its own would register dispatchers of its own, and could
//! hide a helper that no longer holds a second one. Alone, this test is a run
//! filtered to it by construction. It turns red when the helper's second
//! dispatcher is `Dispatch::none()`, which never registers.

/// The line both threads reach. Only this test reaches it, so it is fresh: no
/// thread has registered its callsite before.
fn reach() {
    tracing::warn!("capture probe: a line two threads reach");
}

#[test]
fn the_capture_sees_a_line_another_thread_reached_first() {
    let log = roomler_log_capture::Captured::default();
    let _scope = roomler_log_capture::set_default(roomler_log_capture::fmt(log.clone()).finish());
    // Another thread, with no subscriber of its own, reaches the line first.
    std::thread::spawn(reach).join().unwrap();
    reach();
    log.saw("capture probe: a line two threads reach");
}
