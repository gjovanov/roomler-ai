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
