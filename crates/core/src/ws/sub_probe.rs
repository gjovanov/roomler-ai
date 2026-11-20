// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! #3952 — every Redis subscriber of the server learns that its own
//! connection went deaf, and subscribes again on a fresh one.
//!
//! The setting: `ROOMLER__REDIS__SUBSCRIBER_PROBE_MS`, how often a subscriber
//! publishes a probe to itself. 0 turns the probe off, and a value from 1 to
//! 999 is used as 1000, with a warn line at boot.

use std::time::Duration;

use tracing::warn;

use super::redis_conn::{BOUND_FLOOR_MS, bound_ms, bound_ms_used};

/// The default of `ROOMLER__REDIS__SUBSCRIBER_PROBE_MS`.
pub const DEFAULT_PROBE_INTERVAL: Duration = Duration::from_millis(15_000);

/// The setting by the name a deployment sets it with.
const PROBE_SETTING: &str = "ROOMLER__REDIS__SUBSCRIBER_PROBE_MS";

/// How a subscriber probes its own connection. Built once at boot and handed
/// to every subscriber.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProbeSettings {
    /// A probe every this long; `None` is no probe.
    pub interval: Option<Duration>,
}

impl Default for ProbeSettings {
    fn default() -> Self {
        Self {
            interval: Some(DEFAULT_PROBE_INTERVAL),
        }
    }
}

impl ProbeSettings {
    /// No probe: a subscriber hears what its connection delivers, as before
    /// #3952.
    pub const OFF: Self = Self { interval: None };

    /// From the settings, in milliseconds: 0 is off, and a value under
    /// [`BOUND_FLOOR_MS`] is used as it, with a warn line.
    pub fn from_settings(settings: &roomler_ai_config::RedisSettings) -> Self {
        let given = settings.subscriber_probe_ms;
        let used = bound_ms_used(given);
        if let Some(used) = used.filter(|&used| used != given) {
            warn!(
                setting = PROBE_SETTING,
                given_ms = given,
                used_ms = used,
                "redis: a subscriber probe interval under {BOUND_FLOOR_MS} ms is used as {BOUND_FLOOR_MS} ms"
            );
        }
        Self {
            interval: used.map(Duration::from_millis),
        }
    }

    /// The interval as the settings write it: 0 is off.
    pub fn interval_ms(&self) -> u64 {
        bound_ms(self.interval)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(subscriber_probe_ms: u64) -> roomler_ai_config::RedisSettings {
        roomler_ai_config::RedisSettings {
            url: "redis://r:6379".to_string(),
            response_timeout_ms: 3000,
            connection_timeout_ms: 3000,
            subscriber_probe_ms,
        }
    }

    /// 0 is off; a value under 1 s is used as 1 s; 1 s and over as given; the
    /// default is 15 s.
    #[test]
    fn the_probe_interval_is_off_at_0_and_never_under_1_s() {
        let used: Vec<_> = [0, 1, 500, 999, 1000, 15_000]
            .into_iter()
            .map(|given| {
                (
                    given,
                    ProbeSettings::from_settings(&settings(given)).interval_ms(),
                )
            })
            .collect();
        assert_eq!(
            used,
            [
                (0, 0),
                (1, 1000),
                (500, 1000),
                (999, 1000),
                (1000, 1000),
                (15_000, 15_000)
            ]
        );
        assert_eq!(
            ProbeSettings::from_settings(&settings(0)),
            ProbeSettings::OFF
        );
        assert_eq!(
            ProbeSettings::from_settings(&settings(15_000)),
            ProbeSettings::default()
        );
    }
}
