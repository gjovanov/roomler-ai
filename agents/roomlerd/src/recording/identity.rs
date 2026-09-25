// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-85 — who the recorder would run as.
//!
//! A LOCAL recording is asked for by the console user (the LocalAPI gate
//! checks that) and belongs to them: it goes into their folder, as them. The
//! recorder inherits the daemon's identity, so a daemon running as SYSTEM (the
//! Windows SystemContext worker) or root (the Linux system unit, the macOS
//! LaunchDaemon) would record into the service account's own profile —
//! `C:\Windows\System32\config\systemprofile\Videos`, `/root/Videos` — where
//! the person who pressed Record cannot even see the file.
//!
//! Until the recorder is launched as the console user (FR-85 P1e), such a
//! daemon refuses a local recording and says why. `roomlerd record` run by
//! hand is not gated: whoever runs it gets their own folder.

/// Is this process a service account — SYSTEM on Windows, root on unix?
///
/// When the answer cannot be read, `true`: a refusal names itself, a
/// recording saved into the wrong profile does not.
pub fn daemon_is_service_account() -> bool {
    imp()
}

#[cfg(unix)]
fn imp() -> bool {
    // SAFETY: `geteuid` reads the caller's own credentials; it cannot fail.
    unsafe { libc::geteuid() == 0 }
}

#[cfg(windows)]
fn imp() -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Security::{
        GetTokenInformation, IsWellKnownSid, TOKEN_QUERY, TOKEN_USER, TokenUser, WinLocalSystemSid,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: the pseudo-handle of the current process, and a valid out-param.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return true;
    }
    let mut len: u32 = 0;
    // SAFETY: a size query — a null buffer of length 0 is the documented form.
    unsafe { GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut len) };
    let is_system = if len == 0 {
        true
    } else {
        // `u64`s, so the buffer is aligned for the pointer inside TOKEN_USER.
        let mut buf = vec![0u64; (len as usize).div_ceil(8)];
        // SAFETY: `buf` holds at least `len` bytes; `len` is the size the
        // query above asked for.
        let ok = unsafe {
            GetTokenInformation(token, TokenUser, buf.as_mut_ptr().cast(), len, &mut len)
        };
        if ok == 0 {
            true
        } else {
            // SAFETY: on success the buffer starts with a TOKEN_USER whose SID
            // points into the same buffer, which outlives this read.
            let user = unsafe { &*(buf.as_ptr().cast::<TOKEN_USER>()) };
            // SAFETY: a valid SID pointer from the token.
            unsafe { IsWellKnownSid(user.User.Sid, WinLocalSystemSid) != 0 }
        }
    };
    // SAFETY: we own the token handle.
    unsafe { CloseHandle(token) };
    is_system
}

#[cfg(not(any(unix, windows)))]
fn imp() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A test run is an ordinary user in CI and on a dev box; the probe must
    /// not misread it as a service account (which would refuse every local
    /// recording on a healthy per-user install).
    #[test]
    fn a_test_process_is_not_a_service_account() {
        #[cfg(unix)]
        // SAFETY: as above.
        if unsafe { libc::geteuid() } == 0 {
            return; // a root container: the probe is right to say so
        }
        assert!(!daemon_is_service_account());
    }
}
