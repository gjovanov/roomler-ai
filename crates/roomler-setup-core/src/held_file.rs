// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! #3246 — a staged artifact, held open from the check to the run.
//!
//! A check followed by a re-open by path says nothing about what the second
//! open finds. A [`HeldFile`] is opened once with write and delete sharing
//! refused, so for as long as it lives nobody can write the file, rename it or
//! delete it, while anyone may still read it: `WinVerifyTrust`, `msiexec`, the
//! Windows Installer service. Every check that follows reads through this one
//! handle.
//!
//! The hold is one half of the guarantee. A process of the same user can end
//! the process that holds it, so across an elevation the other half is where
//! the file sits: `agents/roomler-setup/src/package_step.rs`.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::time::Duration;

use anyhow::{Result, anyhow};
use sha2::{Digest, Sha256};

/// How long to wait for a writer to let go. A download's own handle has just
/// closed, and an antivirus filter may still be looking at the file.
const SHARING_RETRY: Duration = Duration::from_millis(200);
const SHARING_RETRIES: u32 = 25;

/// A file opened for reading, with writers and deleters refused until it drops.
#[derive(Debug)]
pub struct HeldFile {
    file: File,
}

impl HeldFile {
    /// Open `path` and hold it. A link at the path is refused, never followed:
    /// what is checked is the file that was named. A writer that keeps the file
    /// open past the retries is an error, so a successful open also says that
    /// no handle can write it now.
    pub fn open(path: &Path) -> std::io::Result<HeldFile> {
        let mut tries = 0;
        loop {
            match open_held(path) {
                Ok(file) => return Ok(HeldFile { file }),
                Err(e) if is_sharing_violation(&e) && tries < SHARING_RETRIES => {
                    tries += 1;
                    std::thread::sleep(SHARING_RETRY);
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// The handle, for a check that takes one.
    pub fn file(&self) -> &File {
        &self.file
    }

    /// Does what this handle reads hash to `expected_digest`? The digest format
    /// is [`crate::asset_resolver::verify_sha256`]'s; `Err` on a malformed one.
    pub fn sha256_matches(&mut self, expected_digest: &str) -> Result<bool> {
        let hex = crate::asset_resolver::digest_hex(expected_digest)?;
        self.file.seek(SeekFrom::Start(0))?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = self.file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        self.file.seek(SeekFrom::Start(0))?;
        Ok(crate::asset_resolver::hex_encode(&hasher.finalize()).eq_ignore_ascii_case(hex))
    }
}

#[cfg(target_os = "windows")]
fn open_held(path: &Path) -> std::io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    // Not re-exported where this crate's windows-sys features reach, and fixed
    // by the Win32 ABI.
    const FILE_SHARE_READ: u32 = 0x1;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    refuse_unless_plain_file(&file, path)?;
    Ok(file)
}

/// Off Windows there are no share modes; the Linux and macOS installs take
/// their guarantee from the privileged half's own verification.
#[cfg(not(target_os = "windows"))]
fn open_held(path: &Path) -> std::io::Result<File> {
    let file = File::open(path)?;
    refuse_unless_plain_file(&file, path)?;
    Ok(file)
}

fn refuse_unless_plain_file(file: &File, path: &Path) -> std::io::Result<()> {
    if file.metadata()?.is_file() {
        return Ok(());
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        anyhow!("{} is not a plain file", path.display()).to_string(),
    ))
}

#[cfg(target_os = "windows")]
fn is_sharing_violation(e: &std::io::Error) -> bool {
    // ERROR_SHARING_VIOLATION
    e.raw_os_error() == Some(32)
}

#[cfg(not(target_os = "windows"))]
fn is_sharing_violation(_e: &std::io::Error) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    const HELLO_SHA256: &str =
        "sha256:2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

    #[test]
    fn the_digest_is_read_through_the_handle() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.bin");
        std::fs::write(&path, b"hello").unwrap();
        let mut held = HeldFile::open(&path).unwrap();
        assert!(held.sha256_matches(HELLO_SHA256).unwrap());
        assert!(
            held.sha256_matches(HELLO_SHA256).unwrap(),
            "and again: a check leaves the handle where it found it"
        );
        assert!(
            !held
                .sha256_matches(
                    "sha256:0000000000000000000000000000000000000000000000000000000000000000"
                )
                .unwrap()
        );
        assert!(held.sha256_matches("sha256:abc").is_err(), "malformed");
    }

    #[test]
    fn a_folder_is_not_held() {
        let dir = tempfile::tempdir().unwrap();
        assert!(HeldFile::open(dir.path()).is_err());
        assert!(HeldFile::open(&dir.path().join("no-such-file")).is_err());
    }

    /// What the hold is for. While it lives nobody can write the file, rename
    /// it or delete it, and anyone can read it, which is how `msiexec` and the
    /// Windows Installer service open a package. After it, all of that works.
    #[cfg(target_os = "windows")]
    #[test]
    fn while_held_a_file_can_be_read_and_nothing_else() {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ: u32 = 0x1;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.msi");
        let aside = dir.path().join("b.msi");
        std::fs::write(&path, b"hello").unwrap();

        let held = HeldFile::open(&path).unwrap();
        // Copy-Item -Force, and an open that keeps writing.
        assert!(std::fs::write(&path, b"swapped").is_err());
        assert!(std::fs::OpenOptions::new().write(true).open(&path).is_err());
        // The swap everyone reaches for next.
        assert!(std::fs::rename(&path, &aside).is_err());
        assert!(std::fs::remove_file(&path).is_err());
        // A rename over it, from a file of the attacker's own.
        std::fs::write(&aside, b"swapped").unwrap();
        assert!(std::fs::rename(&aside, &path).is_err());
        // A reader that shares what the Installer's own open shares.
        let mut text = String::new();
        std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(&path)
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        assert_eq!(text, "hello");
        // The copy that places the companion, and the step's read of a limited
        // wizard's download.
        std::fs::copy(&path, dir.path().join("copied.msi")).unwrap();
        File::open(&path)
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        // A second hold, as the check and the run may each take.
        drop(HeldFile::open(&path).unwrap());

        drop(held);
        std::fs::remove_file(&path).unwrap();
    }

    /// A writer that does not let go is a refusal: holding the file would be a
    /// claim nobody can write it.
    #[cfg(target_os = "windows")]
    #[test]
    fn a_file_someone_still_writes_is_not_held() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.msi");
        let writer = File::create(&path).unwrap();
        let e = HeldFile::open(&path).unwrap_err();
        assert!(is_sharing_violation(&e), "{e}");
        drop(writer);
        assert!(HeldFile::open(&path).is_ok());
    }

    /// A link at the name is refused and not followed.
    #[cfg(target_os = "windows")]
    #[test]
    fn a_junction_at_the_name_is_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        std::fs::create_dir(&target).unwrap();
        let link = dir.path().join("a.msi");
        let cmd = std::env::var_os("ComSpec").expect("ComSpec");
        let made = std::process::Command::new(cmd)
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(&target)
            .output()
            .unwrap();
        assert!(made.status.success(), "mklink: {made:?}");
        assert!(HeldFile::open(&link).is_err());
    }
}
