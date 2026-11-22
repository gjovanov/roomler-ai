// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-148 — the daemon's half of one declared list per `enum:` config key.
//!
//! `roomler config set` takes the values a key's registry entry declares
//! (`kind: "enum:a|b|c"`, `config_surface::declared_values`) and no others.
//! Each test here sends every declared value through that setter, then
//! through the parser in this crate that reads the key, fed the way a device
//! feeds it, and holds both directions (`config_surface::pin_declared_values`):
//! every declared value reads as a mode of its own, only one of them as the
//! default, and every mode the parser returns is read from a declared value.
//! A value a parser learns stays unsettable no longer than a test run, which
//! `route` did through 0.4.231, and a declared value the daemon reads as the
//! default fails here as well.
//!
//! Each mode list goes through a match without a wildcard, so a new variant
//! fails to compile here until it is listed. `qsv_rate_mode` is pinned in the
//! FFmpeg encoder's tests, beside its parser (`ffmpeg-encoder` builds only);
//! `encoder_preference` and `ice_follow_renomination` in `roomler-node-core`,
//! whose setter parses them into typed fields.

use std::collections::BTreeSet;

use crate::config::{AgentConfig, env_fallbacks, test_fixture};
use crate::config_surface::{self, pin_declared_values};
use crate::extauth::ExternalConsent;
use crate::power::PowerPolicy;
use crate::rc_overlay::RankMode;
use crate::{media_delivery, media_inflight};

/// A key's config fallback, as `tunnel_core::env::node_env` hands it to the
/// parser when no environment variable is set.
fn fallback(cfg: &AgentConfig, suffix: &str) -> Option<String> {
    env_fallbacks(cfg).remove(suffix)
}

/// `RankMode::read`, once per session, through `RC_CARRIER_RANK`.
#[test]
fn rc_carrier_rank_declares_every_rank_mode() {
    let modes = [
        RankMode::Off,
        RankMode::Prio,
        RankMode::Cost,
        RankMode::Route,
    ]
    .map(|m| match m {
        RankMode::Off | RankMode::Prio | RankMode::Cost | RankMode::Route => m,
    });
    pin_declared_values(
        "rc_carrier_rank",
        |cfg| RankMode::parse(fallback(cfg, "RC_CARRIER_RANK").as_deref()),
        &modes,
    );
}

/// `media_delivery::mode`, once per pump, through `PATH_MODEL`.
#[test]
fn path_model_declares_every_mode_the_pump_reads() {
    use media_delivery::Mode;
    let modes = [Mode::Off, Mode::Shadow, Mode::On].map(|m| match m {
        Mode::Off | Mode::Shadow | Mode::On => m,
    });
    pin_declared_values(
        "path_model",
        |cfg| media_delivery::mode_of(fallback(cfg, "PATH_MODEL").as_deref()),
        &modes,
    );
}

/// `media_inflight::mode`, once per pump, through `INFLIGHT_BUDGET`.
#[test]
fn inflight_budget_declares_every_mode_the_pump_reads() {
    use media_inflight::Mode;
    let modes = [Mode::Off, Mode::Shadow, Mode::On].map(|m| match m {
        Mode::Off | Mode::Shadow | Mode::On => m,
    });
    pin_declared_values(
        "inflight_budget",
        |cfg| media_inflight::mode_of(fallback(cfg, "INFLIGHT_BUDGET").as_deref()),
        &modes,
    );
}

/// The power keeper's policy, read from the config at start (`app.rs`).
#[test]
fn power_policy_declares_every_policy_the_keeper_reads() {
    let modes = [PowerPolicy::Never, PowerPolicy::OnAc, PowerPolicy::Always].map(|p| match p {
        PowerPolicy::Never | PowerPolicy::OnAc | PowerPolicy::Always => p,
    });
    pin_declared_values(
        "power_policy",
        |cfg| PowerPolicy::parse(&cfg.power_policy),
        &modes,
    );
}

/// An external session's consent, read from the config file at each request
/// (`extauth::imp::admit_session`). A refusal is not a mode: a declared value
/// the device refuses fails as one that reads as nothing listed.
#[test]
fn external_consent_mode_declares_every_consent_the_device_reads() {
    let modes = [ExternalConsent::Prompt, ExternalConsent::Auto].map(|c| match c {
        ExternalConsent::Prompt | ExternalConsent::Auto => Ok::<_, &'static str>(c),
    });
    pin_declared_values(
        "external_consent_mode",
        |cfg| ExternalConsent::parse(cfg.external_consent_mode.as_deref()),
        &modes,
    );
}

/// `ExternalConsent::parse` reads what `admit_session` read before it was
/// taken out of that function: trimmed, case kept, and anything but `prompt`,
/// `auto` or nothing is refused rather than read as either.
#[test]
fn external_consent_reads_as_admit_session_read_it() {
    for asks in [None, Some(""), Some("  "), Some("prompt"), Some(" prompt ")] {
        assert_eq!(
            ExternalConsent::parse(asks),
            Ok(ExternalConsent::Prompt),
            "{asks:?}"
        );
    }
    for skips in ["auto", " auto "] {
        assert_eq!(
            ExternalConsent::parse(Some(skips)),
            Ok(ExternalConsent::Auto),
            "{skips:?}"
        );
    }
    for refused in ["Auto", "PROMPT", "ask", "yes", "auto,prompt"] {
        assert_eq!(
            ExternalConsent::parse(Some(refused)),
            Err("external_consent_mode is neither prompt nor auto"),
            "{refused:?}"
        );
    }
}

/// Every `enum:` key of the surface is pinned against the parser that reads
/// it: here, in the FFmpeg encoder's tests, or in `roomler-node-core`. A new
/// one fails here until its pin is written.
#[test]
fn every_enum_key_is_pinned() {
    let enum_keys: BTreeSet<String> = config_surface::entries(&test_fixture())
        .into_iter()
        .filter(|e| e.kind.starts_with("enum:"))
        .map(|e| e.key)
        .collect();
    let pinned: BTreeSet<String> = [
        // Here.
        "rc_carrier_rank",
        "path_model",
        "inflight_budget",
        "power_policy",
        "external_consent_mode",
        // `encode::ffmpeg::encoder`'s tests, beside `QsvRateMode::parse`.
        "qsv_rate_mode",
        // `roomler_node_core::config_surface`'s tests: typed fields.
        "encoder_preference",
        "ice_follow_renomination",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    assert_eq!(enum_keys, pinned);
}
