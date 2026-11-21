// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! A process token's identity, on every Windows build: what a daemon that is
//! not SYSTEM hands to what it starts for a person, the restricted, medium
//! copy of its own token, and the reads it is built from.
//!
//! FR-85's recorder made it (`recording::launch::win`, where a recording is
//! never made elevated). FR-90 P1i-2 moved it here, unchanged, because an
//! agent session on Windows needs the same copy and a build without
//! `recording` has to have it too. The recorder still imports it from here.

#![cfg(target_os = "windows")]

use windows_sys::Win32::Foundation::{
    CloseHandle, GENERIC_ALL, GetLastError, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACL, ACL_REVISION, AddAccessAllowedAce, CheckTokenMembership,
    CreateRestrictedToken, CreateWellKnownSid, DISABLE_MAX_PRIVILEGE, GetLengthSid,
    GetSidIdentifierAuthority, GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation,
    InitializeAcl, PSID, SID_AND_ATTRIBUTES, SetTokenInformation, TOKEN_ADJUST_DEFAULT,
    TOKEN_ASSIGN_PRIMARY, TOKEN_DEFAULT_DACL, TOKEN_DUPLICATE, TOKEN_GROUPS, TOKEN_IMPERSONATE,
    TOKEN_INFORMATION_CLASS, TOKEN_MANDATORY_LABEL, TOKEN_OWNER, TOKEN_QUERY, TOKEN_USER,
    TokenDefaultDacl, TokenGroups, TokenIntegrityLevel, TokenOwner, TokenUser, WELL_KNOWN_SID_TYPE,
    WinBuiltinAdministratorsSid, WinLocalSystemSid, WinMediumLabelSid,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// `SECURITY_MANDATORY_MEDIUM_RID`.
pub const MEDIUM_RID: u32 = 0x2000;
/// `SE_GROUP_INTEGRITY`.
const SE_GROUP_INTEGRITY: u32 = 0x20;
/// Longest SID `CreateWellKnownSid` writes (`SECURITY_MAX_SID_SIZE`).
const MAX_SID: usize = 68;

/// A kernel handle closed on drop.
pub struct Owned(pub(crate) HANDLE);

// SAFETY: a token or pipe handle is a process-wide reference to a kernel
// object, usable from any thread.
unsafe impl Send for Owned {}
unsafe impl Sync for Owned {}

impl Owned {
    pub fn new(h: HANDLE) -> Option<Self> {
        (!h.is_null() && h != INVALID_HANDLE_VALUE).then_some(Self(h))
    }
    pub fn raw(&self) -> HANDLE {
        self.0
    }
    /// Give the handle up (to a `File`, which closes it instead).
    pub fn into_raw(self) -> HANDLE {
        let h = self.0;
        std::mem::forget(self);
        h
    }
}

impl Drop for Owned {
    fn drop(&mut self) {
        // SAFETY: we own the handle and close it once.
        unsafe { CloseHandle(self.0) };
    }
}

pub fn last_error(what: &str) -> String {
    // SAFETY: a thread-local read.
    format!("{what} failed (win32 error {})", unsafe { GetLastError() })
}

/// A token's variable-length information, in a buffer aligned for the
/// pointers inside it.
pub(crate) fn token_info(token: HANDLE, class: TOKEN_INFORMATION_CLASS) -> Option<Vec<u64>> {
    let mut len: u32 = 0;
    // SAFETY: a size query; a null buffer of length 0 is the documented
    // form.
    unsafe { GetTokenInformation(token, class, std::ptr::null_mut(), 0, &mut len) };
    if len == 0 {
        return None;
    }
    let mut buf = vec![0u64; (len as usize).div_ceil(8)];
    // SAFETY: `buf` holds at least `len` bytes.
    let ok = unsafe { GetTokenInformation(token, class, buf.as_mut_ptr().cast(), len, &mut len) };
    (ok != 0).then_some(buf)
}

pub fn own_token(access: u32) -> Option<Owned> {
    let mut t: HANDLE = std::ptr::null_mut();
    // SAFETY: the pseudo-handle of this process and a valid out-param.
    if unsafe { OpenProcessToken(GetCurrentProcess(), access, &mut t) } == 0 {
        return None;
    }
    Owned::new(t)
}

/// The integrity RID of a token's label (0x2000 medium, 0x3000 high,
/// 0x4000 system).
pub(crate) fn integrity_rid(token: HANDLE) -> Option<u32> {
    let buf = token_info(token, TokenIntegrityLevel)?;
    // SAFETY: on success the buffer starts with a TOKEN_MANDATORY_LABEL
    // whose SID points into the same buffer.
    unsafe {
        let label = &*(buf.as_ptr() as *const TOKEN_MANDATORY_LABEL);
        let n = *GetSidSubAuthorityCount(label.Label.Sid);
        if n == 0 {
            return None;
        }
        Some(*GetSidSubAuthority(label.Label.Sid, u32::from(n) - 1))
    }
}

/// This process's integrity RID. `None` when it cannot be read, which
/// the decision treats as ABOVE medium: a restricted copy of an ordinary
/// token is still an ordinary token, while an elevated recorder is the
/// thing the rule forbids.
pub fn own_integrity_rid() -> Option<u32> {
    let t = own_token(TOKEN_QUERY)?;
    integrity_rid(t.raw())
}

/// `S-1-5-…` for a SID.
pub(crate) fn sid_string(sid: PSID) -> String {
    // SAFETY: `sid` is a valid SID from a token.
    unsafe {
        let auth = (*GetSidIdentifierAuthority(sid)).Value;
        let authority = auth.iter().fold(0u64, |acc, b| (acc << 8) | u64::from(*b));
        let n = *GetSidSubAuthorityCount(sid);
        let mut s = format!("S-1-{authority}");
        for i in 0..u32::from(n) {
            s.push_str(&format!("-{}", *GetSidSubAuthority(sid, i)));
        }
        s
    }
}

/// This process's user, as a SID string.
pub fn own_user_sid() -> Option<String> {
    // SAFETY: the pseudo-handle of this process, valid for its whole life.
    user_sid_of(unsafe { GetCurrentProcess() })
}

/// #3885 — the user of the process behind `process` (a handle with at least
/// `PROCESS_QUERY_LIMITED_INFORMATION`), as a SID string, from its token.
/// `None` when its token cannot be opened or read.
pub(crate) fn user_sid_of(process: HANDLE) -> Option<String> {
    let mut t: HANDLE = std::ptr::null_mut();
    // SAFETY: a process handle the caller holds and a valid out-param.
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut t) } == 0 {
        return None;
    }
    let t = Owned::new(t)?;
    let buf = token_info(t.raw(), TokenUser)?;
    // SAFETY: a TOKEN_USER whose SID points into `buf`.
    Some(sid_string(unsafe {
        (*(buf.as_ptr() as *const TOKEN_USER)).User.Sid
    }))
}

/// Is BUILTIN\Administrators ENABLED in this process's token (not merely
/// present deny-only, as it is in a filtered or restricted token)?
pub fn admin_group_enabled() -> Option<bool> {
    let mut sid = [0u8; MAX_SID];
    let mut size = MAX_SID as u32;
    let mut member = 0;
    // SAFETY: `sid` holds SECURITY_MAX_SID_SIZE bytes; a null token means
    // "the calling thread's effective token".
    unsafe {
        if CreateWellKnownSid(
            WinBuiltinAdministratorsSid,
            std::ptr::null_mut(),
            sid.as_mut_ptr().cast(),
            &mut size,
        ) == 0
        {
            return None;
        }
        if CheckTokenMembership(std::ptr::null_mut(), sid.as_mut_ptr().cast(), &mut member) == 0 {
            return None;
        }
    }
    Some(member != 0)
}

/// Is this group one of the admin-equivalent ones the UAC filter disables?
///
/// # Safety
/// `sid` must be a valid SID.
unsafe fn admin_equivalent(sid: PSID) -> bool {
    // SAFETY: forwarded; every sub-authority read is bounded by the count.
    unsafe {
        if (*GetSidIdentifierAuthority(sid)).Value != [0, 0, 0, 0, 0, 5] {
            return false;
        }
        let n = u32::from(*GetSidSubAuthorityCount(sid));
        let sub = |i: u32| *GetSidSubAuthority(sid, i);
        match n {
            // S-1-5-114: local account and member of Administrators.
            1 => sub(0) == 114,
            // BUILTIN: Administrators, Power Users, Account / Server /
            // Print / Backup Operators, Replicator, Network Configuration
            // and Cryptographic Operators, Hyper-V Administrators.
            2 => {
                sub(0) == 32
                    && matches!(
                        sub(1),
                        544 | 547 | 548 | 549 | 550 | 551 | 552 | 556 | 569 | 578
                    )
            }
            // S-1-5-21-a-b-c-RID: the domain's (or machine's) admin groups.
            5 => {
                sub(0) == 21
                    && matches!(sub(4), 498 | 512 | 517 | 518 | 519 | 520 | 521 | 526 | 527)
            }
            _ => false,
        }
    }
}

/// A restricted, medium-integrity copy of this process's own token: the
/// same user, every admin-equivalent group deny-only, every privilege
/// but traverse removed, owner and default DACL made the user's.
pub fn restricted_medium_copy() -> Result<Owned, String> {
    let access = TOKEN_DUPLICATE
        | TOKEN_QUERY
        | TOKEN_ASSIGN_PRIMARY
        | TOKEN_ADJUST_DEFAULT
        | TOKEN_IMPERSONATE;
    let own = own_token(access).ok_or_else(|| last_error("OpenProcessToken"))?;
    let groups_buf = token_info(own.raw(), TokenGroups)
        .ok_or_else(|| last_error("reading the token's groups"))?;
    // SAFETY: a TOKEN_GROUPS whose array and SIDs live in `groups_buf`,
    // which outlives `deny` and the CreateRestrictedToken call.
    let deny: Vec<SID_AND_ATTRIBUTES> = unsafe {
        let groups = &*(groups_buf.as_ptr() as *const TOKEN_GROUPS);
        std::slice::from_raw_parts(groups.Groups.as_ptr(), groups.GroupCount as usize)
            .iter()
            .filter(|g| g.Attributes & SE_GROUP_INTEGRITY == 0 && admin_equivalent(g.Sid))
            .map(|g| SID_AND_ATTRIBUTES {
                Sid: g.Sid,
                Attributes: 0,
            })
            .collect()
    };
    let mut out: HANDLE = std::ptr::null_mut();
    // SAFETY: `deny` points at SIDs inside `groups_buf`, alive here; no
    // privileges listed (DISABLE_MAX_PRIVILEGE removes them) and no
    // restricting SIDs.
    let ok = unsafe {
        CreateRestrictedToken(
            own.raw(),
            DISABLE_MAX_PRIVILEGE,
            deny.len() as u32,
            deny.as_ptr(),
            0,
            std::ptr::null(),
            0,
            std::ptr::null(),
            &mut out,
        )
    };
    if ok == 0 {
        return Err(last_error("CreateRestrictedToken"));
    }
    let token =
        Owned::new(out).ok_or_else(|| "CreateRestrictedToken returned no token".to_string())?;

    set_medium_integrity(token.raw())?;

    let user_buf =
        token_info(token.raw(), TokenUser).ok_or_else(|| last_error("reading the token's user"))?;
    // SAFETY: a TOKEN_USER whose SID lives in `user_buf`, alive for both
    // calls below.
    let user_sid = unsafe { (*(user_buf.as_ptr() as *const TOKEN_USER)).User.Sid };
    // An elevated token's objects are owned by Administrators by default,
    // which is deny-only now and so can own nothing.
    let owner = TOKEN_OWNER { Owner: user_sid };
    // SAFETY: `owner` and the SID it names outlive the call.
    if unsafe {
        SetTokenInformation(
            token.raw(),
            TokenOwner,
            (&owner as *const TOKEN_OWNER).cast(),
            std::mem::size_of::<TOKEN_OWNER>() as u32,
        )
    } == 0
    {
        return Err(last_error("setting the restricted token's owner"));
    }
    set_default_dacl(token.raw(), user_sid)?;
    Ok(token)
}

pub fn well_known_sid(kind: WELL_KNOWN_SID_TYPE) -> Result<[u8; MAX_SID], String> {
    let mut sid = [0u8; MAX_SID];
    let mut size = MAX_SID as u32;
    // SAFETY: `sid` holds SECURITY_MAX_SID_SIZE bytes.
    if unsafe {
        CreateWellKnownSid(
            kind,
            std::ptr::null_mut(),
            sid.as_mut_ptr().cast(),
            &mut size,
        )
    } == 0
    {
        return Err(last_error("CreateWellKnownSid"));
    }
    Ok(sid)
}

fn set_medium_integrity(token: HANDLE) -> Result<(), String> {
    set_integrity(token, WinMediumLabelSid, "medium")
}

/// Lower `token`'s integrity to the level `label` names.
fn set_integrity(token: HANDLE, label: WELL_KNOWN_SID_TYPE, level: &str) -> Result<(), String> {
    let mut sid = well_known_sid(label)?;
    let label = TOKEN_MANDATORY_LABEL {
        Label: SID_AND_ATTRIBUTES {
            Sid: sid.as_mut_ptr().cast(),
            Attributes: SE_GROUP_INTEGRITY,
        },
    };
    // SAFETY: `label` and the SID it names outlive the call; lowering a
    // token's own integrity needs no privilege.
    let ok = unsafe {
        SetTokenInformation(
            token,
            TokenIntegrityLevel,
            (&label as *const TOKEN_MANDATORY_LABEL).cast(),
            std::mem::size_of::<TOKEN_MANDATORY_LABEL>() as u32
                + GetLengthSid(sid.as_mut_ptr().cast()),
        )
    };
    if ok == 0 {
        return Err(last_error(&format!(
            "lowering the token to {level} integrity"
        )));
    }
    Ok(())
}

/// #3048 — an impersonation copy of this process's own token at LOW
/// integrity: the same user, writing only where low integrity may. The file
/// tests stand it in for the person a SYSTEM worker writes as: a folder this
/// process can write and the copy cannot is one the "person" may not write.
#[cfg(test)]
pub fn low_integrity_copy() -> Result<Owned, String> {
    use windows_sys::Win32::Security::{
        DuplicateTokenEx, SecurityImpersonation, TokenImpersonation, WinLowLabelSid,
    };
    let own =
        own_token(TOKEN_DUPLICATE | TOKEN_QUERY).ok_or_else(|| last_error("OpenProcessToken"))?;
    let mut out: HANDLE = std::ptr::null_mut();
    // SAFETY: a live token opened for duplication; null attributes take the
    // default descriptor; the out-handle is checked below.
    let ok = unsafe {
        DuplicateTokenEx(
            own.raw(),
            TOKEN_QUERY | TOKEN_IMPERSONATE | TOKEN_ADJUST_DEFAULT | TOKEN_DUPLICATE,
            std::ptr::null(),
            SecurityImpersonation,
            TokenImpersonation,
            &mut out,
        )
    };
    if ok == 0 {
        return Err(last_error("DuplicateTokenEx"));
    }
    let token = Owned::new(out).ok_or_else(|| "DuplicateTokenEx returned no token".to_string())?;
    set_integrity(token.raw(), WinLowLabelSid, "low")?;
    Ok(token)
}

/// The DACL of objects the recorder creates without inheriting one — its
/// own process among them: the user and SYSTEM. An elevated token's
/// default grants Administrators, which is deny-only in the copy, and a
/// recorder that cannot open its own process breaks in ways that do not
/// name the cause.
fn set_default_dacl(token: HANDLE, user_sid: PSID) -> Result<(), String> {
    let mut system = well_known_sid(WinLocalSystemSid)?;
    // SAFETY: both SIDs are valid.
    let sid_len = unsafe { GetLengthSid(user_sid) + GetLengthSid(system.as_mut_ptr().cast()) };
    let size = std::mem::size_of::<ACL>() as u32
        + 2 * (std::mem::size_of::<ACCESS_ALLOWED_ACE>() as u32 - 4)
        + sid_len;
    let mut buf = vec![0u64; (size as usize).div_ceil(8)];
    let acl = buf.as_mut_ptr() as *mut ACL;
    // SAFETY: `buf` holds `size` bytes, sized for exactly the two ACEs.
    unsafe {
        if InitializeAcl(acl, size, ACL_REVISION) == 0
            || AddAccessAllowedAce(acl, ACL_REVISION, GENERIC_ALL, user_sid) == 0
            || AddAccessAllowedAce(acl, ACL_REVISION, GENERIC_ALL, system.as_mut_ptr().cast()) == 0
        {
            return Err(last_error("building the restricted token's default DACL"));
        }
    }
    let dacl = TOKEN_DEFAULT_DACL { DefaultDacl: acl };
    // SAFETY: `dacl` and the ACL it points at outlive the call.
    if unsafe {
        SetTokenInformation(
            token,
            TokenDefaultDacl,
            (&dacl as *const TOKEN_DEFAULT_DACL).cast(),
            std::mem::size_of::<TOKEN_DEFAULT_DACL>() as u32,
        )
    } == 0
    {
        return Err(last_error("setting the restricted token's default DACL"));
    }
    Ok(())
}

/// FR-150 P1a — the file rights a replace and a delete of a scheduled task
/// take on the task's file: `FILE_GENERIC_WRITE | DELETE`.
const REPLACE_AND_DELETE: u32 = 0x0012_0116 | 0x0001_0000;

/// The file generic mapping: `FILE_GENERIC_READ`, `FILE_GENERIC_WRITE`,
/// `FILE_GENERIC_EXECUTE` and `FILE_ALL_ACCESS`.
const FILE_MAPPING: windows_sys::Win32::Security::GENERIC_MAPPING =
    windows_sys::Win32::Security::GENERIC_MAPPING {
        GenericRead: 0x0012_0089,
        GenericWrite: 0x0012_0116,
        GenericExecute: 0x0012_00A0,
        GenericAll: 0x001F_01FF,
    };

/// A security descriptor the system allocated, freed on drop.
struct LocalDescriptor(windows_sys::Win32::Security::PSECURITY_DESCRIPTOR);

impl Drop for LocalDescriptor {
    fn drop(&mut self) {
        // SAFETY: allocated by the call that filled it, freed once.
        unsafe { windows_sys::Win32::Foundation::LocalFree(self.0 as _) };
    }
}

/// FR-150 P1a — may a medium copy of this process's token
/// ([`restricted_medium_copy`]) write and delete the file at `path`, by the
/// file's own security descriptor? A per-user process asks it of its task's
/// file under `System32\Tasks`: Task Scheduler refuses a replace or a delete of
/// the task from a medium token exactly when this answers no (#4128's P0, C6
/// and C3c), and that file is the copy of the task's descriptor a medium
/// process may read. An elevated process gets the answer for the account
/// behind it, which is what its next self-update runs with.
pub fn medium_may_write_and_delete(path: &std::path::Path) -> Result<bool, String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT};
    use windows_sys::Win32::Security::{
        DACL_SECURITY_INFORMATION, GROUP_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION,
    };
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut sd = std::ptr::null_mut();
    // SAFETY: a NUL-terminated path and out-params; the descriptor is freed
    // by the guard below.
    let rc = unsafe {
        GetNamedSecurityInfoW(
            wide.as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | GROUP_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut sd,
        )
    };
    if rc != 0 {
        return Err(format!(
            "reading the security of {} failed (win32 error {rc})",
            path.display()
        ));
    }
    let sd = LocalDescriptor(sd);
    medium_access_check(sd.0, REPLACE_AND_DELETE)
}

/// [`medium_may_write_and_delete`] over a descriptor given as SDDL, which
/// names an owner, a group and a DACL.
pub fn medium_may_write_and_delete_sddl(sddl: &str) -> Result<bool, String> {
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    let wide: Vec<u16> = sddl.encode_utf16().chain(Some(0)).collect();
    let mut sd = std::ptr::null_mut();
    // SAFETY: a NUL-terminated SDDL string and an out-param; the descriptor
    // is freed by the guard below.
    let ok = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            wide.as_ptr(),
            SDDL_REVISION_1,
            &mut sd,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(last_error("parsing the SDDL"));
    }
    let sd = LocalDescriptor(sd);
    medium_access_check(sd.0, REPLACE_AND_DELETE)
}

/// `AccessCheck` of a medium copy of this process's token against `sd`, for
/// all of `desired`, under the file generic mapping.
fn medium_access_check(
    sd: windows_sys::Win32::Security::PSECURITY_DESCRIPTOR,
    desired: u32,
) -> Result<bool, String> {
    use windows_sys::Win32::Security::{AccessCheck, DuplicateToken, SecurityIdentification};
    let primary = restricted_medium_copy()?;
    let mut imp: HANDLE = std::ptr::null_mut();
    // SAFETY: a token this process holds with TOKEN_DUPLICATE, and an
    // out-param.
    if unsafe { DuplicateToken(primary.raw(), SecurityIdentification, &mut imp) } == 0 {
        return Err(last_error("DuplicateToken"));
    }
    let imp = Owned::new(imp).ok_or_else(|| "DuplicateToken returned no token".to_string())?;
    // A PRIVILEGE_SET with room to spare: the medium copy holds no privilege
    // an access check could use.
    let mut privileges = [0u64; 32];
    let mut privileges_len = std::mem::size_of_val(&privileges) as u32;
    let mut granted = 0u32;
    let mut status = 0;
    // SAFETY: a descriptor with an owner, a group and a DACL, an
    // impersonation token with TOKEN_QUERY, and buffers that outlive the call.
    let ok = unsafe {
        AccessCheck(
            sd,
            imp.raw(),
            desired,
            &FILE_MAPPING,
            privileges.as_mut_ptr().cast(),
            &mut privileges_len,
            &mut granted,
            &mut status,
        )
    };
    if ok == 0 {
        return Err(last_error("AccessCheck"));
    }
    Ok(status != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FR-150 P1a — the access check answers as Task Scheduler did in #4128's
    /// P0, over the descriptors it read: yes for a task registered at medium
    /// (C1) and for P1a's own descriptor (C3, C3b, C3c), no for a task
    /// registered elevated (C2, #3507), whose account only reads it. Whatever
    /// token runs the test: the medium copy holds Administrators deny-only.
    #[test]
    fn a_task_the_account_cannot_replace_is_named_at_start() {
        let me = own_user_sid().expect("this process's SID");
        if me == "S-1-5-18" {
            // SYSTEM's own entries grant it the write: not a per-user process.
            return;
        }
        let medium = format!(
            "O:{me}G:SYD:(A;ID;0x1f019f;;;BA)(A;ID;0x1f019f;;;SY)(A;ID;FA;;;{me})(A;;FR;;;{me})"
        );
        let own = format!("O:{me}G:SYD:(A;;FA;;;SY)(A;;FA;;;BA)(A;;FA;;;{me})");
        let elevated = format!(
            "O:BAG:SYD:(A;ID;0x1f019f;;;BA)(A;ID;0x1f019f;;;SY)(A;ID;FA;;;BA)(A;;FR;;;{me})"
        );
        assert_eq!(medium_may_write_and_delete_sddl(&medium), Ok(true));
        assert_eq!(medium_may_write_and_delete_sddl(&own), Ok(true));
        assert_eq!(medium_may_write_and_delete_sddl(&elevated), Ok(false));
        // Write without delete is not enough: a replace that cannot delete is
        // refused.
        let no_delete = format!("O:BAG:SYD:(A;;0x120116;;;{me})");
        assert_eq!(medium_may_write_and_delete_sddl(&no_delete), Ok(false));
        assert!(medium_may_write_and_delete_sddl("not SDDL").is_err());
    }

    /// The check reads a real file's descriptor: one this account made in its
    /// own temp folder is its own to write and delete.
    #[test]
    fn the_medium_check_reads_a_files_own_descriptor() {
        let dir = tempfile::tempdir().expect("temp dir");
        let file = dir.path().join("task");
        std::fs::write(&file, b"<Task/>").unwrap();
        assert_eq!(medium_may_write_and_delete(&file), Ok(true));
        assert!(medium_may_write_and_delete(&dir.path().join("absent")).is_err());
    }
}
