// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! Is THIS process `LocalSystem`, and whose profile does its session belong
//! to? — answered on every Windows build (FR-84 D4).
//!
//! `system_context::worker_role` answers the first question for the capture /
//! input plumbing, but it is compiled only with the `system-context` feature.
//! The `files_dir` placement rule — a SYSTEM writer stays inside the active
//! user's profile — is a security control and must not depend on which
//! features a build carries: a `roomlerd run` started under a SYSTEM token by
//! any means (`psexec -s -i`, a scheduled task running as SYSTEM) writes
//! dropped files as SYSTEM whether or not it can capture the lock screen. So
//! the token read lives here, feature-free, reading the same bytes
//! `worker_role` does.
//!
//! The profile comes from the session user's TOKEN, never from their name:
//! `system_context::user_profile::active_user_profile_root` builds
//! `C:\Users\<name>`, which is wrong for a redirected profile and — worse,
//! for a security rule — names ANOTHER account's folder when a local and a
//! domain account share a name (`C:\Users\alice` vs `C:\Users\alice.CORP`).

#![cfg(target_os = "windows")]

use windows_sys::Win32::Foundation::{CloseHandle, ERROR_SUCCESS, HANDLE, LocalFree};
use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows_sys::Win32::Security::{
    GetLengthSid, GetTokenInformation, IsValidSid, PSID, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::System::Registry::{
    HKEY_LOCAL_MACHINE, RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ, RegGetValueW,
};
use windows_sys::Win32::System::RemoteDesktop::{
    ProcessIdToSessionId, WTS_CURRENT_SERVER_HANDLE, WTS_PROCESS_INFOW, WTSEnumerateProcessesW,
    WTSFreeMemory,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentProcessId, OpenProcessToken,
};
use windows_sys::Win32::UI::Shell::GetUserProfileDirectoryW;
use windows_sys::core::PWSTR;

/// `true` when the calling process's primary token is `S-1-5-18`
/// (`NT AUTHORITY\SYSTEM`). Any failure to read the token is `false`: the
/// only consequence of a wrong `false` is the ordinary (user) folder rules,
/// which SYSTEM still cannot be talked past on the system roots.
pub fn process_is_local_system() -> bool {
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: `GetCurrentProcess` is a pseudo-handle that is always valid;
    // `OpenProcessToken` with `TOKEN_QUERY` on our own process is documented
    // infallible in normal conditions; the out-pointer is checked on return.
    let ok = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) };
    if ok == 0 || token.is_null() {
        return false;
    }
    let result = token_user_is_local_system(token);
    // SAFETY: we own the handle `OpenProcessToken` gave us.
    unsafe { CloseHandle(token) };
    result
}

/// `true` when the calling process runs elevated, with a full administrator
/// token ("Run as administrator", or UAC off), or as `LocalSystem`. Any failure
/// to read the token is `false`: the answer that asks for elevation up front,
/// which beats an access-denied halfway. FR-104's anchor writer refuses on it;
/// the setup wizard (#3116) asks for it before a SystemContext install spends
/// an enrollment token on a config it could not write.
pub fn process_is_elevated() -> bool {
    use windows_sys::Win32::Security::{TOKEN_ELEVATION, TokenElevation};
    if process_is_local_system() {
        return true;
    }
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: our own process's pseudo-handle; the token is closed below.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0
        || token.is_null()
    {
        return false;
    }
    let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
    let mut len = 0u32;
    // SAFETY: a live token; `elevation` is sized for TokenElevation.
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            (&raw mut elevation).cast(),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        )
    };
    // SAFETY: the handle OpenProcessToken gave us, closed once.
    unsafe { CloseHandle(token) };
    ok != 0 && elevation.TokenIsElevated != 0
}

fn token_user_is_local_system(token: HANDLE) -> bool {
    let mut needed: u32 = 0;
    // SAFETY: the documented size-discovery call — a NULL buffer and a zero
    // length; `ERROR_INSUFFICIENT_BUFFER` is the expected outcome.
    let _ = unsafe { GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut needed) };
    if needed == 0 {
        return false;
    }
    let mut buf: Vec<u8> = vec![0; needed as usize];
    // SAFETY: `buf` is alive for the call and at least `needed` bytes long;
    // the OS writes at most `needed` bytes into it.
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            buf.as_mut_ptr().cast(),
            needed,
            &mut needed,
        )
    };
    if ok == 0 {
        return false;
    }
    // SAFETY: the buffer holds a `TOKEN_USER`; its `User.Sid` points INTO
    // this same buffer (the SID follows the struct) — the documented idiom.
    let token_user = unsafe { &*(buf.as_ptr() as *const TOKEN_USER) };
    let sid = token_user.User.Sid as *const u8;
    if sid.is_null() {
        return false;
    }
    // Bound the read to the buffer the OS filled: a SID we compare needs its
    // 8-byte header and one 4-byte sub-authority.
    let start = sid as usize;
    let base = buf.as_ptr() as usize;
    if start < base || start + 12 > base + buf.len() {
        return false;
    }
    // SAFETY: the 12 bytes at `sid` are inside `buf`, checked just above.
    let bytes = unsafe { std::slice::from_raw_parts(sid, 12) };
    sid_bytes_are_local_system(bytes)
}

/// The profile directory of the user signed in to THIS process's session —
/// the "active user" for a SYSTEM writer. A SystemContext worker lives in
/// the interactive session it serves, so that is the person at the screen
/// being shared; a SYSTEM process in session 0 has no such user.
///
/// `None` when there is no user in the session (session 0, the pre-logon
/// lock screen), when this process may not ask (`WTSQueryUserToken` needs
/// SYSTEM with `SeTcbPrivilege` — any other caller gets
/// `ERROR_PRIVILEGE_NOT_HELD`), or on any other failure. `None` refuses
/// every `files_dir` for a privileged writer: a security rule that cannot
/// name the profile does not guess one.
pub fn session_user_profile_dir() -> Option<String> {
    let mut session: u32 = 0;
    // SAFETY: `GetCurrentProcessId` has no preconditions; `session` is a
    // live out-pointer the call writes one u32 into.
    let ok = unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session) };
    if ok == 0 {
        return None;
    }
    match crate::win_service::supervisor::query_user_token(session) {
        Ok(Some(token)) => profile_dir_of_token(token.raw()),
        Ok(None) => None,
        Err(e) => {
            tracing::debug!(%e, session, "files_dir: no user token for this session");
            None
        }
    }
}

/// Run `f` on this thread AS the user signed in to this process's session.
/// For a SYSTEM writer touching a path that user controls — their profile.
///
/// ⚠️ Every file access inside `f` is checked against THEIR token. That is
/// the point: a profile folder is the user's to rearrange, so a SYSTEM write
/// there can be steered by a junction plus an object-manager link to ANY
/// path SYSTEM can write (the classic arbitrary-file-overwrite escalation).
/// Validating the path first does not close it — the link can be swapped
/// between the check and the write. Impersonated, the write can only land
/// where the user could have written anyway.
///
/// `None` when nobody is signed in to the session, when the token cannot be
/// had (`WTSQueryUserToken` needs SYSTEM with `SeTcbPrivilege`), or when
/// impersonation is refused. Then `f` has not run: a privileged writer that
/// cannot become the user does not fall back to writing as itself.
pub fn as_session_user<T>(f: impl FnOnce() -> T) -> Option<T> {
    use windows_sys::Win32::Security::{ImpersonateLoggedOnUser, RevertToSelf};

    /// Ends the impersonation even if `f` panics.
    struct Revert;
    impl Drop for Revert {
        fn drop(&mut self) {
            // SAFETY: ends this thread's impersonation.
            if unsafe { RevertToSelf() } == 0 {
                // A thread left wearing someone else's identity would run
                // whatever it does next as the wrong account. There is no
                // safe way on; the service manager restarts us.
                std::process::abort();
            }
        }
    }

    let mut session: u32 = 0;
    // SAFETY: as in `session_user_profile_dir`.
    if unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session) } == 0 {
        return None;
    }
    let token = match crate::win_service::supervisor::query_user_token(session) {
        Ok(Some(t)) => t,
        Ok(None) => return None,
        Err(e) => {
            tracing::debug!(%e, session, "as_session_user: no user token for this session");
            return None;
        }
    };
    // SAFETY: a live primary token from `WTSQueryUserToken`. Should the
    // impersonation be refused, Windows hands the thread an identification
    // token instead and every open fails: closed, never open.
    if unsafe { ImpersonateLoggedOnUser(token.raw()) } == 0 {
        tracing::warn!(session, "as_session_user: ImpersonateLoggedOnUser refused");
        return None;
    }
    let _revert = Revert;
    Some(f())
}

/// #3048 — the person signed in to this process's session, as a SYSTEM
/// worker writes a received file for them: read from their token, never from
/// a name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionPerson {
    /// Their SID (`S-1-5-21-…`).
    pub sid: String,
    /// Their profile folder, as Windows records it for them.
    pub profile: std::path::PathBuf,
    /// Their Downloads known folder, read without verifying it, so the
    /// lookup never touches a share it is redirected to.
    pub downloads: Option<std::path::PathBuf>,
}

/// #3048 — where a SYSTEM worker takes the person's token from, at every step
/// it runs as them.
#[derive(Clone)]
pub enum PersonToken {
    /// The person signed in to this process's session, asked for again at
    /// each step (`WTSQueryUserToken`).
    Session,
    /// A token a test holds, standing in for the person.
    #[cfg(test)]
    Fixed(std::sync::Arc<crate::win_token::Owned>),
}

impl PartialEq for PersonToken {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Session, Self::Session) => true,
            #[cfg(test)]
            (Self::Fixed(a), Self::Fixed(b)) => std::sync::Arc::ptr_eq(a, b),
            #[cfg(test)]
            _ => false,
        }
    }
}

impl Eq for PersonToken {}

impl std::fmt::Debug for PersonToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Session => f.write_str("Session"),
            #[cfg(test)]
            Self::Fixed(_) => f.write_str("Fixed"),
        }
    }
}

/// #3048 — the person signed in to this process's session, read from their
/// token. `None` when nobody is signed in to it, or when this process may not
/// ask (`WTSQueryUserToken` needs SYSTEM).
pub fn session_person() -> Option<SessionPerson> {
    let token = session_user_token()?;
    Some(SessionPerson {
        sid: token_user_sid(token.raw())?,
        profile: profile_dir_of_token(token.raw())?.into(),
        downloads: downloads_of_token(token.raw()),
    })
}

/// The token of the user signed in to this process's session, now.
fn session_user_token() -> Option<crate::win_service::supervisor::OwnedHandle> {
    let mut session: u32 = 0;
    // SAFETY: as in `session_user_profile_dir`.
    if unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session) } == 0 {
        return None;
    }
    match crate::win_service::supervisor::query_user_token(session) {
        Ok(token) => token,
        Err(e) => {
            tracing::debug!(%e, session, "files: no user token for this session");
            None
        }
    }
}

/// `token`'s user, as a SID string.
fn token_user_sid(token: HANDLE) -> Option<String> {
    let buf = crate::win_token::token_info(token, TokenUser)?;
    // SAFETY: a TOKEN_USER whose SID points into `buf`.
    Some(crate::win_token::sid_string(unsafe {
        (*(buf.as_ptr() as *const TOKEN_USER)).User.Sid
    }))
}

/// The Downloads known folder of `token`'s user, NOT verified
/// (`KF_FLAG_DONT_VERIFY`): the lookup never touches a share the folder is
/// redirected to, so it cannot stall on one that is offline.
fn downloads_of_token(token: HANDLE) -> Option<std::path::PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::{
        FOLDERID_Downloads, KF_FLAG_DONT_VERIFY, SHGetKnownFolderPath,
    };
    let mut raw: PWSTR = std::ptr::null_mut();
    // SAFETY: a valid GUID, a live token, and an out-param freed below.
    // The flag is a non-negative `KNOWN_FOLDER_FLAG` (i32); the call takes it
    // as a u32.
    let flags = KF_FLAG_DONT_VERIFY as u32;
    let hr = unsafe { SHGetKnownFolderPath(&FOLDERID_Downloads, flags, token, &mut raw) };
    let path = (hr >= 0 && !raw.is_null()).then(|| {
        // SAFETY: on success `raw` is a NUL-terminated wide string.
        let len = (0..).take_while(|&i| unsafe { *raw.add(i) } != 0).count();
        std::path::PathBuf::from(std::ffi::OsString::from_wide(unsafe {
            std::slice::from_raw_parts(raw, len)
        }))
    });
    // SAFETY: allocated with CoTaskMemAlloc, even on failure; null is a no-op.
    unsafe { CoTaskMemFree(raw as *const core::ffi::c_void) };
    path
}

/// #3048 — run `f` on this thread AS the person whose SID is `sid`, with the
/// token `source` gives: the session user's, asked for now, or a test's.
///
/// `Err`, and `f` never run, when there is no token (nobody signed in), when
/// it is another person's, or when the impersonation is refused: a SYSTEM
/// worker that cannot act as the person never acts as itself instead. The
/// impersonation ends when `f` returns, a panic included, and a thread that
/// cannot end it aborts the process: it would run whatever came next as the
/// wrong account.
///
/// ⚠️ Run it only on a blocking thread, with no `.await` inside `f`: a tokio
/// worker must never carry the person's token.
pub fn as_person<T>(sid: &str, source: &PersonToken, f: impl FnOnce() -> T) -> Result<T, String> {
    use windows_sys::Win32::Security::{ImpersonateLoggedOnUser, RevertToSelf};

    /// Ends the impersonation even if `f` panics.
    struct Revert;
    impl Drop for Revert {
        fn drop(&mut self) {
            // SAFETY: ends this thread's impersonation.
            if unsafe { RevertToSelf() } == 0 {
                std::process::abort();
            }
        }
    }

    let session_token;
    let token = match source {
        PersonToken::Session => {
            session_token =
                session_user_token().ok_or_else(|| crate::files::NOT_THE_PERSON.to_string())?;
            session_token.raw()
        }
        #[cfg(test)]
        PersonToken::Fixed(t) => t.raw(),
    };
    match token_user_sid(token) {
        Some(theirs) if theirs.eq_ignore_ascii_case(sid) => {}
        _ => return Err(crate::files::NOT_THE_PERSON.to_string()),
    }
    // SAFETY: a live token of the person; a refused impersonation leaves the
    // thread with an identification token, on which every open fails.
    if unsafe { ImpersonateLoggedOnUser(token) } == 0 {
        return Err(crate::win_token::last_error("ImpersonateLoggedOnUser"));
    }
    let _revert = Revert;
    Ok(f())
}

/// This process's OWN profile directory, from its own token — an
/// unprivileged writer's home. A registry read, unlike the known-folder
/// lookups `directories::UserDirs` makes (those verify every folder they
/// resolve, so a Documents redirected to an offline share can stall them).
pub fn own_profile_dir() -> Option<String> {
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: as in `process_is_local_system`.
    let ok = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) };
    if ok == 0 || token.is_null() {
        return None;
    }
    let dir = profile_dir_of_token(token);
    // SAFETY: we own the handle `OpenProcessToken` gave us.
    unsafe { CloseHandle(token) };
    dir
}

/// Who this process's session belongs to, as far as an administrator can tell.
#[derive(Debug, PartialEq, Eq)]
pub enum SessionAccount {
    /// This account has a shell here, or no shell could be read at all.
    ThisOrUnknown,
    /// Another account's shell owns this session: an administrator's password
    /// was typed on a standard user's desktop. `profile` is that person's
    /// profile folder, when the machine names one.
    Another { profile: Option<String> },
}

/// #3229 — is this session someone else's? [`session_user_profile_dir`] cannot
/// say for an administrator: `WTSQueryUserToken` needs SYSTEM, and opening the
/// person's Explorer for its token is refused without `SeDebug`. But
/// `WTSEnumerateProcessesW` gives each process's owner with no token and no
/// domain controller, and the machine's `ProfileList` names that owner's folder.
///
/// A process named `explorer.exe` at the person's level proves nothing more than
/// that person's own SID, which is the answer asked for. The folder this leads
/// to is one that person can write in any case, so a caller treats it as such.
pub fn session_account() -> SessionAccount {
    let Some(own) = own_sid() else {
        return SessionAccount::ThisOrUnknown;
    };
    let mut session: u32 = 0;
    // SAFETY: as in `session_user_profile_dir`.
    if unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session) } == 0 {
        return SessionAccount::ThisOrUnknown;
    }
    match someone_elses_shell(&session_shell_owners(session), &own) {
        Some(owner) => SessionAccount::Another {
            profile: profile_of_sid(owner),
        },
        None => SessionAccount::ThisOrUnknown,
    }
}

/// The owner to believe, among the owners of a session's shells: none if this
/// account has a shell there too. Pure, so it is testable with synthetic SIDs.
fn someone_elses_shell<'a>(owners: &'a [Vec<u8>], own: &[u8]) -> Option<&'a [u8]> {
    if owners.iter().any(|owner| owner == own) {
        return None;
    }
    owners.first().map(Vec::as_slice)
}

/// The SIDs that own an `explorer.exe` in `session`.
fn session_shell_owners(session: u32) -> Vec<Vec<u8>> {
    let mut info: *mut WTS_PROCESS_INFOW = std::ptr::null_mut();
    let mut count: u32 = 0;
    // SAFETY: out-pointers to locals; version 1 is the only one documented, and
    // the list is freed below.
    let ok =
        unsafe { WTSEnumerateProcessesW(WTS_CURRENT_SERVER_HANDLE, 0, 1, &mut info, &mut count) };
    if ok == 0 || info.is_null() {
        return Vec::new();
    }
    // SAFETY: the call returned `count` entries at `info`, which stay valid until
    // `WTSFreeMemory`, and every use of them ends before it.
    let owners = unsafe { std::slice::from_raw_parts(info, count as usize) }
        .iter()
        .filter(|p| p.SessionId == session)
        .filter(|p| wide_to_string(p.pProcessName).eq_ignore_ascii_case("explorer.exe"))
        .filter_map(|p| sid_bytes(p.pUserSid))
        .collect();
    // SAFETY: `info` came from `WTSEnumerateProcessesW`.
    unsafe { WTSFreeMemory(info.cast()) };
    owners
}

/// This process's own SID.
fn own_sid() -> Option<Vec<u8>> {
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: as in `process_is_local_system`.
    let ok = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) };
    if ok == 0 || token.is_null() {
        return None;
    }
    let mut needed: u32 = 0;
    // SAFETY: the size query and read of `token_user_is_local_system`.
    let _ = unsafe { GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut needed) };
    let mut buf: Vec<u8> = vec![0; needed as usize];
    let read = needed != 0
        && unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                buf.as_mut_ptr().cast(),
                needed,
                &mut needed,
            )
        } != 0;
    // SAFETY: we own the handle `OpenProcessToken` gave us.
    unsafe { CloseHandle(token) };
    if !read {
        return None;
    }
    // SAFETY: the buffer holds a `TOKEN_USER` whose `User.Sid` points into it.
    sid_bytes(unsafe { &*(buf.as_ptr() as *const TOKEN_USER) }.User.Sid)
}

/// A copy of the SID at `sid`, when it is one.
fn sid_bytes(sid: PSID) -> Option<Vec<u8>> {
    // SAFETY: `IsValidSid` is the documented check of a SID pointer, and the
    // length it validated bounds the read.
    unsafe {
        if sid.is_null() || IsValidSid(sid) == 0 {
            return None;
        }
        Some(std::slice::from_raw_parts(sid as *const u8, GetLengthSid(sid) as usize).to_vec())
    }
}

/// `ProfileList\<SID>\ProfileImagePath`, expanded.
fn profile_of_sid(sid: &[u8]) -> Option<String> {
    let mut text: PWSTR = std::ptr::null_mut();
    // SAFETY: `sid` holds a whole SID (`sid_bytes`); `text` is freed below.
    if unsafe { ConvertSidToStringSidW(sid.as_ptr() as PSID, &mut text) } == 0 {
        return None;
    }
    let sid_text = wide_to_string(text);
    // SAFETY: `text` came from `ConvertSidToStringSidW`, which says to.
    unsafe { LocalFree(text.cast()) };

    let wide = |s: &str| -> Vec<u16> { s.encode_utf16().chain(std::iter::once(0)).collect() };
    let key = wide(&format!(
        r"SOFTWARE\Microsoft\Windows NT\CurrentVersion\ProfileList\{sid_text}"
    ));
    let value = wide("ProfileImagePath");
    let mut buf = vec![0u16; 1024];
    let mut bytes = (buf.len() * 2) as u32;
    // SAFETY: both names are NUL-terminated and `bytes` is `buf`'s size.
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ,
            std::ptr::null_mut(),
            buf.as_mut_ptr().cast(),
            &mut bytes,
        )
    };
    if status != ERROR_SUCCESS {
        return None;
    }
    let path = wide_to_string(buf.as_ptr());
    (!path.is_empty()).then_some(path)
}

/// A NUL-terminated wide string, bounded by the longest path Windows has.
fn wide_to_string(text: *const u16) -> String {
    if text.is_null() {
        return String::new();
    }
    // SAFETY: the callers pass NUL-terminated strings from the OS, and the read
    // stops at the terminator or the bound.
    unsafe {
        let len = (0..32_768).take_while(|&i| *text.add(i) != 0).count();
        String::from_utf16_lossy(std::slice::from_raw_parts(text, len))
    }
}

/// `GetUserProfileDirectoryW` for `token` — the profile root Windows itself
/// records for that user (`ProfileList\<SID>\ProfileImagePath`), however it
/// is named or redirected.
pub(crate) fn profile_dir_of_token(token: HANDLE) -> Option<String> {
    let mut len: u32 = 0;
    // SAFETY: the documented size query — a NULL buffer and a zero length;
    // it fails with ERROR_INSUFFICIENT_BUFFER and writes the size needed
    // (in WCHARs, terminator included) into `len`.
    let _ = unsafe { GetUserProfileDirectoryW(token, std::ptr::null_mut(), &mut len) };
    if len == 0 || len > 32_768 {
        return None;
    }
    let mut buf: Vec<u16> = vec![0; len as usize];
    // SAFETY: `buf` holds `len` WCHARs and outlives the call; the OS writes
    // at most `len` of them, terminator included.
    let ok = unsafe { GetUserProfileDirectoryW(token, buf.as_mut_ptr(), &mut len) };
    if ok == 0 {
        return None;
    }
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    let dir = String::from_utf16_lossy(&buf[..end]);
    (!dir.is_empty()).then_some(dir)
}

/// `S-1-5-18` in SID byte layout: revision 1, exactly one sub-authority,
/// the NT authority `{0,0,0,0,0,5}`, sub-authority[0] = 18 (little-endian).
/// Pure, so it is testable with synthetic buffers.
pub(crate) fn sid_bytes_are_local_system(sid: &[u8]) -> bool {
    sid.len() >= 12
        && sid[0] == 1
        && sid[1] == 1
        && sid[2..8] == [0, 0, 0, 0, 0, 5]
        && u32::from_le_bytes([sid[8], sid[9], sid[10], sid[11]]) == 18
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sid(revision: u8, count: u8, auth: [u8; 6], sub0: u32) -> Vec<u8> {
        let mut v = vec![revision, count];
        v.extend_from_slice(&auth);
        v.extend_from_slice(&sub0.to_le_bytes());
        v
    }

    /// #3229 — only a session this account has no shell in is someone else's.
    #[test]
    fn a_session_is_someone_elses_only_without_a_shell_of_ours() {
        let own = sid(1, 1, [0, 0, 0, 0, 0, 5], 1001);
        let person = sid(1, 1, [0, 0, 0, 0, 0, 5], 1002);
        let third = sid(1, 1, [0, 0, 0, 0, 0, 5], 1003);
        assert_eq!(someone_elses_shell(&[], &own), None, "no shell to read");
        assert_eq!(someone_elses_shell(std::slice::from_ref(&own), &own), None);
        assert_eq!(
            someone_elses_shell(std::slice::from_ref(&person), &own),
            Some(person.as_slice())
        );
        assert_eq!(
            someone_elses_shell(&[person.clone(), own.clone()], &own),
            None,
            "our own shell settles it"
        );
        assert_eq!(
            someone_elses_shell(&[person.clone(), third], &own),
            Some(person.as_slice())
        );
    }

    /// The real calls, in the session this test runs in: the shell here, when
    /// there is one, is this account's own.
    #[test]
    fn this_session_is_not_someone_elses() {
        assert_eq!(session_account(), SessionAccount::ThisOrUnknown);
    }

    /// The real `ProfileList`: every machine has SYSTEM's profile, and a SID with
    /// no profile has no answer.
    #[test]
    fn the_machine_names_a_profile_by_sid() {
        let system = sid(1, 1, [0, 0, 0, 0, 0, 5], 18);
        let profile = profile_of_sid(&system).expect("SYSTEM's profile");
        assert!(
            profile.to_ascii_lowercase().contains("systemprofile"),
            "{profile}"
        );
        assert_eq!(
            profile_of_sid(&sid(1, 1, [0, 0, 0, 0, 0, 5], 999_999)),
            None
        );
    }

    #[test]
    fn local_system_sid_is_recognised_and_nothing_else_is() {
        assert!(sid_bytes_are_local_system(&sid(
            1,
            1,
            [0, 0, 0, 0, 0, 5],
            18
        )));
        // LocalService / NetworkService share the authority, not the RID.
        assert!(!sid_bytes_are_local_system(&sid(
            1,
            1,
            [0, 0, 0, 0, 0, 5],
            19
        )));
        assert!(!sid_bytes_are_local_system(&sid(
            1,
            1,
            [0, 0, 0, 0, 0, 5],
            20
        )));
        // A domain / local user: S-1-5-21-…, several sub-authorities.
        assert!(!sid_bytes_are_local_system(&sid(
            1,
            5,
            [0, 0, 0, 0, 0, 5],
            21
        )));
        // Wrong authority, wrong revision, truncated.
        assert!(!sid_bytes_are_local_system(&sid(
            1,
            1,
            [0, 0, 0, 0, 0, 1],
            18
        )));
        assert!(!sid_bytes_are_local_system(&sid(
            2,
            1,
            [0, 0, 0, 0, 0, 5],
            18
        )));
        assert!(!sid_bytes_are_local_system(&[1, 1, 0, 0, 0, 0, 0, 5]));
        assert!(!sid_bytes_are_local_system(&[]));
    }

    #[test]
    fn the_test_runner_is_not_local_system() {
        // `cargo test` runs as the developer or the CI user; the read must
        // succeed (no panic) and say so.
        assert!(!process_is_local_system());
    }

    /// The token → profile half, on our OWN token: Windows' recorded
    /// profile for the test runner is its `%USERPROFILE%`. (The session
    /// half, `WTSQueryUserToken`, needs SYSTEM and is proven on a
    /// perMachine host — see FR-84's field log.)
    #[test]
    fn profile_of_our_own_token_is_our_userprofile() {
        let got = own_profile_dir();
        let want = std::env::var("USERPROFILE").expect("USERPROFILE is set for a test runner");
        assert!(
            got.as_deref()
                .is_some_and(|g| g.eq_ignore_ascii_case(want.trim_end_matches('\\'))),
            "{got:?} vs {want:?}"
        );
    }

    /// Not SYSTEM ⇒ `WTSQueryUserToken` refuses ⇒ no profile, never a
    /// guess — and never a panic.
    #[test]
    fn a_non_system_caller_gets_no_session_profile() {
        assert_eq!(session_user_profile_dir(), None);
    }

    /// FR-92 — with no session user's token to become, the closure does NOT
    /// run: a privileged writer that cannot act as the person must never act
    /// as itself instead. (A test runner is not SYSTEM, so the token is
    /// refused, exactly as when nobody is signed in.)
    #[test]
    fn without_the_session_users_token_nothing_runs() {
        assert!(!process_is_local_system());
        let mut ran = false;
        assert_eq!(as_session_user(|| ran = true), None);
        assert!(!ran);
    }
}
