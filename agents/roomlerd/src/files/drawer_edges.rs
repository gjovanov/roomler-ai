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
