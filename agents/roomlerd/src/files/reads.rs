// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! #3682 — the file drawer's reads, to their edges: what resolving a path
//! answers, as the reader of the step that resolves it.
//!
//! #3669 made a machine-wide Windows install read as the person signed in at
//! the console, one step at a time ([`FsAs::run`]). What a read touches
//! outside such a step, or without a bound, and what its refusal says, is
//! gathered here; the tests are `drawer_edges`.

use std::path::{Path, PathBuf};

/// Resolve `asked` to the path it names, symlinks and `..` followed, as
/// whoever runs the calling step: a download's open, a folder download's
/// begin and walk, a listing.
pub(super) fn resolve_path(asked: &Path) -> std::io::Result<PathBuf> {
    #[cfg(test)]
    if let Some(refused) = super::drawer_edges::seams::resolving(asked) {
        return Err(refused);
    }
    std::fs::canonicalize(asked)
}

/// The entries of the folder `read` lists, `at` being its resolved path:
/// where a test stands in a share whose entries do not come
/// (`drawer_edges::seams::hold_entries`).
pub(super) fn read_entries(at: &Path, read: std::fs::ReadDir) -> Vec<super::DirEntryView> {
    #[cfg(test)]
    super::drawer_edges::seams::entries_wait(at);
    #[cfg(not(test))]
    let _ = at;
    super::collect_dir_entries(read)
}

impl super::FilesHandler {
    /// A listing on this channel, read as `who`.
    #[cfg(test)]
    pub(crate) async fn list_as(
        &self,
        who: &super::FsAs,
        path: &str,
    ) -> anyhow::Result<super::DirListing> {
        super::list_dir_as(who, path).await
    }
}

/// Whether this process's own token is LocalSystem's (`None`: it could not be
/// read), as the Windows reader is decided from it (`fs_as_for`); a test
/// stands it in (`drawer_edges::seams::person_lookup`).
#[cfg(target_os = "windows")]
pub(super) fn own_token_is_local_system() -> Option<bool> {
    #[cfg(test)]
    if let Some(stood) = super::drawer_edges::seams::person_lookup_now() {
        return stood.own_token;
    }
    crate::win_identity::own_token_is_local_system()
}

/// `ROOMLERD_FILES_AS_SEAT_USER`, on unless set to `0`; a test stands it in.
#[cfg(target_os = "windows")]
pub(super) fn files_as_seat_user() -> bool {
    #[cfg(test)]
    if let Some(stood) = super::drawer_edges::seams::person_lookup_now() {
        return stood.switch_on;
    }
    tunnel_core::env::flag("FILES_AS_SEAT_USER", true)
}

/// The person signed in to this process's session; a test stands the lookup
/// in.
#[cfg(target_os = "windows")]
pub(super) fn session_person() -> Option<crate::win_identity::SessionPerson> {
    #[cfg(test)]
    if let Some(stood) = super::drawer_edges::seams::person_lookup_now() {
        return (stood.lookup)();
    }
    crate::win_identity::session_person()
}

/// The machine's staging folder, as the Upload staging folder button lists
/// it; a test stands one in (`drawer_edges::seams::staging_root`).
#[cfg(target_os = "windows")]
pub(super) fn staging_root() -> PathBuf {
    #[cfg(test)]
    if let Some(root) = super::drawer_edges::seams::staging_root_now() {
        return root;
    }
    super::staging_root_windows()
}
