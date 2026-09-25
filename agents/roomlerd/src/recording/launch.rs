// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-85 P1e — who the recorder runs as, and launching it as that.
//!
//! **The identity rule.** Whenever a person is signed in at the device, the
//! recorder runs as THAT person at normal integrity: never SYSTEM, never root,
//! never elevated. A recording is theirs. It goes into their folder, as them,
//! where they can find it and where no one else's rights reach.
//!
//! | This daemon runs as | The recorder runs as |
//! |---|---|
//! | an ordinary user at normal integrity (a per-user install, a user unit) | the same: nothing to change |
//! | an ELEVATED user: the default Windows service worker of a UAC-split administrator (`ROOMLERD_ELEVATE_WORKER`) | a restricted copy of the same token: admin groups deny-only, no privileges, medium integrity |
//! | SYSTEM, with someone signed in at the console | that person's own token |
//! | SYSTEM with nobody signed in, or root | refused, by name |
//!
//! ⚠️ **A restricted COPY, not the linked token.** An elevated administrator's
//! token links to the filtered one, but without `SeTcbPrivilege` (a worker has
//! none) `TokenLinkedToken` hands back an IDENTIFICATION-level token. That kind
//! can be neither impersonated nor launched with. A restricted token made from
//! this process's OWN token is a child of it, which `CreateProcessAsUserW`
//! accepts without `SeAssignPrimaryTokenPrivilege`.
//!
//! ⚠️ **The daemon touches the folder the way the recorder does.** Listing,
//! downloading and deleting recordings run on a thread that impersonates the
//! same token ([`as_identity`]). The folder is the user's to rearrange. A
//! junction or a hard link planted in it would otherwise be followed with
//! SYSTEM's rights, or the elevated token's, and an elevated reader or deleter
//! in a user-writable folder is exactly the medium-to-high primitive this rule
//! exists to avoid. Impersonated, every open is checked against the user's own
//! rights, wherever the path turns out to lead.
//!
//! Not yet here: the unix drop to the console user (a root daemon still
//! refuses), and the unattended exception (a device with nobody signed in
//! records as the daemon, into the daemon's own folder).

use std::ffi::OsString;
use std::path::Path;

/// Who a recorder launched now would run as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Identity {
    /// This daemon's own identity is already an ordinary user at normal
    /// integrity.
    Inherit,
    /// (Windows) A restricted, medium-integrity copy of this daemon's own,
    /// elevated, token.
    RestrictedCopy,
    /// (Windows) The token of the person signed in at console session
    /// `session`. This daemon is SYSTEM.
    ConsoleUser { session: u32 },
}

/// Why no recorder can be launched here right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// SYSTEM, and nobody is signed in at the console.
    NoConsoleUser,
    /// root on Linux or macOS: launching the recorder as the person at the
    /// screen is built on Windows first.
    RootDaemon,
}

impl Refusal {
    /// The sentence a client shows (the state's `unavailable_reason`, a
    /// refused start).
    pub fn message(self) -> &'static str {
        match self {
            Self::NoConsoleUser => {
                "this device service runs as SYSTEM and nobody is signed in at its screen: a \
                 recording is made as the person signed in at the device, so there is no one to \
                 record as"
            }
            Self::RootDaemon => {
                "this device service runs as root, and launching the recorder as the person at \
                 the screen is not built here yet (FR-85 P1e covers Windows): run `roomlerd \
                 record` in your own session"
            }
        }
    }
}

/// What the decision reads. Split out so its table is a unit test on every
/// platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Facts {
    pub windows: bool,
    /// SYSTEM on Windows, root on unix.
    pub service_account: bool,
    /// (Windows) this process's integrity is above medium.
    pub above_medium: bool,
    /// (Windows, SYSTEM only) the console session, when a person is signed in
    /// there and their token can be had.
    pub console_session: Option<u32>,
}

/// The identity rule, as a table.
pub fn decide_from(f: Facts) -> Result<Identity, Refusal> {
    if f.service_account {
        if !f.windows {
            return Err(Refusal::RootDaemon);
        }
        return match f.console_session {
            Some(session) => Ok(Identity::ConsoleUser { session }),
            None => Err(Refusal::NoConsoleUser),
        };
    }
    if f.windows && f.above_medium {
        return Ok(Identity::RestrictedCopy);
    }
    Ok(Identity::Inherit)
}

/// Read the facts for THIS process, now.
pub fn facts() -> Facts {
    #[cfg(windows)]
    {
        let service_account = crate::win_identity::process_is_local_system();
        Facts {
            windows: true,
            service_account,
            above_medium: win::own_integrity_rid().is_none_or(|rid| rid > win::MEDIUM_RID),
            console_session: if service_account {
                win::signed_in_console_session()
            } else {
                None
            },
        }
    }
    #[cfg(unix)]
    {
        Facts {
            windows: false,
            // SAFETY: `geteuid` reads the caller's own credentials; it cannot
            // fail.
            service_account: unsafe { libc::geteuid() } == 0,
            above_medium: false,
            console_session: None,
        }
    }
    #[cfg(not(any(windows, unix)))]
    {
        Facts {
            windows: false,
            service_account: true,
            above_medium: false,
            console_session: None,
        }
    }
}

/// Who a recorder launched now would run as, or why none can be.
pub fn decide() -> Result<Identity, Refusal> {
    decide_from(facts())
}

/// What `roomlerd record --whoami` prints: the identity a recorder actually
/// got, for the tests and for a field question ("what does it run as here?").
pub fn describe_self() -> serde_json::Value {
    #[cfg(windows)]
    {
        serde_json::json!({
            "user": win::own_user_sid(),
            "integrity_rid": win::own_integrity_rid(),
            "admin_enabled": win::admin_group_enabled(),
            "system": crate::win_identity::process_is_local_system(),
        })
    }
    #[cfg(unix)]
    {
        // SAFETY: both read the caller's own credentials; they cannot fail.
        let (uid, euid) = unsafe { (libc::getuid(), libc::geteuid()) };
        serde_json::json!({ "uid": uid, "euid": euid })
    }
    #[cfg(not(any(windows, unix)))]
    {
        serde_json::json!({})
    }
}

/// A launched recorder: its pipes and its process.
pub struct Launched {
    pub stdin: Option<Box<dyn tokio::io::AsyncWrite + Send + Unpin>>,
    pub stdout: Box<dyn tokio::io::AsyncRead + Send + Unpin>,
    pub child: Child,
}

/// The recorder's process, however it was launched.
pub enum Child {
    Tokio(Box<tokio::process::Child>),
    #[cfg(windows)]
    Win(std::sync::Arc<crate::win_service::supervisor::OwnedProcess>),
}

impl Child {
    /// Wait for the process to exit. Cancel-safe: dropping the future leaves
    /// the process alone.
    pub async fn wait(&mut self) {
        match self {
            Self::Tokio(c) => {
                let _ = c.wait().await;
            }
            #[cfg(windows)]
            // Polled rather than a blocking wait, so a caller's timeout never
            // strands a pool thread waiting on a recorder that outlives it.
            Self::Win(p) => loop {
                match p.try_wait() {
                    Ok(None) => tokio::time::sleep(std::time::Duration::from_millis(50)).await,
                    _ => return,
                }
            },
        }
    }

    /// Kill the process (a recorder that missed its start deadline).
    pub fn start_kill(&mut self) {
        match self {
            Self::Tokio(c) => {
                let _ = c.start_kill();
            }
            #[cfg(windows)]
            Self::Win(p) => p.terminate(),
        }
    }
}

/// Launch `exe args` as `identity`, with `env` added to its environment,
/// stdin and stdout piped.
pub fn spawn(
    identity: Identity,
    exe: &Path,
    args: &[OsString],
    env: &[(String, String)],
) -> std::io::Result<Launched> {
    match identity {
        Identity::Inherit => spawn_inherit(exe, args, env),
        #[cfg(windows)]
        Identity::RestrictedCopy | Identity::ConsoleUser { .. } => {
            win::spawn_as(identity, exe, args, env)
        }
        #[cfg(not(windows))]
        _ => Err(std::io::Error::other(
            "that identity is a Windows one and this is not Windows",
        )),
    }
}

fn spawn_inherit(
    exe: &Path,
    args: &[OsString],
    env: &[(String, String)],
) -> std::io::Result<Launched> {
    use std::process::Stdio;
    let mut cmd = tokio::process::Command::new(exe);
    cmd.args(args)
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(false);
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = cmd.spawn()?;
    let stdin = child
        .stdin
        .take()
        .map(|s| Box::new(s) as Box<dyn tokio::io::AsyncWrite + Send + Unpin>);
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::other("the recorder has no stdout"))?;
    Ok(Launched {
        stdin,
        stdout: Box::new(stdout),
        child: Child::Tokio(Box::new(child)),
    })
}

/// Run `f` on THIS thread as `identity`, then return to this process's own.
/// Blocking work only: the caller is on a blocking thread (`spawn_blocking`),
/// never an async worker, because the identity belongs to the thread.
///
/// `Inherit` just runs `f`.
pub fn as_identity<R>(identity: Identity, f: impl FnOnce() -> R) -> Result<R, String> {
    match identity {
        Identity::Inherit => Ok(f()),
        #[cfg(windows)]
        Identity::RestrictedCopy | Identity::ConsoleUser { .. } => {
            let token = win::token_for(identity)?;
            let _as_them = win::Impersonating::begin(&token)?;
            Ok(f())
        }
        #[cfg(not(windows))]
        _ => Err("that identity is a Windows one and this is not Windows".into()),
    }
}

#[cfg(windows)]
pub(crate) mod win {
    //! The Windows half: the restricted copy, the console user's token, the
    //! launch with a bounded handle list, and impersonation.

    use std::ffi::{OsStr, OsString, c_void};
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::os::windows::io::FromRawHandle;
    use std::path::Path;
    use std::sync::Arc;

    use windows_sys::Win32::Foundation::{
        CloseHandle, GENERIC_ALL, GetLastError, HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE,
        SetHandleInformation, TRUE,
    };
    use windows_sys::Win32::Security::{
        ACCESS_ALLOWED_ACE, ACL, ACL_REVISION, AddAccessAllowedAce, CheckTokenMembership,
        CreateRestrictedToken, CreateWellKnownSid, DISABLE_MAX_PRIVILEGE, GetLengthSid,
        GetSidIdentifierAuthority, GetSidSubAuthority, GetSidSubAuthorityCount,
        GetTokenInformation, ImpersonateLoggedOnUser, InitializeAcl, PSID, RevertToSelf,
        SECURITY_ATTRIBUTES, SID_AND_ATTRIBUTES, SetTokenInformation, TOKEN_ADJUST_DEFAULT,
        TOKEN_ASSIGN_PRIMARY, TOKEN_DEFAULT_DACL, TOKEN_DUPLICATE, TOKEN_GROUPS, TOKEN_IMPERSONATE,
        TOKEN_INFORMATION_CLASS, TOKEN_MANDATORY_LABEL, TOKEN_OWNER, TOKEN_QUERY, TOKEN_USER,
        TokenDefaultDacl, TokenGroups, TokenIntegrityLevel, TokenOwner, TokenUser,
        WELL_KNOWN_SID_TYPE, WinBuiltinAdministratorsSid, WinLocalSystemSid, WinMediumLabelSid,
    };
    use windows_sys::Win32::System::Pipes::CreatePipe;
    use windows_sys::Win32::System::Threading::{
        CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW,
        DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT, GetCurrentProcess,
        InitializeProcThreadAttributeList, OpenProcessToken, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
        PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOEXW, UpdateProcThreadAttribute,
    };

    use super::{Child, Identity, Launched};
    use crate::win_service::supervisor::{self, OwnedProcess};

    /// `SECURITY_MANDATORY_MEDIUM_RID`.
    pub const MEDIUM_RID: u32 = 0x2000;
    /// `SE_GROUP_INTEGRITY`.
    const SE_GROUP_INTEGRITY: u32 = 0x20;
    /// Longest SID `CreateWellKnownSid` writes (`SECURITY_MAX_SID_SIZE`).
    const MAX_SID: usize = 68;

    /// A kernel handle closed on drop.
    pub struct Owned(HANDLE);

    // SAFETY: a token or pipe handle is a process-wide reference to a kernel
    // object, usable from any thread.
    unsafe impl Send for Owned {}
    unsafe impl Sync for Owned {}

    impl Owned {
        fn new(h: HANDLE) -> Option<Self> {
            (!h.is_null() && h != INVALID_HANDLE_VALUE).then_some(Self(h))
        }
        pub fn raw(&self) -> HANDLE {
            self.0
        }
        /// Give the handle up (to a `File`, which closes it instead).
        fn into_raw(self) -> HANDLE {
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

    fn last_error(what: &str) -> String {
        // SAFETY: a thread-local read.
        format!("{what} failed (win32 error {})", unsafe { GetLastError() })
    }

    /// A token's variable-length information, in a buffer aligned for the
    /// pointers inside it.
    fn token_info(token: HANDLE, class: TOKEN_INFORMATION_CLASS) -> Option<Vec<u64>> {
        let mut len: u32 = 0;
        // SAFETY: a size query; a null buffer of length 0 is the documented
        // form.
        unsafe { GetTokenInformation(token, class, std::ptr::null_mut(), 0, &mut len) };
        if len == 0 {
            return None;
        }
        let mut buf = vec![0u64; (len as usize).div_ceil(8)];
        // SAFETY: `buf` holds at least `len` bytes.
        let ok =
            unsafe { GetTokenInformation(token, class, buf.as_mut_ptr().cast(), len, &mut len) };
        (ok != 0).then_some(buf)
    }

    fn own_token(access: u32) -> Option<Owned> {
        let mut t: HANDLE = std::ptr::null_mut();
        // SAFETY: the pseudo-handle of this process and a valid out-param.
        if unsafe { OpenProcessToken(GetCurrentProcess(), access, &mut t) } == 0 {
            return None;
        }
        Owned::new(t)
    }

    /// The integrity RID of a token's label (0x2000 medium, 0x3000 high,
    /// 0x4000 system).
    fn integrity_rid(token: HANDLE) -> Option<u32> {
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
    fn sid_string(sid: PSID) -> String {
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
        let t = own_token(TOKEN_QUERY)?;
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
            if CheckTokenMembership(std::ptr::null_mut(), sid.as_mut_ptr().cast(), &mut member) == 0
            {
                return None;
            }
        }
        Some(member != 0)
    }

    /// The console session, when a person is signed in at it and their
    /// token can be had. SYSTEM only: `WTSQueryUserToken` needs `SeTcb`.
    pub fn signed_in_console_session() -> Option<u32> {
        let session = supervisor::active_console_session_id()?;
        match supervisor::query_user_token(session) {
            Ok(Some(_)) => Some(session),
            _ => None,
        }
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

        let user_buf = token_info(token.raw(), TokenUser)
            .ok_or_else(|| last_error("reading the token's user"))?;
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

    fn well_known_sid(kind: WELL_KNOWN_SID_TYPE) -> Result<[u8; MAX_SID], String> {
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
        let mut sid = well_known_sid(WinMediumLabelSid)?;
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
            return Err(last_error("lowering the token to medium integrity"));
        }
        Ok(())
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
                || AddAccessAllowedAce(acl, ACL_REVISION, GENERIC_ALL, system.as_mut_ptr().cast())
                    == 0
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

    /// A token for `identity`: ours restricted, or the console user's.
    pub enum Token {
        Mine(Owned),
        Theirs(supervisor::OwnedHandle),
    }

    impl Token {
        pub fn raw(&self) -> HANDLE {
            match self {
                Self::Mine(t) => t.raw(),
                Self::Theirs(t) => t.raw(),
            }
        }
    }

    pub fn token_for(identity: Identity) -> Result<Token, String> {
        match identity {
            Identity::RestrictedCopy => restricted_medium_copy().map(Token::Mine),
            Identity::ConsoleUser { session } => match supervisor::query_user_token(session) {
                Ok(Some(t)) => Ok(Token::Theirs(t)),
                Ok(None) => Err(format!(
                    "nobody is signed in at console session {session} any more"
                )),
                Err(e) => Err(format!("cannot obtain the console user's token: {e:#}")),
            },
            Identity::Inherit => Err("the daemon's own identity needs no token".into()),
        }
    }

    /// This thread, impersonating a token until dropped.
    ///
    /// Not `Send`: an identity belongs to the thread that took it on.
    pub struct Impersonating(std::marker::PhantomData<*const ()>);

    impl Impersonating {
        pub fn begin(token: &Token) -> Result<Self, String> {
            // SAFETY: a live primary token with QUERY | DUPLICATE access.
            // Should impersonation be refused, Windows quietly gives the
            // thread an identification-level token instead, and every open
            // then fails: closed, never open.
            if unsafe { ImpersonateLoggedOnUser(token.raw()) } == 0 {
                return Err(last_error("ImpersonateLoggedOnUser"));
            }
            Ok(Self(std::marker::PhantomData))
        }
    }

    impl Drop for Impersonating {
        fn drop(&mut self) {
            // SAFETY: ends this thread's impersonation.
            if unsafe { RevertToSelf() } == 0 {
                // A pool thread left wearing someone else's identity would run
                // whatever it is handed next as the wrong account. There is no
                // safe way to continue; the service manager restarts us.
                std::process::abort();
            }
        }
    }

    /// Append one argument the way the child's own parser (the MSVC CRT
    /// rules, which Rust's `std::env::args` follows) reads it back — the
    /// algorithm `std::process::Command` uses. A display name or a folder is
    /// always ONE argument, whatever quotes or backslashes it holds.
    pub fn append_arg(cmd: &mut Vec<u16>, arg: &OsStr) {
        let quote = arg.is_empty()
            || arg
                .encode_wide()
                .any(|c| c == u16::from(b' ') || c == u16::from(b'\t'));
        if quote {
            cmd.push(u16::from(b'"'));
        }
        let mut backslashes = 0usize;
        for c in arg.encode_wide() {
            if c == u16::from(b'\\') {
                backslashes += 1;
            } else {
                if c == u16::from(b'"') {
                    // n backslashes before a quote become 2n + 1.
                    cmd.extend(std::iter::repeat_n(u16::from(b'\\'), backslashes + 1));
                }
                backslashes = 0;
            }
            cmd.push(c);
        }
        if quote {
            // …and before the closing quote, 2n.
            cmd.extend(std::iter::repeat_n(u16::from(b'\\'), backslashes));
            cmd.push(u16::from(b'"'));
        }
    }

    /// `"exe" arg arg…`, NUL-terminated.
    pub fn command_line(exe: &Path, args: &[OsString]) -> Vec<u16> {
        let mut cmd = Vec::new();
        // argv[0] follows simpler rules (no escapes), and a path holds no quote.
        cmd.push(u16::from(b'"'));
        cmd.extend(exe.as_os_str().encode_wide());
        cmd.push(u16::from(b'"'));
        for a in args {
            cmd.push(u16::from(b' '));
            append_arg(&mut cmd, a);
        }
        cmd.push(0);
        cmd
    }

    /// The pairs of a `CreateEnvironmentBlock` block.
    ///
    /// # Safety
    /// `block` must be a UTF-16, double-NUL-terminated environment block.
    unsafe fn parse_env_block(block: *const u16) -> Vec<(OsString, OsString)> {
        let mut out = Vec::new();
        let mut p = block;
        loop {
            let mut len = 0usize;
            // SAFETY: forwarded; each entry is NUL-terminated and the block
            // ends with an empty one.
            while unsafe { *p.add(len) } != 0 {
                len += 1;
            }
            if len == 0 {
                break;
            }
            // SAFETY: `len` u16s were just read from `p`.
            let entry = unsafe { std::slice::from_raw_parts(p, len) };
            // `=C:=C:\…` entries (the per-drive directories) start with `=`:
            // the name is up to the SECOND `=`.
            if let Some(eq) = entry.iter().skip(1).position(|c| *c == u16::from(b'=')) {
                let eq = eq + 1;
                out.push((
                    OsString::from_wide(&entry[..eq]),
                    OsString::from_wide(&entry[eq + 1..]),
                ));
            }
            // SAFETY: past this entry's NUL, still inside the block.
            p = unsafe { p.add(len + 1) };
        }
        out
    }

    /// A sorted, double-NUL-terminated block of `base` with `extra` on top
    /// (names compared case-insensitively, as Windows does).
    pub fn env_block(base: Vec<(OsString, OsString)>, extra: &[(String, String)]) -> Vec<u16> {
        let upper = |s: &OsStr| s.to_string_lossy().to_uppercase();
        let mut vars: Vec<(OsString, OsString)> = base
            .into_iter()
            .filter(|(k, _)| !extra.iter().any(|(e, _)| upper(k) == e.to_uppercase()))
            .collect();
        vars.extend(
            extra
                .iter()
                .map(|(k, v)| (OsString::from(k), OsString::from(v))),
        );
        vars.sort_by_key(|(k, _)| upper(k));
        let mut block = Vec::new();
        for (k, v) in vars {
            block.extend(k.encode_wide());
            block.push(u16::from(b'='));
            block.extend(v.encode_wide());
            block.push(0);
        }
        if block.is_empty() {
            block.push(0);
        }
        block.push(0);
        block
    }

    /// An anonymous pipe whose `child` end is inheritable and whose other end
    /// is not. Returns `(read, write)`.
    fn pipe(child_reads: bool) -> std::io::Result<(Owned, Owned)> {
        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: std::ptr::null_mut(),
            bInheritHandle: TRUE,
        };
        let (mut r, mut w): (HANDLE, HANDLE) = (std::ptr::null_mut(), std::ptr::null_mut());
        // SAFETY: valid out-params; `sa` outlives the call.
        if unsafe { CreatePipe(&mut r, &mut w, &sa, 0) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let (r, w) = (Owned(r), Owned(w));
        // Ours must not leak into the child: a child holding our end of its
        // own stdout keeps the pipe open after it exits, and we never see EOF.
        let ours = if child_reads { &w } else { &r };
        // SAFETY: a live handle we own.
        if unsafe { SetHandleInformation(ours.raw(), HANDLE_FLAG_INHERIT, 0) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok((r, w))
    }

    /// The recorder's stderr, into this daemon's log (a service has no
    /// stderr of its own to share with it).
    fn forward_stderr(read: Owned) {
        use std::io::BufRead;
        // SAFETY: the handle is ours; the File closes it.
        let file = unsafe { std::fs::File::from_raw_handle(read.into_raw()) };
        for line in std::io::BufReader::new(file).lines() {
            let Ok(line) = line else { break };
            let line: String = line.chars().take(2048).collect();
            tracing::info!(target: "roomlerd::recorder", "{line}");
        }
    }

    /// Launch the recorder with a token: the bounded handle list, our end of
    /// each pipe kept back, the child's ends closed here the moment it has
    /// them.
    pub fn spawn_as(
        identity: Identity,
        exe: &Path,
        args: &[OsString],
        env: &[(String, String)],
    ) -> std::io::Result<Launched> {
        let token = token_for(identity).map_err(std::io::Error::other)?;
        // A console user's own environment (their PATH, USERPROFILE, APPDATA
        // — a SYSTEM daemon's are the wrong ones); a restricted copy is the
        // same user as us, so ours.
        let base = match identity {
            Identity::ConsoleUser { .. } => {
                let block = supervisor::EnvBlock::for_token(token.raw())
                    .map_err(|e| std::io::Error::other(format!("{e:#}")))?;
                // SAFETY: CreateEnvironmentBlock's own block, alive here.
                unsafe { parse_env_block(block.raw as *const u16) }
            }
            _ => std::env::vars_os().collect(),
        };
        let env_w = env_block(base, env);

        let (in_r, in_w) = pipe(true)?;
        let (out_r, out_w) = pipe(false)?;
        let (err_r, err_w) = pipe(false)?;

        let mut si: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
        si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
        si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        si.StartupInfo.hStdInput = in_r.raw();
        si.StartupInfo.hStdOutput = out_w.raw();
        si.StartupInfo.hStdError = err_w.raw();

        // ⚠️ Inheritance must be ON for the std handles to reach the child,
        // and ON alone hands it EVERY inheritable handle in this daemon (other
        // children's pipes among them). The list makes it exactly these three.
        let mut inherit: Box<[HANDLE; 3]> = Box::new([in_r.raw(), out_w.raw(), err_w.raw()]);
        let mut bytes: usize = 0;
        // SAFETY: the sizing call is documented to fail with the size out.
        unsafe { InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut bytes) };
        if bytes == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut attr_buf = vec![0u8; bytes];
        let attrs = attr_buf.as_mut_ptr().cast();
        // SAFETY: the buffer is the size the sizing call asked for.
        if unsafe { InitializeProcThreadAttributeList(attrs, 1, 0, &mut bytes) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        struct Attrs(*mut c_void);
        impl Drop for Attrs {
            fn drop(&mut self) {
                // SAFETY: initialised above, deleted once.
                unsafe { DeleteProcThreadAttributeList(self.0.cast()) };
            }
        }
        let _attrs = Attrs(attrs);
        // SAFETY: `inherit` outlives the CreateProcessAsUserW call below.
        if unsafe {
            UpdateProcThreadAttribute(
                attrs,
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                inherit.as_mut_ptr() as *const c_void,
                std::mem::size_of::<HANDLE>() * 3,
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        si.lpAttributeList = attrs;

        // A console user's process belongs on the interactive desktop; a
        // restricted copy stays where this worker is.
        let mut desktop: Vec<u16> = OsStr::new("winsta0\\default")
            .encode_wide()
            .chain(Some(0))
            .collect();
        if matches!(identity, Identity::ConsoleUser { .. }) {
            si.StartupInfo.lpDesktop = desktop.as_mut_ptr();
        }

        let app: Vec<u16> = exe.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut cmdline = command_line(exe, args);
        let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: every buffer outlives the call; `app` names the program, so
        // the command line's first token is never searched for; the env block
        // is UTF-16 and double-NUL-terminated.
        let ok = unsafe {
            CreateProcessAsUserW(
                token.raw(),
                app.as_ptr(),
                cmdline.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                TRUE,
                CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT,
                env_w.as_ptr() as *const c_void,
                std::ptr::null(),
                &si.StartupInfo,
                &mut pi,
            )
        };
        if ok == 0 {
            let e = std::io::Error::last_os_error();
            return Err(std::io::Error::new(
                e.kind(),
                format!("CreateProcessAsUserW: {e}"),
            ));
        }
        // The child has its ends; ours go now, or EOF never comes.
        drop((in_r, out_w, err_w));
        std::thread::Builder::new()
            .name("recorder-stderr".into())
            .spawn(move || forward_stderr(err_r))
            .ok();

        // SAFETY: the handles are ours; each File closes its own.
        let stdin = unsafe { std::fs::File::from_raw_handle(in_w.into_raw()) };
        // SAFETY: as above.
        let stdout = unsafe { std::fs::File::from_raw_handle(out_r.into_raw()) };
        Ok(Launched {
            stdin: Some(Box::new(tokio::fs::File::from_std(stdin))),
            stdout: Box::new(tokio::fs::File::from_std(stdout)),
            child: Child::Win(Arc::new(OwnedProcess::from_raw_parts(
                pi.hProcess,
                pi.hThread,
                pi.dwProcessId,
            ))),
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// The quoting is checked against Windows's own parser: a controller's
        /// display name or a folder with quotes, backslashes or spaces stays
        /// ONE argument, and nothing in it can become another flag.
        #[test]
        fn every_argument_comes_back_whole() {
            use windows_sys::Win32::Foundation::LocalFree;
            use windows_sys::Win32::UI::Shell::CommandLineToArgvW;
            let cases = [
                "plain",
                "",
                "two words",
                "tab\there",
                "\"",
                "\\",
                "trailing\\",
                "trailing space\\",
                "a\\\\\"b",
                "\\\\server\\share\\",
                "--remote-user-name=Ana \"--out C:\\Windows\" Maria",
                "\" --out C:\\Windows\\System32 \"",
                "Ведран Јовановић",
                "C:\\Users\\a b\\Videos\\Roomler",
            ];
            let args: Vec<OsString> = cases.iter().map(OsString::from).collect();
            let cmd = command_line(Path::new(r"C:\Program Files\Roomler\roomlerd.exe"), &args);
            let mut n = 0i32;
            // SAFETY: a NUL-terminated command line; the array is freed below.
            let argv = unsafe { CommandLineToArgvW(cmd.as_ptr(), &mut n) };
            assert!(!argv.is_null());
            let parsed: Vec<String> = (0..n as usize)
                .map(|i| {
                    // SAFETY: `argv` holds `n` NUL-terminated strings.
                    unsafe {
                        let p = *argv.add(i);
                        let len = (0..).take_while(|&j| *p.add(j) != 0).count();
                        String::from_utf16_lossy(std::slice::from_raw_parts(p, len))
                    }
                })
                .collect();
            // SAFETY: the array CommandLineToArgvW allocated.
            unsafe { LocalFree(argv.cast()) };
            assert_eq!(parsed[0], r"C:\Program Files\Roomler\roomlerd.exe");
            assert_eq!(&parsed[1..], &cases[..]);
        }

        /// The daemon's work in the person's folder gets only the person's
        /// rights: on an elevated run, a folder only Administrators may write
        /// to takes this process's write and refuses the same write made
        /// through `as_identity(RestrictedCopy, …)` — so a junction from the
        /// recordings folder to such a place reaches nothing. The thread is
        /// itself again afterwards.
        #[test]
        fn work_done_as_the_recorder_gets_only_the_recorders_rights() {
            if admin_group_enabled() != Some(true) {
                // The CI lane runs elevated on purpose; there, skipping
                // would pass without proving anything.
                assert!(
                    std::env::var_os("ROOMLERD_TEST_REQUIRE_ELEVATED").is_none(),
                    "this lane must run elevated for the refusal to be proven"
                );
                println!("not elevated: there is no higher right to withhold here");
                return;
            }
            let dir = tempfile::tempdir().unwrap();
            let locked = dir.path().join("administrators-only");
            std::fs::create_dir(&locked).unwrap();
            let granted = std::process::Command::new("icacls")
                .arg(&locked)
                .args(["/inheritance:r", "/grant:r", "*S-1-5-32-544:(OI)(CI)F"])
                .output()
                .unwrap();
            assert!(granted.status.success(), "{granted:?}");

            std::fs::write(locked.join("as-the-daemon"), b"x")
                .expect("the elevated daemon may write there: the control");
            let (inside, write) = super::super::as_identity(Identity::RestrictedCopy, || {
                (
                    admin_group_enabled(),
                    std::fs::write(locked.join("as-the-recorder"), b"x"),
                )
            })
            .unwrap();
            assert_eq!(
                inside,
                Some(false),
                "the thread did not take the identity on"
            );
            assert!(
                write.is_err(),
                "work done as the recorder wrote into an Administrators-only folder"
            );
            assert!(!locked.join("as-the-recorder").exists());
            assert_eq!(
                admin_group_enabled(),
                Some(true),
                "the thread is itself again"
            );
        }

        #[test]
        fn extra_variables_replace_their_namesake_whatever_its_case() {
            let base = vec![
                (OsString::from("Path"), OsString::from("C:\\x")),
                (
                    OsString::from("ROOMLERD_SYNTHETIC_FRAMES"),
                    OsString::from("0"),
                ),
                (OsString::from("=C:"), OsString::from("C:\\")),
            ];
            let block = env_block(base, &[("roomlerd_synthetic_frames".into(), "1".into())]);
            // SAFETY: `env_block` builds a double-NUL-terminated block.
            let back = unsafe { parse_env_block(block.as_ptr()) };
            let get = |k: &str| {
                back.iter()
                    .filter(|(n, _)| n.to_string_lossy().eq_ignore_ascii_case(k))
                    .map(|(_, v)| v.to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
            };
            assert_eq!(get("ROOMLERD_SYNTHETIC_FRAMES"), vec!["1".to_string()]);
            assert_eq!(get("PATH"), vec!["C:\\x".to_string()]);
            assert_eq!(get("=C:"), vec!["C:\\".to_string()]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(windows: bool, service: bool, high: bool, console: Option<u32>) -> Facts {
        Facts {
            windows,
            service_account: service,
            above_medium: high,
            console_session: console,
        }
    }

    #[test]
    fn the_identity_rule_as_a_table() {
        // An ordinary user: nothing to change.
        assert_eq!(
            decide_from(f(true, false, false, None)),
            Ok(Identity::Inherit)
        );
        assert_eq!(
            decide_from(f(false, false, false, None)),
            Ok(Identity::Inherit)
        );
        // An elevated worker: never records elevated.
        assert_eq!(
            decide_from(f(true, false, true, None)),
            Ok(Identity::RestrictedCopy)
        );
        // SYSTEM with someone at the console: as them.
        assert_eq!(
            decide_from(f(true, true, true, Some(3))),
            Ok(Identity::ConsoleUser { session: 3 })
        );
        // SYSTEM with nobody: refused, never "as SYSTEM".
        assert_eq!(
            decide_from(f(true, true, true, None)),
            Err(Refusal::NoConsoleUser)
        );
        // root: refused until the unix drop exists.
        assert_eq!(
            decide_from(f(false, true, false, None)),
            Err(Refusal::RootDaemon)
        );
    }

    /// A test run is an ordinary account or an elevated one, never SYSTEM
    /// or (outside a container) root; the probe must read it as such.
    #[test]
    fn this_process_is_not_refused() {
        #[cfg(unix)]
        // SAFETY: reads our own credentials.
        if unsafe { libc::geteuid() } == 0 {
            return; // a root container: the refusal is right
        }
        assert!(decide().is_ok(), "{:?}", facts());
    }
}
