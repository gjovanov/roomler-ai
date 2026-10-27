// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! The daemon binary: one call into the lib.
//!
//! Everything the binary does lives in [`roomlerd::app`] — the CLI, every
//! subcommand, and `app::main`, which builds the runtime and polls the
//! daemon. FR-123 P2 moved it there (`src/main.rs` became `src/app.rs`) so
//! the daemon compiles as ONE unit: rustc emits an `async fn`'s body in the
//! crate that polls it, so a bin that polled `daemon_main` itself would
//! instantiate the daemon's whole async tree a second time, after the lib
//! and on one core. Nothing may poll a lib future from here; this file stays
//! a call. The file is `src/bin/roomlerd.rs`, not `src/main.rs`, so that git
//! pairs the rename and carries an edit in flight on the old file onto
//! `app.rs` when its branch rebases.
fn main() -> anyhow::Result<()> {
    roomlerd::app::main()
}
