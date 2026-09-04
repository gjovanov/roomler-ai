// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! `GET /api/capabilities` — what this server is composed of (FR-69 D10).
//!
//! Unauthenticated by design and deliberately small: the module list, the
//! version, and which `[modules]` switches an operator has turned off. It is
//! how one UI build and one daemon work against any server profile — the SPA
//! hides navigation from it, the daemon reads it before offering a pillar the
//! server does not have.
//!
//! `modules` is what THIS server mounts (P8: the profile it was built as,
//! minus any `[modules]` switch); `compiled` is what the build linked. Per-
//! tenant capabilities (plan limits, tenant settings, the module's own flags)
//! arrive with each module (`roomler_core::Module::capabilities`) and are
//! mirrored into `/api/auth/me`, so a signed-in client pays no extra request
//! for them.
//!
//! ⚠️ `switched_off` is a config fact: since P7b every switch is real (each
//! module is extracted), so `modules` = `compiled` − `switched_off`. The field
//! is named for what it is so no client mistakes it for "absent from the
//! build" — a client that must tell the two apart reads `compiled`.

use axum::{Json, extract::State};
use serde_json::{Value, json};

use crate::{compose, state::AppState};

pub async fn get(State(state): State<AppState>) -> Json<Value> {
    Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "modules": state.modules.mounted(),
        "compiled": compose::EXTRACTED,
        "switched_off": state.settings.modules.switched_off(),
    }))
}
