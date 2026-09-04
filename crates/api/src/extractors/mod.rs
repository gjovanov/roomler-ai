// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
// FR-69 P1d — the core-only extractors live in `roomler_core::extractors`;
// P5a — `agent` (it loads the agent row) is the fleet module's. Both are
// re-exported so every `crate::extractors::{agent, auth, tenant}` path in this
// crate reads as before.
pub use roomler_ai_mod_fleet::auth_agent as agent;
pub use roomler_core::extractors::{auth, tenant};
