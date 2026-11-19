// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! #3289 — which boot of the OS this process runs in.
//!
//! The crash-loop detector asks one question of it
//! ([`crate::config::previous_run`]): did the OS restart since the last run
//! began? A run the OS ended, by a restart, a power cut or a kernel fault, did
//! not crash, and a healthy device must not roll itself back over three of them.
//!
//! An id, never a time: a boot time worked out from the wall clock moves when
//! the clock is stepped (a Win11 guest stepped by 9 hours at start in #3289's
//! own field read), and two starts of one boot would then read as two boots.

/// An opaque id: the same for every call within one boot of the OS, different
/// after a restart. `None` when this host cannot say. The caller reads that as
/// "the same boot", which is the side that still counts a crash.
pub fn current() -> Option<String> {
    current_impl()
}

#[cfg(target_os = "linux")]
fn current_impl() -> Option<String> {
    let id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
    let id = id.trim();
    (!id.is_empty()).then(|| id.to_string())
}

#[cfg(target_os = "macos")]
fn current_impl() -> Option<String> {
    let mut buf = [0u8; 64];
    let mut len = buf.len();
    // SAFETY: a fixed C-string name, a byte buffer with its size, no new value.
    let rc = unsafe {
        libc::sysctlbyname(
            c"kern.bootsessionuuid".as_ptr(),
            buf.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return None;
    }
    let id = std::str::from_utf8(buf.get(..len)?).ok()?;
    let id = id.trim_end_matches('\0').trim();
    (!id.is_empty()).then(|| id.to_string())
}

#[cfg(windows)]
fn current_impl() -> Option<String> {
    // `KUSER_SHARED_DATA` is mapped read-only at 0x7FFE0000 in every user-mode
    // process on every NT architecture (`KI_USER_SHARED_DATA` in the WDK's
    // ntddk.h), and `BootId` sits at offset 0x2C4 there since Windows 8. Windows
    // has no API for it. Read on Win11 26200 it equals the registry's
    // `...\Memory Management\PrefetchParameters\BootId`, and a restart moves it.
    const BOOT_ID: *const u32 = 0x7FFE_02C4 as *const u32;
    // SAFETY: the page is mapped for the process's whole life, the address is
    // aligned for a u32, and the kernel only ever writes it at boot.
    let id = unsafe { std::ptr::read_volatile(BOOT_ID) };
    (id != 0).then(|| id.to_string())
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn current_impl() -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_id_holds_still_within_a_boot() {
        let first = current();
        // A host that can say, says something: `None` would quietly turn the
        // restart check off, and every OS restart back into a crash.
        #[cfg(any(target_os = "linux", target_os = "macos", windows))]
        assert!(first.as_deref().is_some_and(|id| !id.is_empty()));
        assert_eq!(first, current());
    }
}
