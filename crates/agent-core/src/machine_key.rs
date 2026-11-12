// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! #3737 — a machine registry key that only SYSTEM and Administrators may
//! change or read, and the check that says whether a key is one.
//!
//! The SYSTEM service's anchor (FR-94) and its SSH host key (FR-105) live in
//! `HKLM\SOFTWARE\Roomler\ExecService` (`roomlerd`'s `exec_service::store`).
//! The service takes them only from a key that:
//!
//! - is owned by SYSTEM, Administrators or TrustedInstaller;
//! - gives no other account a right to change it, or to read its values
//!   ([`KEY_SECRET`]): one of them is the service's credential ([`verdict`]);
//! - sits in a parent that gives no other account a right to change it,
//!   counting what the parent hands down to a key made below it later
//!   ([`parent_verdict`]).
//!
//! A key is never moved from one parent to another, and only an account that
//! may write a key can rename it inside its parent, so these three answer who
//! chose what the key holds. Only SYSTEM and Administrators may create a key
//! below `HKLM\SOFTWARE`.
//!
//! The writer ([`create_sealed`]) makes the key with a protected DACL of
//! SYSTEM and Administrators alone (`O:BAD:P(A;CI;KA;;;SY)(A;CI;KA;;;BA)`),
//! and an administrator's writer takes over and empties one it finds that
//! fails the check. Every read and write goes through a handle the check was
//! made on.

use crate::machine_tree::{ACE_ALLOW, ACE_DENY, INHERIT_ONLY, Sd};

/// `KEY_QUERY_VALUE`: read a value.
pub const KEY_QUERY_VALUE: u32 = 0x0001;
/// `KEY_SET_VALUE`: create, change or delete a value.
pub const KEY_SET_VALUE: u32 = 0x0002;
/// `KEY_CREATE_SUB_KEY`.
pub const KEY_CREATE_SUB_KEY: u32 = 0x0004;
/// `KEY_ENUMERATE_SUB_KEYS`.
pub const KEY_ENUMERATE_SUB_KEYS: u32 = 0x0008;
/// `KEY_NOTIFY`.
pub const KEY_NOTIFY: u32 = 0x0010;
/// `KEY_CREATE_LINK`: make a symbolic link below the key.
pub const KEY_CREATE_LINK: u32 = 0x0020;
const DELETE: u32 = 0x1_0000;
const READ_CONTROL: u32 = 0x2_0000;
const WRITE_DAC: u32 = 0x4_0000;
const WRITE_OWNER: u32 = 0x8_0000;
/// `KEY_READ`, which `KEY_EXECUTE` equals.
pub const KEY_READ: u32 = READ_CONTROL | KEY_QUERY_VALUE | KEY_ENUMERATE_SUB_KEYS | KEY_NOTIFY;
/// `KEY_WRITE`.
pub const KEY_WRITE: u32 = READ_CONTROL | KEY_SET_VALUE | KEY_CREATE_SUB_KEY;
/// `KEY_ALL_ACCESS`.
pub const KEY_ALL_ACCESS: u32 = 0xF_003F;

/// Rights that let a holder change what a key holds, its subkeys, its name,
/// its DACL or its owner.
pub const KEY_WRITE_LIKE: u32 =
    KEY_SET_VALUE | KEY_CREATE_SUB_KEY | KEY_CREATE_LINK | DELETE | WRITE_DAC | WRITE_OWNER;
/// [`KEY_WRITE_LIKE`] and the right to read a value: what no account other
/// than SYSTEM or Administrators may hold on a key whose values are a secret.
pub const KEY_SECRET: u32 = KEY_WRITE_LIKE | KEY_QUERY_VALUE;

/// A mask with its generic bits replaced by the key rights they stand for
/// (`GENERIC_READ` → `KEY_READ`, `GENERIC_WRITE` → `KEY_WRITE`,
/// `GENERIC_EXECUTE` → `KEY_EXECUTE`, `GENERIC_ALL` → `KEY_ALL_ACCESS`). An
/// inherit-only ACE keeps generic bits as written.
pub fn map_generic_key(mask: u32) -> u32 {
    const GENERIC_READ: u32 = 0x8000_0000;
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const GENERIC_EXECUTE: u32 = 0x2000_0000;
    const GENERIC_ALL: u32 = 0x1000_0000;
    let mut out = mask & !(GENERIC_READ | GENERIC_WRITE | GENERIC_EXECUTE | GENERIC_ALL);
    if mask & (GENERIC_READ | GENERIC_EXECUTE) != 0 {
        out |= KEY_READ;
    }
    if mask & GENERIC_WRITE != 0 {
        out |= KEY_WRITE;
    }
    if mask & GENERIC_ALL != 0 {
        out |= KEY_ALL_ACCESS;
    }
    out
}

/// SYSTEM, the Administrators group, or TrustedInstaller (no person signs
/// in as it, #3666's rule).
pub fn trusted(sid: &str) -> bool {
    crate::machine_tree::trusted(sid) || sid == crate::machine_tree::TRUSTED_INSTALLER
}

/// `S-1-3-0`. An ACE for it hands what it grants to whoever makes a key below
/// the one that carries it.
const CREATOR_OWNER: &str = "S-1-3-0";

/// The key's own check, over its owner and DACL as read: `Ok` only when it is
/// owned by SYSTEM, Administrators or TrustedInstaller, has a DACL, and no
/// allow-ACE that applies to the key gives another account a right in
/// `counts` ([`KEY_SECRET`] for a key whose values are a secret), generic
/// rights mapped first. An inherit-only ACE does not apply to the key. A deny
/// is never credited: an allow counts whatever a deny beside it says, so a
/// wrong answer only ever stops a device. An ACE of any other kind is a
/// refusal, because this check fails closed. `what` names the key in the
/// reason.
pub fn verdict(sd: &Sd, counts: u32, what: &str) -> Result<(), String> {
    owned(sd, what)?;
    let Some(aces) = &sd.dacl else {
        return Err(format!(
            "{what} has no DACL, so every account may change it"
        ));
    };
    for ace in aces {
        if ace.flags & INHERIT_ONLY != 0 {
            continue;
        }
        match ace.kind {
            ACE_DENY => {}
            ACE_ALLOW => {
                let granted = map_generic_key(ace.mask) & counts;
                if granted != 0 && !trusted(&ace.sid) {
                    let may = if granted & KEY_WRITE_LIKE != 0 {
                        "change it"
                    } else {
                        "read what it holds"
                    };
                    return Err(format!(
                        "{what} lets an account other than SYSTEM or Administrators {may}"
                    ));
                }
            }
            other => return Err(unread(what, other)),
        }
    }
    Ok(())
}

/// The parent's check: `Ok` only when it is owned by SYSTEM, Administrators or
/// TrustedInstaller, has a DACL, and no allow-ACE gives another account a
/// right in [`KEY_WRITE_LIKE`], inherit-only ACEs included: what the parent
/// hands down reaches a key made below it later, and only an account that may
/// write a key below the parent can rename that key to the name the reader
/// opens. Reading the parent is fine: `HKLM\SOFTWARE` lets Users, app packages
/// and capability SIDs read, and so does the key Windows makes below it.
/// CREATOR OWNER is spared: it hands its rights only to whoever makes a key
/// below the parent, and making one takes a right this check already counts.
/// A deny is never credited; an ACE of another kind is a refusal.
pub fn parent_verdict(sd: &Sd, what: &str) -> Result<(), String> {
    owned(sd, what)?;
    let Some(aces) = &sd.dacl else {
        return Err(format!(
            "{what} has no DACL, so every account may change it"
        ));
    };
    for ace in aces {
        match ace.kind {
            ACE_DENY => {}
            ACE_ALLOW => {
                if map_generic_key(ace.mask) & KEY_WRITE_LIKE != 0
                    && !trusted(&ace.sid)
                    && ace.sid != CREATOR_OWNER
                {
                    let whom = if ace.flags & INHERIT_ONLY != 0 {
                        "a key made below it"
                    } else {
                        "it"
                    };
                    return Err(format!(
                        "{what} lets an account other than SYSTEM or Administrators change {whom}"
                    ));
                }
            }
            other => return Err(unread(what, other)),
        }
    }
    Ok(())
}

/// The owner decides the DACL, so it must be one the checks trust.
fn owned(sd: &Sd, what: &str) -> Result<(), String> {
    if trusted(&sd.owner) {
        Ok(())
    } else {
        Err(format!(
            "{what} is owned by an account other than SYSTEM or Administrators"
        ))
    }
}

fn unread(what: &str, kind: u8) -> String {
    format!("{what} carries an access rule of a kind this check does not read (type {kind})")
}

#[cfg(windows)]
pub use win::{Hive, Key, KeyPath, create_sealed, open_checked, open_for_change};

/// Tests only: the step of a takeover after which every later takeover in this
/// process stops with an error, as a crash there would. `0`: none.
#[cfg(all(windows, any(test, feature = "test-fixtures")))]
static STOP_TAKEOVER_AFTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// The Windows half: open, create, read and write a key by handle.
#[cfg(windows)]
mod win {
    use windows_sys::Win32::Foundation::{
        ERROR_FILE_NOT_FOUND, ERROR_MORE_DATA, ERROR_NO_MORE_ITEMS, ERROR_SUCCESS, HANDLE,
        LocalFree,
    };
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1, SE_REGISTRY_KEY,
        SetSecurityInfo,
    };
    use windows_sys::Win32::Security::{
        ACL, DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, GetSecurityDescriptorOwner,
        OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
        PSID, SECURITY_ATTRIBUTES,
    };
    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_LOCAL_MACHINE, KEY_WOW64_64KEY, REG_CREATED_NEW_KEY, REG_OPTION_NON_VOLATILE,
        REG_SZ, RRF_RT_REG_SZ, RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegEnumValueW,
        RegGetValueW, RegOpenKeyExW, RegSetValueExW,
    };

    use super::{
        KEY_CREATE_SUB_KEY, KEY_QUERY_VALUE, KEY_READ, KEY_SECRET, KEY_SET_VALUE, READ_CONTROL,
        WRITE_DAC, WRITE_OWNER, parent_verdict, verdict,
    };

    /// The sealed key: Administrators own it, SYSTEM and Administrators alone
    /// may touch it, inheritance off. `CI` hands the same to a subkey.
    pub(super) const SEALED_SDDL: &str = "O:BAD:P(A;CI;KA;;;SY)(A;CI;KA;;;BA)";

    /// Which hive a key is in. A test build may name the person's own hive,
    /// for a stand-in key no test run ever makes under `HKLM`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Hive {
        LocalMachine,
        #[cfg(any(test, feature = "test-fixtures"))]
        CurrentUser,
    }

    impl Hive {
        pub(super) fn root(self) -> HKEY {
            match self {
                Hive::LocalMachine => HKEY_LOCAL_MACHINE,
                #[cfg(any(test, feature = "test-fixtures"))]
                Hive::CurrentUser => windows_sys::Win32::System::Registry::HKEY_CURRENT_USER,
            }
        }

        fn shown(self) -> &'static str {
            match self {
                Hive::LocalMachine => "HKLM",
                #[cfg(any(test, feature = "test-fixtures"))]
                Hive::CurrentUser => "HKCU",
            }
        }
    }

    /// A key: its hive, the path of its parent in that hive, and its name.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct KeyPath {
        pub hive: Hive,
        pub parent: String,
        pub name: String,
    }

    impl KeyPath {
        /// `HKLM\<parent>\<name>`, for messages.
        pub fn shown(&self) -> String {
            format!("{}\\{}\\{}", self.hive.shown(), self.parent, self.name)
        }

        /// `HKLM\<parent>`, for messages.
        pub fn parent_shown(&self) -> String {
            format!("{}\\{}", self.hive.shown(), self.parent)
        }
    }

    pub(super) fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn os_error(code: u32) -> std::io::Error {
        std::io::Error::from_raw_os_error(code as i32)
    }

    /// An open key, closed on drop.
    #[derive(Debug)]
    pub struct Key {
        hkey: HKEY,
        shown: String,
    }

    // SAFETY: a registry handle has no thread affinity.
    unsafe impl Send for Key {}

    impl Drop for Key {
        fn drop(&mut self) {
            // SAFETY: a handle this value opened, closed once.
            unsafe { RegCloseKey(self.hkey) };
        }
    }

    impl Key {
        pub(super) fn handle(&self) -> HANDLE {
            self.hkey as HANDLE
        }

        /// The value `name`, a `REG_SZ` of at most `max_bytes`. `Ok(None)`:
        /// there is none.
        pub fn read(&self, name: &str, max_bytes: u32) -> Result<Option<String>, String> {
            let value = wide(name);
            let mut len: u32 = 0;
            // SAFETY: a live key opened with KEY_QUERY_VALUE; a null buffer
            // asks only for the size, in bytes.
            let rc = unsafe {
                RegGetValueW(
                    self.hkey,
                    std::ptr::null(),
                    value.as_ptr(),
                    RRF_RT_REG_SZ,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &mut len,
                )
            };
            if rc == ERROR_FILE_NOT_FOUND {
                return Ok(None);
            }
            if rc != ERROR_SUCCESS {
                return Err(format!("{} {name}: {}", self.shown, os_error(rc)));
            }
            if len > max_bytes {
                return Err(format!("{} {name} is too large to be one", self.shown));
            }
            let mut buf = vec![0u16; (len as usize).div_ceil(2).max(1)];
            let mut len = (buf.len() * 2) as u32;
            // SAFETY: `buf` holds `len` bytes; RegGetValueW NUL-terminates a
            // REG_SZ it returns, and fails with ERROR_MORE_DATA if it grew.
            let rc = unsafe {
                RegGetValueW(
                    self.hkey,
                    std::ptr::null(),
                    value.as_ptr(),
                    RRF_RT_REG_SZ,
                    std::ptr::null_mut(),
                    buf.as_mut_ptr().cast(),
                    &mut len,
                )
            };
            if rc == ERROR_MORE_DATA {
                return Err(format!("{} {name} changed while it was read", self.shown));
            }
            if rc != ERROR_SUCCESS {
                return Err(format!("{} {name}: {}", self.shown, os_error(rc)));
            }
            let chars = (len as usize / 2).min(buf.len());
            let text: Vec<u16> = buf[..chars]
                .iter()
                .copied()
                .take_while(|&c| c != 0)
                .collect();
            String::from_utf16(&text)
                .map(Some)
                .map_err(|_| format!("{} {name} is not text", self.shown))
        }

        /// Write `text` as the `REG_SZ` value `name`.
        pub fn write(&self, name: &str, text: &str) -> Result<(), String> {
            let value = wide(name);
            let data = wide(text);
            // SAFETY: a live key opened with KEY_SET_VALUE; `data` is the
            // REG_SZ with its NUL, sized in bytes.
            let rc = unsafe {
                RegSetValueExW(
                    self.hkey,
                    value.as_ptr(),
                    0,
                    REG_SZ,
                    data.as_ptr().cast(),
                    (data.len() * 2) as u32,
                )
            };
            if rc != ERROR_SUCCESS {
                return Err(format!("writing {} {name}: {}", self.shown, os_error(rc)));
            }
            Ok(())
        }

        /// Delete the value `name`. Whether there was one.
        pub fn delete(&self, name: &str) -> Result<bool, String> {
            let value = wide(name);
            // SAFETY: a live key opened with KEY_SET_VALUE; a NUL-terminated
            // name.
            let rc = unsafe { RegDeleteValueW(self.hkey, value.as_ptr()) };
            match rc {
                ERROR_SUCCESS => Ok(true),
                ERROR_FILE_NOT_FOUND => Ok(false),
                rc => Err(format!("deleting {} {name}: {}", self.shown, os_error(rc))),
            }
        }

        /// The names of every value the key holds.
        pub fn names(&self) -> Result<Vec<String>, String> {
            let mut names = Vec::new();
            for index in 0.. {
                // 16,383 characters is the longest value name there is.
                let mut buf = vec![0u16; 16_384];
                let mut len = buf.len() as u32;
                // SAFETY: a live key opened with KEY_QUERY_VALUE; `buf` holds
                // `len` characters; no type or data is asked for.
                let rc = unsafe {
                    RegEnumValueW(
                        self.hkey,
                        index,
                        buf.as_mut_ptr(),
                        &mut len,
                        std::ptr::null(),
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                    )
                };
                match rc {
                    ERROR_SUCCESS => {
                        names.push(String::from_utf16_lossy(&buf[..len as usize]));
                    }
                    ERROR_NO_MORE_ITEMS => break,
                    rc => {
                        return Err(format!(
                            "listing what {} holds: {}",
                            self.shown,
                            os_error(rc)
                        ));
                    }
                }
            }
            Ok(names)
        }

        /// Make this key the sealed one: Administrators as its owner and the
        /// protected DACL of SYSTEM and Administrators alone, by handle, in
        /// one call. The handle carries `WRITE_OWNER` and `WRITE_DAC`.
        fn seal(&self) -> Result<(), String> {
            let sd = Descriptor::from_sddl(SEALED_SDDL)
                .map_err(|e| format!("the sealed key's security: {e}"))?;
            let mut owner: PSID = std::ptr::null_mut();
            let mut dacl: *mut ACL = std::ptr::null_mut();
            let mut present = 0;
            let mut defaulted = 0;
            // SAFETY: the descriptor made above; `owner` and `dacl` point into
            // it, which lives until the end of this function.
            let ok = unsafe {
                GetSecurityDescriptorOwner(sd.0, &mut owner, &mut defaulted) != 0
                    && GetSecurityDescriptorDacl(sd.0, &mut present, &mut dacl, &mut defaulted) != 0
            };
            if !ok {
                return Err(format!(
                    "the sealed key's security: {}",
                    std::io::Error::last_os_error()
                ));
            }
            // SAFETY: a live key opened with WRITE_OWNER and WRITE_DAC; the
            // owner and DACL point into `sd`.
            let rc = unsafe {
                SetSecurityInfo(
                    self.handle(),
                    SE_REGISTRY_KEY,
                    OWNER_SECURITY_INFORMATION
                        | DACL_SECURITY_INFORMATION
                        | PROTECTED_DACL_SECURITY_INFORMATION,
                    owner,
                    std::ptr::null_mut(),
                    dacl,
                    std::ptr::null(),
                )
            };
            if rc != ERROR_SUCCESS {
                return Err(format!("taking {} over: {}", self.shown, os_error(rc)));
            }
            Ok(())
        }
    }

    /// A security descriptor made from SDDL, freed on drop.
    pub(super) struct Descriptor(pub(super) PSECURITY_DESCRIPTOR);

    impl Descriptor {
        pub(super) fn from_sddl(sddl: &str) -> std::io::Result<Self> {
            let text = wide(sddl);
            let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
            // SAFETY: a NUL-terminated string; `sd` receives a LocalAlloc'd
            // descriptor, freed in Drop; the size out-param is optional.
            let ok = unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    text.as_ptr(),
                    SDDL_REVISION_1,
                    &mut sd,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(Self(sd))
        }
    }

    impl Drop for Descriptor {
        fn drop(&mut self) {
            // SAFETY: the one descriptor ConvertStringSecurityDescriptor… made.
            unsafe { LocalFree(self.0 as _) };
        }
    }

    /// Open `sub` below `root` with `access` (64-bit view). `Ok(None)`: it is
    /// not there.
    pub(super) fn open(
        root: HKEY,
        sub: &str,
        access: u32,
        shown: String,
    ) -> Result<Option<Key>, String> {
        let name = wide(sub);
        let mut hkey: HKEY = std::ptr::null_mut();
        // SAFETY: a NUL-terminated subkey; `hkey` is a valid out-pointer,
        // closed by `Key`'s Drop.
        let rc =
            unsafe { RegOpenKeyExW(root, name.as_ptr(), 0, access | KEY_WOW64_64KEY, &mut hkey) };
        match rc {
            ERROR_SUCCESS => Ok(Some(Key { hkey, shown })),
            ERROR_FILE_NOT_FOUND => Ok(None),
            rc => Err(format!("{shown}: {}", os_error(rc))),
        }
    }

    /// Create `sub` below `root`, or open it: with `sddl` when it is made
    /// here, with what `root` hands down when `sddl` is `None`. Whether it
    /// was made here.
    fn create(
        root: HKEY,
        sub: &str,
        access: u32,
        sddl: Option<&str>,
        shown: String,
    ) -> Result<(Key, bool), String> {
        let sd = sddl
            .map(Descriptor::from_sddl)
            .transpose()
            .map_err(|e| format!("{shown}: its security: {e}"))?;
        let sa = sd.as_ref().map(|sd| SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd.0,
            bInheritHandle: 0,
        });
        let name = wide(sub);
        let mut hkey: HKEY = std::ptr::null_mut();
        let mut disposition = 0u32;
        // SAFETY: a NUL-terminated subkey; the attributes, when given, point
        // into `sd`, alive across the call; `hkey` is closed by `Key`'s Drop.
        let rc = unsafe {
            RegCreateKeyExW(
                root,
                name.as_ptr(),
                0,
                std::ptr::null(),
                REG_OPTION_NON_VOLATILE,
                access | KEY_WOW64_64KEY,
                sa.as_ref()
                    .map_or(std::ptr::null(), |sa| sa as *const SECURITY_ATTRIBUTES),
                &mut hkey,
                &mut disposition,
            )
        };
        if rc != ERROR_SUCCESS {
            return Err(format!("{shown}: {}", os_error(rc)));
        }
        Ok((Key { hkey, shown }, disposition == REG_CREATED_NEW_KEY))
    }

    fn security(key: &Key, shown: &str) -> Result<crate::machine_tree::Sd, String> {
        crate::machine_tree::security_of(key.handle(), SE_REGISTRY_KEY)
            .map_err(|e| format!("{shown}: reading its security: {e}"))
    }

    /// The reader: the key `path`, opened to read its values, when it and its
    /// parent pass the check ([`verdict`] with [`KEY_SECRET`],
    /// [`parent_verdict`]). `Ok(None)`: there is no such key. `Err`: there is
    /// one, and its owner or DACL, or its parent's, says nobody should act on
    /// what it holds. The parent is judged first and the key is opened below
    /// that parent's handle, so both checks are of the keys the values come
    /// from.
    pub fn open_checked(path: &KeyPath) -> Result<Option<Key>, String> {
        let Some(parent) = open(
            path.hive.root(),
            &path.parent,
            KEY_READ,
            path.parent_shown(),
        )?
        else {
            return Ok(None);
        };
        let Some(key) = open(
            parent.hkey,
            &path.name,
            KEY_QUERY_VALUE | READ_CONTROL,
            path.shown(),
        )?
        else {
            return Ok(None);
        };
        verdict(&security(&key, &path.shown())?, KEY_SECRET, &path.shown())?;
        parent_verdict(
            &security(&parent, &path.parent_shown())?,
            &path.parent_shown(),
        )?;
        Ok(Some(key))
    }

    /// The writer: the key `path`, made sealed when it is not there, opened to
    /// read and write its values. Its parent is made when missing, with what
    /// its own parent hands down (`HKLM\SOFTWARE`'s: Users may read), and must
    /// pass [`parent_verdict`]: a key below a parent another account may
    /// change is never written.
    ///
    /// A key that is there and fails [`verdict`] (an administrator loosened
    /// it, or it was made by hand) is an `Err` when `take_over` is false. With
    /// `take_over` (an elevated administrator's writer) it is taken over: the
    /// sealed owner and DACL, by handle, and then every value it holds is
    /// deleted, so nothing that was in it while it was open is kept. An
    /// account other than SYSTEM or Administrators that owns the key keeps the
    /// writer out (the open is refused); the error says so.
    pub fn create_sealed(path: &KeyPath, take_over: bool) -> Result<Key, String> {
        let (parent, _) = create(
            path.hive.root(),
            &path.parent,
            KEY_READ | KEY_CREATE_SUB_KEY,
            None,
            path.parent_shown(),
        )?;
        parent_verdict(
            &security(&parent, &path.parent_shown())?,
            &path.parent_shown(),
        )?;
        let (key, made) = create(
            parent.hkey,
            &path.name,
            KEY_QUERY_VALUE | KEY_SET_VALUE | READ_CONTROL | WRITE_DAC | WRITE_OWNER,
            Some(SEALED_SDDL),
            path.shown(),
        )
        .map_err(|e| {
            format!(
                "{e} (if another account made this key, delete it from an elevated prompt and \
                 try again)"
            )
        })?;
        if made {
            return Ok(key);
        }
        if let Err(why) = verdict(&security(&key, &path.shown())?, KEY_SECRET, &path.shown()) {
            if !take_over {
                return Err(why);
            }
            key.seal()?;
            stop_point(1)?;
            let stray = key.names()?;
            for name in &stray {
                key.delete(name)?;
            }
            stop_point(2)?;
            tracing::warn!(
                key = %path.shown(), reason = %why, removed = ?stray,
                "machine key: took the key over and emptied it"
            );
        }
        Ok(key)
    }

    /// Tests only: stop a takeover after its step `step` with an error, as a
    /// crash there would ([`super::test_support::stop_takeover_after`]).
    fn stop_point(step: usize) -> Result<(), String> {
        #[cfg(any(test, feature = "test-fixtures"))]
        if super::STOP_TAKEOVER_AFTER.load(std::sync::atomic::Ordering::SeqCst) == step {
            return Err(format!("a test stopped the takeover after its step {step}"));
        }
        let _ = step;
        Ok(())
    }

    /// The key `path`, opened to delete its values, judged by nothing:
    /// taking a value away never makes anything trusted. `Ok(None)`: it is not
    /// there.
    pub fn open_for_change(path: &KeyPath) -> Result<Option<Key>, String> {
        let Some(parent) = open(
            path.hive.root(),
            &path.parent,
            KEY_READ,
            path.parent_shown(),
        )?
        else {
            return Ok(None);
        };
        open(
            parent.hkey,
            &path.name,
            KEY_QUERY_VALUE | KEY_SET_VALUE,
            path.shown(),
        )
    }
}

/// Helpers for the tests here and in `roomlerd`: make a key with a given
/// security, read a key's security as SDDL, its value names and values, and
/// remove a stand-in tree. Independent of the reader and the writer above,
/// except for the plain open.
#[cfg(all(windows, any(test, feature = "test-fixtures")))]
#[doc(hidden)]
pub mod test_support {
    use windows_sys::Wdk::System::Registry::{NtDeleteKey, NtSetValueKey};
    use windows_sys::Win32::Foundation::{
        ERROR_FILE_NOT_FOUND, ERROR_NO_MORE_ITEMS, ERROR_SUCCESS, LocalFree, UNICODE_STRING,
    };
    use windows_sys::Win32::Security::Authorization::{
        ConvertSecurityDescriptorToStringSecurityDescriptorW, GetSecurityInfo, SDDL_REVISION_1,
        SE_REGISTRY_KEY,
    };
    use windows_sys::Win32::Security::{
        DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
        SECURITY_ATTRIBUTES,
    };
    use windows_sys::Win32::System::Registry::{
        HKEY, KEY_WOW64_64KEY, REG_LINK, REG_OPTION_CREATE_LINK, REG_OPTION_NON_VOLATILE,
        REG_OPTION_OPEN_LINK, REG_SZ, RegCloseKey, RegCreateKeyExW, RegDeleteKeyExW, RegEnumKeyExW,
        RegOpenKeyExW, RegQueryInfoKeyW, RegSetValueExW,
    };

    use super::win::{Descriptor, open, wide};
    use super::{Hive, KEY_ALL_ACCESS, KEY_READ, Key, KeyPath};

    /// `HKLM\SOFTWARE\Roomler` as a machine has it, made by SYSTEM below
    /// `HKLM\SOFTWARE`: owned by Administrators, SYSTEM and Administrators
    /// full control, Users read, and CREATOR OWNER's full control handed down.
    pub const PARENT_SDDL: &str =
        "O:BAD:P(A;CI;KA;;;SY)(A;CI;KA;;;BA)(A;CI;KR;;;BU)(A;CIIO;KA;;;CO)";

    /// Make `sub` below `hive` with exactly `sddl`, or panic. It must not be
    /// there yet.
    pub fn make(hive: Hive, sub: &str, sddl: &str) {
        let sd = Descriptor::from_sddl(sddl).expect("the SDDL");
        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd.0,
            bInheritHandle: 0,
        };
        let name = wide(sub);
        let mut hkey: HKEY = std::ptr::null_mut();
        let mut disposition = 0u32;
        // SAFETY: a NUL-terminated name; the attributes point into `sd`; the
        // key is closed below.
        let rc = unsafe {
            RegCreateKeyExW(
                hive.root(),
                name.as_ptr(),
                0,
                std::ptr::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_ALL_ACCESS | KEY_WOW64_64KEY,
                &sa,
                &mut hkey,
                &mut disposition,
            )
        };
        assert_eq!(rc, ERROR_SUCCESS, "making {sub}");
        // SAFETY: the key made above, closed once.
        unsafe { RegCloseKey(hkey) };
        assert_eq!(disposition, 1, "{sub} was made here, not found");
    }

    /// Make `sub` below `hive` with what its parent hands down, unless it is
    /// there.
    pub fn ensure(hive: Hive, sub: &str) {
        let name = wide(sub);
        let mut hkey: HKEY = std::ptr::null_mut();
        // SAFETY: a NUL-terminated name; default security; the key is closed
        // below.
        let rc = unsafe {
            RegCreateKeyExW(
                hive.root(),
                name.as_ptr(),
                0,
                std::ptr::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_READ | KEY_WOW64_64KEY,
                std::ptr::null(),
                &mut hkey,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(rc, ERROR_SUCCESS, "making {sub}");
        // SAFETY: the key opened above, closed once.
        unsafe { RegCloseKey(hkey) };
    }

    /// Open `path` for everything this process may do to it.
    pub fn opened(path: &KeyPath) -> Option<Key> {
        open(
            path.hive.root(),
            &format!("{}\\{}", path.parent, path.name),
            KEY_ALL_ACCESS,
            path.shown(),
        )
        .expect("opening the key")
    }

    /// The owner and DACL of `sub` below `hive` in SDDL (`O:…D:…`).
    pub fn sddl(hive: Hive, sub: &str) -> String {
        let key = open(hive.root(), sub, KEY_READ, sub.to_string())
            .expect("opening the key")
            .expect("the key is there");
        let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        // SAFETY: a live key opened with READ_CONTROL; `sd` is LocalAlloc'd,
        // freed below; the other out-params are optional.
        let rc = unsafe {
            GetSecurityInfo(
                key.handle(),
                SE_REGISTRY_KEY,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut sd,
            )
        };
        assert_eq!(rc, ERROR_SUCCESS, "reading {sub}'s security");
        let mut text: windows_sys::core::PWSTR = std::ptr::null_mut();
        // SAFETY: the descriptor read above; `text` is LocalAlloc'd, freed
        // below.
        let ok = unsafe {
            ConvertSecurityDescriptorToStringSecurityDescriptorW(
                sd,
                SDDL_REVISION_1,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                &mut text,
                std::ptr::null_mut(),
            )
        };
        assert_ne!(ok, 0, "SDDL of {sub}");
        // SAFETY: the NUL-terminated string the call above wrote.
        let len = (0..).take_while(|&i| unsafe { *text.add(i) } != 0).count();
        // SAFETY: `len` units were just read from `text`.
        let s = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, len) });
        // SAFETY: both LocalAlloc'd by the calls above, freed once.
        unsafe {
            LocalFree(text as _);
            LocalFree(sd as _);
        }
        s
    }

    /// The value names of `path`, sorted; empty when there is no such key.
    pub fn names(path: &KeyPath) -> Vec<String> {
        let mut names = opened(path)
            .map(|k| k.names().expect("listing the values"))
            .unwrap_or_default();
        names.sort();
        names
    }

    /// The value `name` of `path`.
    pub fn get(path: &KeyPath, name: &str) -> Option<String> {
        opened(path).and_then(|k| k.read(name, 1 << 20).expect("reading a value"))
    }

    /// Set the value `name` of `path` to `text`.
    pub fn put(path: &KeyPath, name: &str, text: &str) {
        opened(path)
            .expect("the key is there")
            .write(name, text)
            .expect("writing a value");
    }

    /// Remove `sub` below `hive`, the key and everything in it, each key
    /// through its own handle and never through a registry link (a link is
    /// removed itself, and what it leads to is left); not being there is
    /// fine.
    pub fn remove_tree(hive: Hive, sub: &str) {
        if let Some(hkey) = open_itself(hive.root(), sub) {
            delete_with_all_below(hkey, sub);
        }
        assert!(open_itself(hive.root(), sub).is_none(), "{sub} is gone");
    }

    /// Remove `sub` below `hive` when it holds nothing at all: a key another
    /// test still uses below it stays.
    pub fn remove_if_empty(hive: Hive, sub: &str) {
        let name = wide(sub);
        // SAFETY: a NUL-terminated name. `RegDeleteKeyExW` refuses a key that
        // has subkeys, which is the point.
        unsafe { RegDeleteKeyExW(hive.root(), name.as_ptr(), KEY_WOW64_64KEY, 0) };
    }

    /// `sub` below `root` itself, a registry link included, for everything
    /// this process may do to it. `None`: it is not there.
    fn open_itself(root: HKEY, sub: &str) -> Option<HKEY> {
        let name = wide(sub);
        let mut hkey: HKEY = std::ptr::null_mut();
        // SAFETY: a NUL-terminated name; `hkey` is closed by the caller.
        let rc = unsafe {
            RegOpenKeyExW(
                root,
                name.as_ptr(),
                REG_OPTION_OPEN_LINK,
                KEY_ALL_ACCESS | KEY_WOW64_64KEY,
                &mut hkey,
            )
        };
        match rc {
            ERROR_SUCCESS => Some(hkey),
            ERROR_FILE_NOT_FOUND => None,
            rc => panic!("opening {sub}: {rc}"),
        }
    }

    /// Delete `hkey` and every key below it, each through its own handle;
    /// closes `hkey`.
    fn delete_with_all_below(hkey: HKEY, shown: &str) {
        loop {
            let mut buf = [0u16; 256];
            let mut len = buf.len() as u32;
            // SAFETY: a live key opened with KEY_ENUMERATE_SUB_KEYS; `buf`
            // holds `len` characters, and 255 is the longest key name.
            let rc = unsafe {
                RegEnumKeyExW(
                    hkey,
                    0,
                    buf.as_mut_ptr(),
                    &mut len,
                    std::ptr::null(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            if rc == ERROR_NO_MORE_ITEMS {
                break;
            }
            assert_eq!(rc, ERROR_SUCCESS, "listing what {shown} holds");
            let name = String::from_utf16_lossy(&buf[..len as usize]);
            let child = open_itself(hkey, &name).expect("a key just listed");
            delete_with_all_below(child, &format!("{shown}\\{name}"));
        }
        // SAFETY: a live key opened with DELETE; closed once, after.
        let status = unsafe { NtDeleteKey(hkey) };
        // SAFETY: as above.
        unsafe { RegCloseKey(hkey) };
        assert!(status >= 0, "deleting {shown}: {status:#x}");
    }

    /// The kernel's name of `sub` below `hive`, which a registry link names
    /// its target by: `\REGISTRY\USER\<this account's SID>\…` for the
    /// person's own hive.
    pub fn kernel_name(hive: Hive, sub: &str) -> String {
        match hive {
            Hive::LocalMachine => format!(r"\REGISTRY\MACHINE\{sub}"),
            Hive::CurrentUser => format!(
                r"\REGISTRY\USER\{}\{sub}",
                crate::machine_tree::test_support::me()
            ),
        }
    }

    /// Make `sub` below `hive` a registry link to `target`, a kernel name
    /// ([`kernel_name`]). It must not be there yet.
    pub fn link(hive: Hive, sub: &str, target: &str) {
        let name = wide(sub);
        let mut hkey: HKEY = std::ptr::null_mut();
        let mut disposition = 0u32;
        // SAFETY: a NUL-terminated name; the key is closed below.
        let rc = unsafe {
            RegCreateKeyExW(
                hive.root(),
                name.as_ptr(),
                0,
                std::ptr::null(),
                REG_OPTION_CREATE_LINK,
                KEY_ALL_ACCESS | KEY_WOW64_64KEY,
                std::ptr::null(),
                &mut hkey,
                &mut disposition,
            )
        };
        assert_eq!(rc, ERROR_SUCCESS, "making the link {sub}");
        let value = wide("SymbolicLinkValue");
        let to: Vec<u16> = target.encode_utf16().collect();
        // SAFETY: a key just made with KEY_SET_VALUE; a link's target is
        // its name without a NUL, sized in bytes.
        let rc = unsafe {
            RegSetValueExW(
                hkey,
                value.as_ptr(),
                0,
                REG_LINK,
                to.as_ptr().cast(),
                (to.len() * 2) as u32,
            )
        };
        // SAFETY: the key made above, closed once.
        unsafe { RegCloseKey(hkey) };
        assert_eq!(disposition, 1, "{sub} was made here, not found");
        assert_eq!(rc, ERROR_SUCCESS, "pointing the link {sub} at {target}");
    }

    /// Set the `REG_SZ` value of `path` whose name is exactly `name`, which
    /// may hold a NUL: a name the Win32 calls cannot spell.
    pub fn put_raw(path: &KeyPath, name: &[u16], text: &str) {
        let key = opened(path).expect("the key is there");
        let mut name = name.to_vec();
        let bytes = (name.len() * 2) as u16;
        let counted = UNICODE_STRING {
            Length: bytes,
            MaximumLength: bytes,
            Buffer: name.as_mut_ptr(),
        };
        let data = wide(text);
        // SAFETY: a live key opened with KEY_SET_VALUE; `counted` points into
        // `name`, and `data` is the REG_SZ with its NUL, sized in bytes.
        let status = unsafe {
            NtSetValueKey(
                key.handle(),
                &counted,
                0,
                REG_SZ,
                data.as_ptr().cast(),
                (data.len() * 2) as u32,
            )
        };
        assert!(status >= 0, "setting a value: {status:#x}");
    }

    /// How many values `path` holds, whatever their names; 0 when there is no
    /// such key.
    pub fn value_count(path: &KeyPath) -> u32 {
        let Some(key) = opened(path) else {
            return 0;
        };
        let mut values = 0u32;
        // SAFETY: a live key opened with KEY_QUERY_VALUE; every out-param
        // but the value count is optional.
        let rc = unsafe {
            RegQueryInfoKeyW(
                key.handle() as HKEY,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut values,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(rc, ERROR_SUCCESS, "counting what the key holds");
        values
    }

    /// The owner of `sub` below `hive`, as a SID (`S-1-5-32-544`).
    pub fn owner_sid(hive: Hive, sub: &str) -> String {
        let key = open(hive.root(), sub, KEY_READ, sub.to_string())
            .expect("opening the key")
            .expect("the key is there");
        crate::machine_tree::security_of(key.handle(), SE_REGISTRY_KEY)
            .expect("reading its security")
            .owner
    }

    /// Stop every later takeover in this process after its step `step`
    /// (`0`: none) with an error, as a crash there would.
    pub fn stop_takeover_after(step: usize) {
        super::STOP_TAKEOVER_AFTER.store(step, std::sync::atomic::Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::machine_tree::{
        ACE_ALLOW, ACE_DENY, ADMINISTRATORS, Ace, CONTAINER_INHERIT, INHERIT_ONLY, INHERITED,
        SYSTEM, Sd, TRUSTED_INSTALLER,
    };

    const USERS: &str = "S-1-5-32-545";
    const PERSON: &str = "S-1-5-21-1-2-3-1001";
    const CREATOR_OWNER: &str = "S-1-3-0";
    /// ALL APPLICATION PACKAGES.
    const APP_PACKAGES: &str = "S-1-15-2-1";
    /// An app capability SID, as `HKLM\SOFTWARE` carries one with read.
    const CAPABILITY: &str = "S-1-15-3-1024-1065365936-1281604716-3511738428-1654721687";
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const GENERIC_ALL: u32 = 0x1000_0000;

    fn ace(kind: u8, flags: u8, mask: u32, sid: &str) -> Ace {
        Ace {
            kind,
            flags,
            mask,
            sid: sid.into(),
        }
    }

    fn sd(owner: &str, aces: Vec<Ace>) -> Sd {
        Sd {
            owner: owner.into(),
            protected: true,
            dacl: Some(aces),
        }
    }

    /// What the writer makes: SYSTEM and Administrators, full control.
    fn sealed() -> Vec<Ace> {
        vec![
            ace(ACE_ALLOW, CONTAINER_INHERIT, KEY_ALL_ACCESS, SYSTEM),
            ace(ACE_ALLOW, CONTAINER_INHERIT, KEY_ALL_ACCESS, ADMINISTRATORS),
        ]
    }

    /// R1 — the key holds the service's credential, so another account may
    /// neither change it nor read it, and the owner decides the DACL.
    #[test]
    fn key_verdict_refuses_what_another_account_may_change_or_read() {
        let what = "the key";
        assert_eq!(
            verdict(&sd(ADMINISTRATORS, sealed()), KEY_SECRET, what),
            Ok(())
        );
        let mut with_ti = sealed();
        with_ti.push(ace(
            ACE_ALLOW,
            CONTAINER_INHERIT,
            KEY_ALL_ACCESS,
            TRUSTED_INSTALLER,
        ));
        assert_eq!(
            verdict(&sd(SYSTEM, with_ti.clone()), KEY_SECRET, what),
            Ok(())
        );
        assert_eq!(
            verdict(&sd(TRUSTED_INSTALLER, with_ti), KEY_SECRET, what),
            Ok(()),
            "TrustedInstaller is an administrator"
        );
        assert!(
            verdict(&sd(PERSON, sealed()), KEY_SECRET, what).is_err(),
            "an owner other than SYSTEM or Administrators may rewrite the DACL"
        );
        for (mask, right) in [
            (KEY_SET_VALUE, "set a value"),
            (KEY_CREATE_SUB_KEY, "make a subkey"),
            (KEY_CREATE_LINK, "make a link"),
            (WRITE_DAC, "rewrite the DACL"),
            (WRITE_OWNER, "take the key"),
            (DELETE, "delete the key"),
            (GENERIC_WRITE, "GENERIC_WRITE"),
            (GENERIC_ALL, "GENERIC_ALL"),
            (KEY_QUERY_VALUE, "read a value"),
            (KEY_READ, "KR"),
        ] {
            let mut aces = sealed();
            aces.push(ace(ACE_ALLOW, CONTAINER_INHERIT, mask, USERS));
            assert!(
                verdict(&sd(ADMINISTRATORS, aces), KEY_SECRET, what).is_err(),
                "Users may {right}"
            );
        }
        let mut inherited = sealed();
        inherited.push(ace(ACE_ALLOW, INHERITED, KEY_READ, USERS));
        assert!(
            verdict(&sd(ADMINISTRATORS, inherited), KEY_SECRET, what).is_err(),
            "an inherited read applies to the key"
        );
        let mut enumerate = sealed();
        enumerate.push(ace(
            ACE_ALLOW,
            0,
            KEY_ENUMERATE_SUB_KEYS | KEY_NOTIFY | READ_CONTROL,
            USERS,
        ));
        assert_eq!(
            verdict(&sd(ADMINISTRATORS, enumerate), KEY_SECRET, what),
            Ok(()),
            "listing subkeys and reading the DACL show no value"
        );
        let null = Sd {
            owner: ADMINISTRATORS.into(),
            protected: true,
            dacl: None,
        };
        assert!(verdict(&null, KEY_SECRET, what).is_err(), "a NULL DACL");
        let mut odd = sealed();
        odd.push(ace(0x11, 0, 0, ""));
        assert!(
            verdict(&sd(ADMINISTRATORS, odd), KEY_SECRET, what).is_err(),
            "an ACE of a kind the check does not read"
        );
        let mut denied = sealed();
        denied.insert(0, ace(ACE_DENY, 0, KEY_ALL_ACCESS, USERS));
        denied.push(ace(ACE_ALLOW, 0, KEY_SET_VALUE, USERS));
        assert!(
            verdict(&sd(ADMINISTRATORS, denied), KEY_SECRET, what).is_err(),
            "a deny beside an allow is never credited"
        );
        let mut handed_down = sealed();
        handed_down.push(ace(
            ACE_ALLOW,
            CONTAINER_INHERIT | INHERIT_ONLY,
            KEY_ALL_ACCESS,
            USERS,
        ));
        assert_eq!(
            verdict(&sd(ADMINISTRATORS, handed_down), KEY_SECRET, what),
            Ok(()),
            "an inherit-only ACE does not apply to the key"
        );
    }

    /// R2 — the parent is judged by what it lets another account change,
    /// counting what it hands down to a key made below it later; reading it
    /// is fine, as `HKLM\SOFTWARE` lets Users and app packages read.
    #[test]
    fn the_parent_key_counts_what_it_hands_down() {
        let what = "the parent";
        let machine = vec![
            ace(
                ACE_ALLOW,
                CONTAINER_INHERIT | INHERIT_ONLY,
                KEY_ALL_ACCESS,
                CREATOR_OWNER,
            ),
            ace(ACE_ALLOW, CONTAINER_INHERIT, KEY_ALL_ACCESS, SYSTEM),
            ace(ACE_ALLOW, CONTAINER_INHERIT, KEY_ALL_ACCESS, ADMINISTRATORS),
            ace(ACE_ALLOW, CONTAINER_INHERIT, KEY_READ, USERS),
            ace(ACE_ALLOW, CONTAINER_INHERIT, KEY_READ, APP_PACKAGES),
            ace(ACE_ALLOW, CONTAINER_INHERIT, KEY_READ, CAPABILITY),
        ];
        assert_eq!(
            parent_verdict(&sd(ADMINISTRATORS, machine.clone()), what),
            Ok(()),
            "HKLM\\SOFTWARE's own entries"
        );
        let mut software = machine.clone();
        software[0] = ace(ACE_ALLOW, CONTAINER_INHERIT, GENERIC_ALL, CREATOR_OWNER);
        assert_eq!(
            parent_verdict(&sd(SYSTEM, software), what),
            Ok(()),
            "CREATOR OWNER hands the creator's rights to the creator, whoever may create"
        );
        let mut handed = machine.clone();
        handed.push(ace(
            ACE_ALLOW,
            CONTAINER_INHERIT | INHERIT_ONLY,
            KEY_WRITE,
            USERS,
        ));
        assert!(
            parent_verdict(&sd(ADMINISTRATORS, handed), what).is_err(),
            "an inherit-only KW for Users reaches a key made below it later"
        );
        let mut generic = machine.clone();
        generic.push(ace(
            ACE_ALLOW,
            CONTAINER_INHERIT | INHERIT_ONLY,
            GENERIC_WRITE,
            USERS,
        ));
        assert!(
            parent_verdict(&sd(ADMINISTRATORS, generic), what).is_err(),
            "a generic right handed down"
        );
        for mask in [
            KEY_SET_VALUE,
            KEY_CREATE_SUB_KEY,
            KEY_CREATE_LINK,
            WRITE_DAC,
            DELETE,
        ] {
            let mut aces = machine.clone();
            aces.push(ace(ACE_ALLOW, 0, mask, PERSON));
            assert!(
                parent_verdict(&sd(ADMINISTRATORS, aces), what).is_err(),
                "a person with {mask:#x} on the parent"
            );
        }
        assert!(
            parent_verdict(&sd(PERSON, machine.clone()), what).is_err(),
            "a parent another account owns"
        );
        let null = Sd {
            owner: SYSTEM.into(),
            protected: false,
            dacl: None,
        };
        assert!(parent_verdict(&null, what).is_err(), "a NULL DACL");
        let mut odd = machine;
        odd.push(ace(0x11, 0, 0, ""));
        assert!(
            parent_verdict(&sd(ADMINISTRATORS, odd), what).is_err(),
            "an ACE of a kind the check does not read"
        );
    }

    #[test]
    fn generic_rights_map_to_key_rights() {
        assert_eq!(map_generic_key(0x8000_0000), KEY_READ);
        assert_eq!(map_generic_key(GENERIC_WRITE), KEY_WRITE);
        assert_eq!(map_generic_key(0x2000_0000), KEY_READ);
        assert_eq!(map_generic_key(GENERIC_ALL), KEY_ALL_ACCESS);
        assert_eq!(map_generic_key(KEY_QUERY_VALUE), KEY_QUERY_VALUE);
    }
}
