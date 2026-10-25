// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! #2886 — test support: does a child hold a handle of ours?
//!
//! Asked of the child's own handle table, never inferred from a pipe's
//! end-of-file. Inheritance is process-wide: `std::process::Command` spawns
//! with `bInheritHandles = TRUE` and no handle list, so any other test thread
//! that spawns while an inheritable decoy is alive hands it to ITS child,
//! which holds it for as long as it runs — a `cmd /c ping -n 30` whose `ping`
//! outlives the killed `cmd` holds it for 29 s. Two tests that timed a decoy's
//! EOF blamed a bounded spawn for that: green alone, red under the suite,
//! redder under build load.

#![cfg(all(test, target_os = "windows"))]

use windows_sys::Win32::Foundation::{
    CloseHandle, CompareObjectHandles, DUPLICATE_SAME_ACCESS, DuplicateHandle, FALSE, HANDLE,
};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

/// Whether `process` holds a handle to the object `ours` names here. An
/// inherited handle keeps its VALUE in the child, so the child's table is
/// asked at that value; the slot may since have been reused for something
/// the child opened itself, so the objects are compared, not the slot's
/// occupancy. `process` needs `PROCESS_DUP_HANDLE`, which a `CreateProcess*`
/// handle carries.
pub(crate) fn held_by(process: HANDLE, ours: HANDLE) -> bool {
    let mut dup: HANDLE = std::ptr::null_mut();
    // SAFETY: `process` is a live process handle the caller holds; `ours` is
    // a value, looked up in that process's table; the out-param is valid.
    let ok = unsafe {
        DuplicateHandle(
            process,
            ours,
            GetCurrentProcess(),
            &mut dup,
            0,
            FALSE,
            DUPLICATE_SAME_ACCESS,
        )
    };
    if ok == 0 || dup.is_null() {
        // ERROR_INVALID_HANDLE: nothing at that value over there.
        return false;
    }
    // SAFETY: both handles are live and this process's own; `dup` is ours to
    // close, and closed once.
    unsafe {
        let same = CompareObjectHandles(dup, ours) != FALSE;
        CloseHandle(dup);
        same
    }
}

/// The control: a spawn that inherits everything inheritable — std's
/// `Command`, `bInheritHandles = TRUE`, no handle list — alive for a few
/// seconds and touching nothing. [`held_by`] asked of it, for a decoy alive
/// at the spawn, must say yes, or a test can see nothing.
pub(crate) fn unbounded_sibling() -> std::process::Child {
    std::process::Command::new("ping")
        .args(["-n", "6", "127.0.0.1"])
        .stdout(std::process::Stdio::null())
        .spawn()
        .expect("spawn the unbounded sibling")
}
