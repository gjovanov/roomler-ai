// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! Request extractors that need only the [`crate::Core`] — the ones a module
//! crate's handlers use.
//!
//! FR-69 P1d — moved from the api crate unchanged. `AuthUser` and
//! `OptionalAuthUser` are bound on `Core: FromRef<S>`, so they work for any
//! router state that can hand out a `Core` (the host's `AppState`, and every
//! module's own state). The agent extractor stayed in the api crate: it loads
//! the agent row, which is `fleet`'s.

pub mod auth;
pub mod tenant;
