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
//! P1 ships the compiled-module list only. Per-tenant capabilities (plan
//! limits, tenant settings, the module's own flags) arrive with each module
//! (`roomler_core::Module::capabilities`) and are mirrored into
//! `/api/auth/me`, so a signed-in client pays no extra request for them.
//!
//! ⚠️ `switched_off` is a config fact, not a routing fact, until the named
//! module has been extracted: a switch turns off nothing before its module's
//! own PR lands (see `docs/fr/FR-69-modular-monolith.md`, D4). The field is
//! named for what it is so no client mistakes it for "absent".

use axum::{Json, extract::State};
use serde_json::{Value, json};

use crate::core_state::Core;

pub async fn get(State(core): State<Core>) -> Json<Value> {
    Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "modules": roomler_core::graph::MODULES,
        "switched_off": core.settings.modules.switched_off(),
    }))
}
