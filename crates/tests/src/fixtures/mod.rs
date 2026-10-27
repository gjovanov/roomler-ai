// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/// FR-127 — the user socket's `media:*` round trips against a real worker,
/// shared by the view (P1) and simulcast (P2) tests. ⚠️ Test-only: the
/// socket crates it drives are dev-dependencies, and `fixtures` is compiled
/// into the lib target too.
#[cfg(test)]
pub mod media_ws;
pub mod seed;
pub mod test_app;
