// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! #3355 — the bytes this process may still write in a folder, as the operating
//! system answers for that folder itself.
//!
//! The recorder's free-space probe (`recording::folder::available_space`) asks
//! here. It used to match the folder against `sysinfo`'s mount list, which leaves
//! out tmpfs, NFS and CIFS on Linux, every volume that is not local on macOS, and
//! mapped network drives on Windows. A folder on one of those read the free space
//! of `/`, or nothing at all. A query about the folder itself has no list to leave
//! a mount out of, and it touches the folder's own mount only.
//!
//! | OS | query | bytes |
//! |---|---|---|
//! | macOS | `statfs` | `f_bavail` × `f_bsize` |
//! | other Unix | `statvfs` | `f_bavail` × `f_frsize` |
//! | Windows | `GetDiskFreeSpaceExW` on the folder, a share's included | the free bytes available to the caller |
//!
//! ⚠️ On macOS, `statfs` and never `statvfs`: the Mac's `statvfs` counts blocks
//! in 32 bits, and its `statfs` in 64.
//! ⚠️ With `statvfs`, `f_frsize` and never `f_bsize`: `f_bavail` counts
//! fragments, and the two sizes differ on some file systems.
//!
//! `None` means only that the query failed, for example because the folder is
//! gone. The query resolves the path the way an open does, and nothing here
//! canonicalizes it. A folder whose file system has stopped answering (a hard NFS
//! mount whose server is gone) can block the query, as it blocks every write
//! there, so a caller on a paced loop asks from another thread.
//!
//! Compiled on every OS, outside `recording`, so that every OS's unit-test lane
//! runs this module's tests.

use std::path::Path;

/// The bytes this process may still write in `dir`, or `None` when the
/// operating system cannot say.
#[cfg(target_os = "macos")]
pub fn available(dir: &Path) -> Option<u64> {
    let path = c_path(dir)?;
    let mut st = std::mem::MaybeUninit::<libc::statfs>::zeroed();
    // SAFETY: `path` is NUL-terminated and outlives the call, and `st` is a
    // zeroed struct of the type the call fills.
    if unsafe { libc::statfs(path.as_ptr(), st.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: zeroed, then filled by a call that succeeded.
    let st = unsafe { st.assume_init() };
    Some(bytes(st.f_bavail, st.f_bsize))
}

/// The bytes this process may still write in `dir`, or `None` when the
/// operating system cannot say.
#[cfg(all(unix, not(target_os = "macos")))]
pub fn available(dir: &Path) -> Option<u64> {
    let path = c_path(dir)?;
    let mut st = std::mem::MaybeUninit::<libc::statvfs>::zeroed();
    // SAFETY: `path` is NUL-terminated and outlives the call, and `st` is a
    // zeroed struct of the type the call fills.
    if unsafe { libc::statvfs(path.as_ptr(), st.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: zeroed, then filled by a call that succeeded.
    let st = unsafe { st.assume_init() };
    Some(bytes(st.f_bavail, st.f_frsize))
}

/// The bytes this process may still write in `dir`, or `None` when the
/// operating system cannot say.
#[cfg(windows)]
pub fn available(dir: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let mut wide: Vec<u16> = dir.as_os_str().encode_wide().collect();
    // An empty path would ask about the current drive, and one holding a NUL
    // about the part before it: neither is the folder asked about.
    if wide.is_empty() || wide.contains(&0) {
        return None;
    }
    // A share must be named with a trailing separator (`\\server\share\`), and
    // any other folder takes one too.
    if !matches!(wide.last(), Some(&c) if c == u16::from(b'\\') || c == u16::from(b'/')) {
        wide.push(u16::from(b'\\'));
    }
    wide.push(0);
    let mut free = 0u64;
    // SAFETY: `wide` is NUL-terminated and outlives the call, `free` is a live
    // u64, and the two totals are optional, so they are null.
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut free,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    (ok != 0).then_some(free)
}

/// No query on this platform: the answer is "can't tell".
#[cfg(not(any(unix, windows)))]
pub fn available(_dir: &Path) -> Option<u64> {
    None
}

#[cfg(unix)]
fn c_path(dir: &Path) -> Option<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()
}

/// `blocks` × `size`, saturating. The fields' widths differ by target (macOS's
/// block size is a `u32`, 64-bit Linux's fields are `u64`), so each widens here,
/// where a cast would be a no-op on some targets.
#[cfg(unix)]
fn bytes(blocks: impl Into<u64>, size: impl Into<u64>) -> u64 {
    blocks.into().saturating_mul(size.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "roomler-3355-free-space-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_folder_reads_the_room_on_its_volume() {
        let dir = scratch("here");
        let free = available(&dir);
        assert!(free.is_some_and(|b| b > 0), "{}: {free:?}", dir.display());
        // With a trailing separator, the same folder.
        let mut with_sep = dir.into_os_string();
        with_sep.push(std::path::MAIN_SEPARATOR_STR);
        assert!(available(Path::new(&with_sep)).is_some());
    }

    /// "Can't tell" is the answer only when the query fails, and a folder that
    /// is gone is that case: never a number from some other folder.
    #[test]
    fn a_folder_that_is_gone_reads_as_cant_tell() {
        let gone = scratch("gone").join("not-there");
        assert_eq!(available(&gone), None);
    }

    #[test]
    fn a_path_that_names_no_folder_reads_as_cant_tell() {
        assert_eq!(available(Path::new("")), None);
        assert_eq!(available(Path::new("a\0b")), None);
    }

    #[cfg(unix)]
    #[test]
    fn the_product_saturates() {
        assert_eq!(bytes(3u64, 4096u32), 12_288);
        assert_eq!(bytes(u64::MAX, 4096u64), u64::MAX);
    }
}
