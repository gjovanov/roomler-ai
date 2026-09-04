// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
pub mod agent;

// FR-69 P1d — the core-only extractors live in `roomler_core::extractors`;
// re-exported so every `crate::extractors::{auth, tenant}` path in this crate
// reads as before. `agent` stays: it loads the agent row (fleet's).
pub use roomler_core::extractors::{auth, tenant};
