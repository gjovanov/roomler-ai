// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! A thread's filesystem identity: work a root daemon does in a person's
//! folder runs with the thread's fsuid, fsgid and supplementary groups
//! switched to the person's ([`FsIdentity`]). Linux checks every open, create
//! and unlink against those, wherever a path turns out to lead, so a link the
//! person planted in their own folder is followed with their rights, never
//! root's.
//!
//! FR-85 wrote this for recordings (`recording::launch`). FR-97 P3d moved it
//! here because a received file needs it too, and a file transfer is not
//! behind the `recording` feature.

/// This thread's supplementary groups (the calling thread's own: on Linux
/// credentials belong to a thread). On macOS only a recording's identity
/// report reads them.
#[cfg(any(target_os = "linux", feature = "recording"))]
pub(crate) fn own_groups() -> Vec<libc::gid_t> {
    // SAFETY: a count query, then a fill of a buffer of that size.
    let n = unsafe { libc::getgroups(0, std::ptr::null_mut()) };
    if n <= 0 {
        return Vec::new();
    }
    let mut groups = vec![0 as libc::gid_t; n as usize];
    // SAFETY: `groups` holds `n` entries.
    let n = unsafe { libc::getgroups(n, groups.as_mut_ptr()) };
    groups.truncate(n.max(0) as usize);
    groups
}

#[cfg(target_os = "linux")]
pub(crate) use linux::fsuid;
#[cfg(target_os = "linux")]
pub(crate) use linux::{Account, FsIdentity};

#[cfg(target_os = "linux")]
mod linux {
    use std::marker::PhantomData;

    /// An account, resolved the way the privilege drop resolves one
    /// (`exec`, uid 0 refused).
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) struct Account {
        pub(crate) uid: libc::uid_t,
        pub(crate) gid: libc::gid_t,
        pub(crate) groups: Vec<libc::gid_t>,
        pub(crate) name: String,
    }

    impl Account {
        pub(crate) fn by_name(name: &str) -> Result<Self, String> {
            let (uid, gid, groups) = crate::exec::account_ids(name)?;
            Ok(Self {
                uid,
                gid,
                groups,
                name: name.to_string(),
            })
        }

        /// The recorder's lookup: it is handed a uid (`recording::launch`).
        #[cfg(feature = "recording")]
        pub(crate) fn by_uid(uid: u32) -> Result<Self, String> {
            let account = Self::by_name(&crate::exec::account_name(uid)?)?;
            if account.uid != uid {
                return Err(format!(
                    "uid {uid} resolved to {}, whose uid is {}",
                    account.name, account.uid
                ));
            }
            Ok(account)
        }
    }

    /// Set THIS thread's supplementary groups. The raw syscall on purpose:
    /// glibc's `setgroups` changes every thread of the process (POSIX asks
    /// for that), and this must change one.
    fn set_thread_groups(groups: &[libc::gid_t]) -> std::io::Result<()> {
        // SAFETY: the kernel reads `len` gids from a live slice.
        let rc = unsafe { libc::syscall(libc::SYS_setgroups, groups.len(), groups.as_ptr()) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    /// The thread's fsuid. `setfsuid` returns the previous value and changes
    /// nothing for an id it refuses, so asking for an impossible one reads it.
    pub(crate) fn fsuid() -> libc::uid_t {
        // SAFETY: `uid_t::MAX` is never a valid uid; nothing changes.
        unsafe { libc::setfsuid(libc::uid_t::MAX) as libc::uid_t }
    }

    fn fsgid() -> libc::gid_t {
        // SAFETY: as `fsuid`.
        unsafe { libc::setfsgid(libc::gid_t::MAX) as libc::gid_t }
    }

    /// One word of the kernel's capability sets, version 3
    /// (`struct __user_cap_data_struct`): the first holds capabilities 0-31,
    /// the second 32-63.
    #[repr(C)]
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    struct CapData {
        effective: u32,
        permitted: u32,
        inheritable: u32,
    }

    /// `struct __user_cap_header_struct`.
    #[repr(C)]
    struct CapHeader {
        version: u32,
        pid: libc::c_int,
    }

    /// `_LINUX_CAPABILITY_VERSION_3`: 64-bit sets, as two [`CapData`].
    const CAPABILITY_VERSION_3: u32 = 0x2008_0522;

    /// THIS thread's capability sets (pid 0 is the caller).
    fn thread_caps() -> std::io::Result<[CapData; 2]> {
        let mut header = CapHeader {
            version: CAPABILITY_VERSION_3,
            pid: 0,
        };
        let mut sets = [CapData::default(); 2];
        // SAFETY: version 3 writes two `CapData`, which `sets` holds.
        let rc = unsafe {
            libc::syscall(
                libc::SYS_capget,
                &mut header as *mut CapHeader,
                sets.as_mut_ptr(),
            )
        };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(sets)
    }

    /// Set THIS thread's capability sets. The raw syscall, as
    /// [`set_thread_groups`]: nothing must reach the other threads, and
    /// pid 0 is the caller.
    fn set_thread_caps(sets: &[CapData; 2]) -> std::io::Result<()> {
        let mut header = CapHeader {
            version: CAPABILITY_VERSION_3,
            pid: 0,
        };
        // SAFETY: version 3 reads two `CapData` from a live array.
        let rc = unsafe {
            libc::syscall(
                libc::SYS_capset,
                &mut header as *mut CapHeader,
                sets.as_ptr(),
            )
        };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    fn same_groups(mut a: Vec<libc::gid_t>, b: &[libc::gid_t]) -> bool {
        let mut b = b.to_vec();
        a.sort_unstable();
        a.dedup();
        b.sort_unstable();
        b.dedup();
        a == b
    }

    /// THIS thread's filesystem identity, an account's until dropped: its
    /// fsuid, fsgid and supplementary groups. Linux checks every open,
    /// create and unlink against those, and the capabilities that let root
    /// skip the checks (`CAP_DAC_OVERRIDE`, `CAP_DAC_READ_SEARCH`,
    /// `CAP_FOWNER`, …) leave the thread's effective set while its fsuid is
    /// not 0 and come back with it. The Linux form of the Windows
    /// impersonation (`recording::launch::win::Impersonating`).
    ///
    /// Not `Send`: an identity belongs to the thread that took it on.
    pub(crate) struct FsIdentity {
        fsuid: libc::uid_t,
        fsgid: libc::gid_t,
        groups: Vec<libc::gid_t>,
        /// #3688 — the capability sets a read's guard gives back
        /// ([`Self::begin_reading`]); `None` for [`Self::begin`], whose
        /// guard leaves them to the kernel's fsuid rule.
        caps: Option<[CapData; 2]>,
        _thread: PhantomData<*const ()>,
    }

    impl FsIdentity {
        pub(crate) fn begin(account: &Account) -> Result<Self, String> {
            // What to return to, read before anything changes, so a guard
            // taken by a process that is not root puts back what it found.
            let back = Self {
                fsuid: fsuid(),
                fsgid: fsgid(),
                groups: super::own_groups(),
                caps: None,
                _thread: PhantomData,
            };
            set_thread_groups(&account.groups)
                .map_err(|e| format!("taking on {}'s groups: {e}", account.name))?;
            // SAFETY: plain credential syscalls on this thread; checked below.
            unsafe {
                libc::setfsgid(account.gid);
                libc::setfsuid(account.uid);
            }
            // Verified, never assumed: a thread that is still root, or still
            // in root's group, would read a link planted in the folder with
            // root's rights. `back` restores whatever did change.
            if fsuid() != account.uid
                || fsgid() != account.gid
                || !same_groups(super::own_groups(), &account.groups)
            {
                return Err(format!(
                    "the thread did not take on {}'s identity",
                    account.name
                ));
            }
            Ok(back)
        }

        /// #3688 — [`Self::begin`] for a read, which may name any path: the
        /// thread's effective capabilities are emptied too, until dropped.
        /// An fsuid that is not 0 takes away only the file-system ones, and
        /// the rest (`CAP_SYS_PTRACE`, `CAP_SYSLOG`, …) would still answer
        /// for root wherever a check asks for one. With none, the thread
        /// opens a file with the person's rights alone, though its euid
        /// stays 0. The permitted and inheritable sets stay as they are, so
        /// the drop can raise the effective set again.
        pub(crate) fn begin_reading(account: &Account) -> Result<Self, String> {
            // Read before anything changes: what the drop gives back.
            let caps =
                thread_caps().map_err(|e| format!("reading the thread's capabilities: {e}"))?;
            let mut guard = Self::begin(account)?;
            // Held before they change, so a failure below gives them back.
            guard.caps = Some(caps);
            let mut empty = caps;
            for set in &mut empty {
                set.effective = 0;
            }
            set_thread_caps(&empty)
                .map_err(|e| format!("emptying the thread's capabilities: {e}"))?;
            // Verified, never assumed, as the identity is.
            if !thread_caps().is_ok_and(|now| now.iter().all(|set| set.effective == 0)) {
                return Err("the thread kept capabilities it was to give up".to_string());
            }
            Ok(guard)
        }
    }

    impl Drop for FsIdentity {
        fn drop(&mut self) {
            // #3688 — a read's capabilities come back first: putting the
            // groups back needs CAP_SETGID.
            if let Some(caps) = &self.caps {
                let _ = set_thread_caps(caps);
            }
            // SAFETY: plain credential syscalls on this thread; checked below.
            unsafe {
                libc::setfsuid(self.fsuid);
                libc::setfsgid(self.fsgid);
            }
            let groups_back = set_thread_groups(&self.groups).is_ok();
            // And again last, checked: an fsuid back at 0 raises every
            // file-system capability the permitted set holds, which the
            // effective set given back may not have had.
            let caps_back = match &self.caps {
                None => true,
                Some(caps) => {
                    let _ = set_thread_caps(caps);
                    thread_caps().is_ok_and(|now| now == *caps)
                }
            };
            if fsuid() != self.fsuid
                || fsgid() != self.fsgid
                || !groups_back
                || !same_groups(super::own_groups(), &self.groups)
                || !caps_back
            {
                // A pool thread left as someone else would take that identity
                // into whatever work it runs next. Nothing is safe after this.
                // FR-97 P6e (#1538) — never `eprintln!`: on a failing stderr
                // it panics, and the thread unwinds instead of the abort.
                crate::logging::say_on_stderr(
                    "a thread could not become the daemon again after acting as the person \
                     signed in; aborting",
                );
                std::process::abort();
            }
        }
    }
}
