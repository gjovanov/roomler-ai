// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
// FR-69 P1d — the core-only extractors live in `roomler_core::extractors`;
// re-exported so every `crate::extractors::{auth, tenant}` path in this
// crate reads as before.
pub use roomler_core::extractors::{auth, tenant};
