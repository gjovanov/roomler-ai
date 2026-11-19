// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! #3682 — the file drawer's reads, to their edges: what a listing or a
//! download touches, as whom, for how long, and what its refusal says.
//!
//! #3669 made a machine-wide Windows install read as the person signed in at
//! the console, one step at a time ([`FsAs::run`]). These tests pin the reads
//! that still ran outside such a step, or without a bound, or whose refusal
//! did not say what was refused and why.

use super::*;

/// The names a listing holds, in its order.
fn names(listing: &DirListing) -> Vec<String> {
    listing.entries.iter().map(|e| e.name.clone()).collect()
}

/// What the viewer reads of an answer: an error's outermost line, the only
/// one it shows (#3676).
fn outermost<T>(answer: Result<T>) -> String {
    match answer {
        Ok(_) => "answered".to_string(),
        Err(e) => format!("{e}"),
    }
}

/// The stand-ins these tests put where production code reads, each reached
/// from it under `#[cfg(test)]`, as [`stand_in::reader_now`] is.
pub(super) mod seams {
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    /// The folders resolving answers "access denied" at or under.
    static UNRESOLVABLE: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

    /// While held, every path at or under its folder answers resolving
    /// ([`super::super::reads::resolve_path`]) with an access refusal: error 5
    /// on Windows, where a runner with its backup privilege enabled resolves
    /// through any DACL, so no real folder can stand in.
    #[cfg(windows)]
    pub(crate) struct Unresolvable(PathBuf);

    #[cfg(windows)]
    pub(crate) fn unresolvable(folder: &Path) -> Unresolvable {
        let mut held = UNRESOLVABLE.lock().unwrap_or_else(|p| p.into_inner());
        held.push(folder.to_path_buf());
        Unresolvable(folder.to_path_buf())
    }

    #[cfg(windows)]
    impl Drop for Unresolvable {
        fn drop(&mut self) {
            let mut held = UNRESOLVABLE.lock().unwrap_or_else(|p| p.into_inner());
            held.retain(|f| f != &self.0);
        }
    }

    /// What resolving `asked` answers instead, while a stand-in holds it.
    pub(crate) fn resolving(asked: &Path) -> Option<std::io::Error> {
        let held = UNRESOLVABLE.lock().unwrap_or_else(|p| p.into_inner());
        held.iter().any(|f| asked.starts_with(f)).then(|| {
            #[cfg(windows)]
            let refused = std::io::Error::from_raw_os_error(5);
            #[cfg(not(windows))]
            let refused = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
            refused
        })
    }
}

/// #3682 item 6 — a refusal names what was refused and why, for every
/// reader, as its outermost line. A path the reader may not resolve used to
/// answer a bare label (`validating folder path`, `validating outgoing
/// path`, `canonicalising <path>`) to every reader but the person, whose
/// refusal alone was explained. The daemon reads as its own account, which on
/// a per-user install need not be the person at the screen, so its words say
/// that account.
mod a_refusal_names_what_and_why {
    use super::*;

    /// The OS, as a refusal names it.
    const OS: &str = if cfg!(windows) {
        "Windows"
    } else if cfg!(target_os = "macos") {
        "macOS"
    } else {
        "Linux"
    };

    /// The outermost lines of a folder download of `folder`, a download of
    /// `file` and a listing of `folder`, read as this agent's own account.
    async fn three_reads(folder: &std::path::Path, file: &std::path::Path) -> Vec<String> {
        let (folder, file) = (folder.to_string_lossy(), file.to_string_lossy());
        let zip = FilesHandler::new()
            .begin_outgoing_zip_as(FsAs::Daemon, "z3682-6".into(), &folder)
            .await;
        let get = FilesHandler::new()
            .begin_outgoing_as(FsAs::Daemon, "f3682-6".into(), &file)
            .await;
        let dir = list_dir_as(&FsAs::Daemon, &folder).await;
        vec![outermost(zip), outermost(get), outermost(dir)]
    }

    /// A folder and a file below a folder this agent may not search: on Unix
    /// its mode is 000, and a runner that resolves through it anyway (root)
    /// skips; on Windows a stand-in refuses resolving it.
    #[tokio::test]
    async fn a_path_this_agent_may_not_resolve_is_refused_naming_it_and_why() {
        let temp = tempfile::tempdir().unwrap();
        let shut = temp.path().join("shut");
        let (folder, file) = (shut.join("inner"), shut.join("inner.txt"));
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(&file, b"x").unwrap();
        #[cfg(unix)]
        let held = {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&shut, std::fs::Permissions::from_mode(0o000)).unwrap();
            struct Reopen(std::path::PathBuf);
            impl Drop for Reopen {
                fn drop(&mut self) {
                    use std::os::unix::fs::PermissionsExt;
                    let _ =
                        std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
                }
            }
            Reopen(shut.clone())
        };
        #[cfg(windows)]
        let held = seams::unresolvable(&shut);
        if reads::resolve_path(&folder).is_ok() {
            eprintln!(
                "SKIPPED a_path_this_agent_may_not_resolve_is_refused_naming_it_and_why: this \
                 runner resolves below a folder it may not search (root reads through mode \
                 000), so this test proved nothing here"
            );
            return;
        }
        let read = three_reads(&folder, &file).await;
        drop(held);
        let why = |asked: &std::path::Path| {
            format!(
                "{}: {OS} refused to open it for the account this agent runs as",
                asked.display()
            )
        };
        assert_eq!(read, vec![why(&folder), why(&file), why(&folder)]);
    }

    /// A folder and a file that are not there: each read names the path and
    /// the OS's own reason, where it used to name a step of the agent's.
    #[tokio::test]
    async fn a_path_that_is_not_there_is_refused_naming_it_and_why() {
        let temp = tempfile::tempdir().unwrap();
        let (folder, file) = (temp.path().join("gone"), temp.path().join("gone.txt"));
        let read = three_reads(&folder, &file).await;
        let why = |asked: &std::path::Path| {
            let gone = std::fs::canonicalize(asked).expect_err("not there");
            format!("{}: {gone}", asked.display())
        };
        assert_eq!(read, vec![why(&folder), why(&file), why(&folder)]);
    }
}

/// The Windows person's cells: a low-integrity copy of the runner's own token
/// stands in for the person ([`stand_in::person`]), as in #3669's tests.
#[cfg(target_os = "windows")]
mod as_the_person {
    use super::*;

    /// One drive of the Drives view: `X:\`.
    fn is_a_drive(name: &str) -> bool {
        let b = name.as_bytes();
        b.len() == 3 && b[0].is_ascii_uppercase() && &b[1..] == b":\\"
    }

    /// #3682 item 1 — the Drives view is read inside a step as the reader:
    /// Windows keeps drive letters per sign-in, so a letter the person mapped
    /// is theirs, not the service's. A person whose token has gone is told so,
    /// where the service's own letters used to answer; the person still signed
    /// in lists the drives of their own sign-in, which is the runner's here;
    /// the daemon lists its own.
    #[tokio::test]
    async fn the_drives_view_is_read_as_the_reader_not_the_service() {
        let temp = tempfile::tempdir().unwrap();
        for path in ["", "~"] {
            let gone = stand_in::person_signing_out_after(0, temp.path());
            let Err(why) = list_dir_as(&gone, path).await else {
                panic!("{path:?}: the Drives view answered for a person who has gone");
            };
            assert_eq!(format!("{why}"), NOT_THE_PERSON, "{path:?}");
        }
        let daemon = list_dir_as(&FsAs::Daemon, "")
            .await
            .expect("the daemon's drives");
        assert_eq!(daemon.path, "Drives");
        assert!(
            !daemon.entries.is_empty() && daemon.entries.iter().all(|e| is_a_drive(&e.name)),
            "{:?}",
            names(&daemon)
        );
        let me = stand_in::person(temp.path(), temp.path());
        let theirs = list_dir_as(&me, "~").await.expect("the person's drives");
        assert_eq!(theirs.path, "Drives");
        assert_eq!(
            names(&theirs),
            names(&daemon),
            "one sign-in, one set of letters"
        );
    }
}
