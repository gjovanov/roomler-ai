// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! The account signed in to this process's session, for the install
//! clean-up's per-user arm (`install_cleanup.rs`).
//!
//! ## Why
//!
//! The clean-up removes a person's data trees from their profile when an
//! install changes flavour. It runs elevated, often as an account other than
//! the person signed in (an operator's `roomlerd cleanup-legacy-install`), so
//! its own profile is not theirs, and a SystemContext worker's (`LocalSystem`)
//! is `C:\Windows\System32\config\systemprofile\`.
//!
//! A SystemContext worker never loads its CONFIG from here: it runs
//! the machine's own enrollment and nothing else (#3387, `app.rs`
//! `pick_config_path`). The person's own enrollment runs as that
//! person, on the service host's user-context arm. Nor does it find the
//! person's Downloads here: a received file is written as the person,
//! into the Downloads their token names (#3048, `files::FsAs`).
//!
//! ## How (FR-145)
//!
//! 1. `ProcessIdToSessionId(GetCurrentProcessId())`: this process's
//!    session, whether the person is at the lock screen or working.
//! 2. `WTSQuerySessionInformationW` for `WTSUserName` and `WTSDomainName`:
//!    the account signed in there; an empty name when nobody is.
//! 3. `LookupAccountNameW("<domain>\<user>")`: that account's SID, a user
//!    account only (`SidTypeUser`). The domain-qualified name names exactly
//!    one account, so a local account and a domain account of the same name
//!    are never confused. It needs no privilege beyond the elevated
//!    operator's, where the session's own token (`WTSQueryUserToken`) would
//!    need SYSTEM's.
//!
//! The clean-up then takes the profile the system records for that SID
//! (`appdirs::profile_of_account`, `ProfileList\<sid>\ProfileImagePath`).
//! Nothing here builds a profile path from a user name: `C:\Users\<name>` can
//! be another account's folder (a local and a domain account of one name), or
//! none (`<name>.000`, `<name>.<DOMAIN>`, a renamed account, a profile on
//! another drive). `win_identity.rs` takes the file transfers' profile from
//! the person's token for the same reason.

#![cfg(all(feature = "system-context", target_os = "windows"))]

use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Security::{LookupAccountNameW, SID_NAME_USE, SidTypeUser};
use windows_sys::Win32::System::RemoteDesktop::{
    ProcessIdToSessionId, WTS_CURRENT_SERVER_HANDLE, WTS_INFO_CLASS, WTSDomainName, WTSFreeMemory,
    WTSQuerySessionInformationW, WTSUserName,
};
use windows_sys::Win32::System::Threading::GetCurrentProcessId;

/// The user name of the account signed in to this process's session.
/// `None` when nobody is signed in, or any of the discovery steps fail.
pub fn session_user_name() -> Option<String> {
    session_text(current_session_id()?, WTSUserName)
}

/// The SID of the account signed in to this process's session, as text
/// (`S-1-5-21-…`): its domain-qualified name looked up with
/// `LookupAccountNameW`, a user account only. `None` when nobody is signed
/// in, or the account cannot be told.
pub fn session_account_sid() -> Option<String> {
    let session = current_session_id()?;
    let user = session_text(session, WTSUserName)?;
    let domain = session_text(session, WTSDomainName)?;
    account_sid(&format!("{domain}\\{user}"))
}

/// The SID of the user account `account` (`<domain>\<user>`), as text.
fn account_sid(account: &str) -> Option<String> {
    let name: Vec<u16> = account.encode_utf16().chain(Some(0)).collect();
    let mut sid_len = 0u32;
    let mut domain_len = 0u32;
    let mut kind: SID_NAME_USE = 0;
    // SAFETY: a NUL-terminated name; null buffers with zero sizes ask for the
    // sizes, which the call writes back.
    unsafe {
        LookupAccountNameW(
            std::ptr::null(),
            name.as_ptr(),
            std::ptr::null_mut(),
            &mut sid_len,
            std::ptr::null_mut(),
            &mut domain_len,
            &mut kind,
        )
    };
    if sid_len == 0 {
        return None;
    }
    let mut sid = vec![0u8; sid_len as usize];
    let mut domain = vec![0u16; domain_len.max(1) as usize];
    // SAFETY: buffers of the sizes the first call asked for, alive across
    // the call.
    let ok = unsafe {
        LookupAccountNameW(
            std::ptr::null(),
            name.as_ptr(),
            sid.as_mut_ptr().cast(),
            &mut sid_len,
            domain.as_mut_ptr(),
            &mut domain_len,
            &mut kind,
        )
    };
    if ok == 0 || kind != SidTypeUser {
        return None;
    }
    Some(crate::win_token::sid_string(sid.as_mut_ptr().cast()))
}

/// `ProcessIdToSessionId(GetCurrentProcessId())`: the session this
/// process is attached to.
fn current_session_id() -> Option<u32> {
    // SAFETY: GetCurrentProcessId is a thread-safe Win32 call with
    // no preconditions, no ownership concerns.
    let pid = unsafe { GetCurrentProcessId() };
    let mut sid: u32 = 0;
    // SAFETY: `sid` is a stack-allocated u32 the API writes into.
    // Return value: 0 on failure, non-zero on success.
    let ok = unsafe { ProcessIdToSessionId(pid, &mut sid) };
    if ok == 0 { None } else { Some(sid) }
}

/// `WTSQuerySessionInformationW(NULL, session, class, ...)` → owned
/// `String`. `None` for a failure or an empty answer (no signed-in user,
/// typically a fresh boot before the first sign-in).
fn session_text(session: u32, class: WTS_INFO_CLASS) -> Option<String> {
    let mut buffer: *mut u16 = std::ptr::null_mut();
    let mut bytes_returned: u32 = 0;

    // SAFETY: WTS_CURRENT_SERVER_HANDLE is a documented sentinel that means
    // "this server"; `class` is a documented value; the out-params are stack
    // pointers the API writes into. On success the buffer is OS-allocated
    // and released via WTSFreeMemory below.
    let ok = unsafe {
        WTSQuerySessionInformationW(
            WTS_CURRENT_SERVER_HANDLE as HANDLE,
            session,
            class,
            &mut buffer,
            &mut bytes_returned,
        )
    };
    if ok == 0 || buffer.is_null() {
        return None;
    }
    let result = unsafe { extract_wide_string(buffer) };
    // SAFETY: We own `buffer` via the WTS contract; pair Free with
    // the successful Query.
    unsafe { WTSFreeMemory(buffer as *mut _) };
    let s = result?;
    if s.is_empty() { None } else { Some(s) }
}

/// Read a NUL-terminated UTF-16 string from a raw pointer into an
/// owned `String`. Lossy: invalid UTF-16 surrogates become the
/// Unicode replacement character. Returns `None` if the pointer is
/// null. Caller must guarantee the pointer is valid for the duration
/// of the call.
///
/// SAFETY: `ptr` must be a valid pointer to a NUL-terminated UTF-16
/// sequence within an allocation owned by the caller.
unsafe fn extract_wide_string(ptr: *const u16) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    let mut len = 0usize;
    while unsafe { *ptr.add(len) } != 0 {
        len += 1;
        // Sanity cap: usernames are at most ~256 chars on Windows.
        // 4096 covers any realistic case and bounds the loop in
        // case the buffer is unexpectedly unterminated.
        if len > 4096 {
            return None;
        }
    }
    if len == 0 {
        return Some(String::new());
    }
    let slice = unsafe { std::slice::from_raw_parts(ptr, len) };
    Some(String::from_utf16_lossy(slice))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_session_id_returns_some_under_test_runner() {
        // Cargo test runner is in some session (1 typically). Lock
        // against panic + None return.
        let sid = current_session_id();
        assert!(
            sid.is_some(),
            "current_session_id should succeed on any logged-in box"
        );
    }

    /// FR-145 — the session's account is told by its SID, and the system
    /// records a profile for it: what the per-user clean-up removes from.
    #[test]
    fn the_session_account_is_told_by_its_sid() {
        let Some(name) = session_user_name() else {
            eprintln!("NOTE: nobody is signed in to this session; nothing to tell");
            return;
        };
        let sid = session_account_sid().expect("the signed-in account's SID");
        assert!(sid.starts_with("S-1-"), "{name}: {sid}");
        assert!(
            crate::appdirs::profile_of_account(&sid).is_some(),
            "{name} ({sid}) has a profile the system records"
        );
    }

    /// Only a user account is told: a group's name, or a name no account
    /// has, is not.
    #[test]
    fn a_name_that_is_no_user_account_has_no_sid() {
        assert_eq!(account_sid(r"NT AUTHORITY\NETWORK"), None, "a group");
        assert_eq!(account_sid("no-such-account-fr145"), None);
    }

    #[test]
    fn extract_wide_string_handles_empty() {
        let zero: u16 = 0;
        // SAFETY: zero is a valid local; address is valid for the
        // lifetime of this scope; first u16 is 0 (terminator).
        let result = unsafe { extract_wide_string(&zero) };
        assert_eq!(result, Some(String::new()));
    }

    #[test]
    fn extract_wide_string_handles_simple_ascii() {
        let buf: Vec<u16> = "hello\0".encode_utf16().collect();
        // SAFETY: buf is alive for the call; last element is 0.
        let result = unsafe { extract_wide_string(buf.as_ptr()) };
        assert_eq!(result, Some("hello".to_string()));
    }

    #[test]
    fn extract_wide_string_returns_none_on_null() {
        let result = unsafe { extract_wide_string(std::ptr::null()) };
        assert!(result.is_none());
    }
}
