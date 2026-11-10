// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! #3523 — the configured server's clock, as the wizard last read it.
//!
//! The Token step judges a pasted token's `exp`, a time the server issued,
//! against THIS clock and never against the device's alone: a fresh enrollment
//! token lives 10 minutes, so a device whose clock runs that far ahead of the
//! server's refused a token the server would still accept (a cloned Windows VM
//! boots about 3 h ahead).
//!
//! A reading is the `Date` header of an answer the server gives to a request
//! the wizard sends anyway ([`crate::features::ask`]), taken with the monotonic
//! [`Instant`] and the device's wall time at which the answer arrived. The
//! server's time now is that `Date` plus the monotonic time since, so a step of
//! the device's wall clock after the reading does not move it.
//!
//! A reading is used while it is younger than [`MAX_AGE`] by BOTH clocks: the
//! monotonic clock does not count a suspend on Linux or macOS and the wall
//! clock does, so a laptop that slept retires its reading (a forward wall step
//! does too, at the cost of one more request). Carried forward, a reading can
//! only fall behind the server's clock, never run ahead of it: a stale one can
//! let a token the server considers expired reach the enrollment, which refuses
//! it, and never blocks one the server would accept.

use std::collections::BTreeMap;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// How long a reading is used before the server is asked again, by either
/// clock. A fresh enrollment token lives 10 minutes; the Token page reads the
/// clock when it opens, and a person pastes within a few minutes of that.
pub const MAX_AGE: Duration = Duration::from_secs(5 * 60);

/// One reading of a server's clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reading {
    /// The server's time, Unix seconds, from its `Date` header.
    pub server_unix: i64,
    /// This device's wall time when the answer arrived, Unix seconds.
    pub device_unix: i64,
    /// The monotonic time when the answer arrived.
    pub at: Instant,
}

impl Reading {
    /// The server's time at `now` (monotonic) / `device_now` (this device's
    /// wall time), Unix seconds: the reading carried forward on the monotonic
    /// clock. `None` once either clock says the reading is older than
    /// [`MAX_AGE`].
    pub fn server_now(&self, now: Instant, device_now: i64) -> Option<i64> {
        let max_age = MAX_AGE.as_secs() as i64;
        let elapsed = i64::try_from(now.saturating_duration_since(self.at).as_secs()).ok()?;
        let wall_elapsed = device_now.saturating_sub(self.device_unix);
        if elapsed > max_age || wall_elapsed > max_age {
            return None;
        }
        Some(self.server_unix.saturating_add(elapsed))
    }
}

/// This device's wall clock, Unix seconds.
pub fn device_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// A `Date` header's value as Unix seconds, in any of HTTP's three date forms.
/// `None` when it is absent or does not parse: the server's clock is unknown.
pub fn parse_date(value: Option<&str>) -> Option<i64> {
    let at = httpdate::parse_http_date(value?.trim()).ok()?;
    i64::try_from(at.duration_since(UNIX_EPOCH).ok()?.as_secs()).ok()
}

/// The readings this process took, one per server origin. Never written to
/// disk: a new run reads the clock again.
static READINGS: Mutex<BTreeMap<String, Reading>> = Mutex::new(BTreeMap::new());

/// Keep `date`, the `Date` header of an answer from `origin` that arrived at
/// `at` / `device_unix`, as that server's clock. A missing or unparseable
/// `Date` keeps nothing.
pub fn record(origin: &str, date: Option<&str>, at: Instant, device_unix: i64) {
    let Some(server_unix) = parse_date(date) else {
        return;
    };
    READINGS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(
            origin.to_string(),
            Reading {
                server_unix,
                device_unix,
                at,
            },
        );
}

/// `origin`'s clock now, from a reading still young enough to use.
pub fn held(origin: &str) -> Option<i64> {
    let reading = *READINGS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(origin)?;
    reading.server_now(Instant::now(), device_now())
}

/// `server`'s clock now: the reading held for it while that is young enough,
/// else a new one from asking it ([`crate::features::ask`] records it). `None`
/// when the server cannot say: no answer, or no `Date` in it.
pub async fn read(server: &str) -> Option<i64> {
    let origin = crate::features::origin(server);
    if let Some(now) = held(&origin) {
        return Some(now);
    }
    crate::features::ask(server).await;
    held(&origin)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SERVER: i64 = 1_800_000_000;
    const DEVICE: i64 = SERVER + 3 * 3600;

    fn reading() -> Reading {
        Reading {
            server_unix: SERVER,
            device_unix: DEVICE,
            at: Instant::now(),
        }
    }

    #[test]
    fn the_date_header_parses_in_all_three_http_forms() {
        // RFC 9110 §5.6.7's own example, in its three forms.
        for form in [
            "Sun, 06 Nov 1994 08:49:37 GMT",
            "Sunday, 06-Nov-94 08:49:37 GMT",
            "Sun Nov  6 08:49:37 1994",
        ] {
            assert_eq!(parse_date(Some(form)), Some(784_111_777), "{form}");
        }
        assert_eq!(
            parse_date(Some(" Sun, 06 Nov 1994 08:49:37 GMT ")),
            Some(784_111_777)
        );
    }

    #[test]
    fn no_date_or_one_that_does_not_parse_is_an_unknown_clock() {
        assert_eq!(parse_date(None), None);
        assert_eq!(parse_date(Some("")), None);
        assert_eq!(parse_date(Some("yesterday")), None);
        assert_eq!(parse_date(Some("1800000000")), None);
    }

    #[test]
    fn a_reading_is_carried_forward_on_the_monotonic_clock() {
        let r = reading();
        assert_eq!(r.server_now(r.at, DEVICE), Some(SERVER));
        assert_eq!(
            r.server_now(r.at + Duration::from_secs(90), DEVICE + 90),
            Some(SERVER + 90)
        );
    }

    /// The wall clock stepped back after the reading (the time service caught
    /// up): the server's time does not move with it.
    #[test]
    fn a_wall_clock_step_back_does_not_move_the_server_time() {
        let r = reading();
        let later = r.at + Duration::from_secs(30);
        assert_eq!(
            r.server_now(later, DEVICE + 30 - 3 * 3600),
            Some(SERVER + 30)
        );
    }

    /// The wall clock ran far ahead of the monotonic one: a suspend that the
    /// monotonic clock did not count (Linux, macOS), or a forward step. The
    /// reading is retired and the server is asked again.
    #[test]
    fn a_suspend_or_a_forward_step_retires_the_reading() {
        let r = reading();
        let later = r.at + Duration::from_secs(30);
        assert_eq!(r.server_now(later, DEVICE + 8 * 3600), None);
    }

    #[test]
    fn a_reading_older_than_the_limit_is_not_used() {
        let r = reading();
        let limit = MAX_AGE.as_secs() as i64;
        let at_limit = r.at + MAX_AGE;
        assert_eq!(r.server_now(at_limit, DEVICE + limit), Some(SERVER + limit));
        let past = r.at + MAX_AGE + Duration::from_secs(1);
        assert_eq!(r.server_now(past, DEVICE + limit + 1), None);
    }

    #[test]
    fn a_date_that_does_not_parse_records_nothing() {
        let origin = "https://no-date.invalid";
        record(origin, Some("not a date"), Instant::now(), DEVICE);
        assert_eq!(held(origin), None);
        record(origin, None, Instant::now(), DEVICE);
        assert_eq!(held(origin), None);
    }

    #[test]
    fn a_recorded_reading_answers_for_its_own_origin_only() {
        let origin = "https://recorded.invalid";
        let date = httpdate::fmt_http_date(UNIX_EPOCH + Duration::from_secs(SERVER as u64));
        record(origin, Some(&date), Instant::now(), device_now());
        let now = held(origin).expect("a fresh reading");
        assert!((SERVER..SERVER + 5).contains(&now), "{now}");
        assert_eq!(held("https://other.invalid"), None);
    }

    /// Only the configured server's own answer to the route, from the origin
    /// that was asked, is a reading of its clock ([`crate::features::ask`]).
    #[test]
    fn only_the_servers_own_answer_from_the_asked_origin_is_a_reading() {
        use crate::features::tells_the_servers_clock as tells;
        let asked = "https://roomler.example/api/setup/features";
        let url = |s: &str| reqwest::Url::parse(s).unwrap();
        let same = url(asked);
        assert!(tells(asked, &same, 200, b"{}"));
        assert!(tells(asked, &same, 200, br#"{"pairing": true}"#));
        let within = url("https://roomler.example/api/setup/features/");
        assert!(
            tells(asked, &within, 200, b"{}"),
            "a redirect within the origin"
        );
        for elsewhere in [
            "https://portal.example/login",
            "http://roomler.example/api/setup/features",
            "https://roomler.example:8443/api/setup/features",
        ] {
            assert!(!tells(asked, &url(elsewhere), 200, b"{}"), "{elsewhere}");
        }
        let not_its_answer: [(u16, &[u8]); 7] = [
            (404, b"404 page not found"),
            (404, b"{}"),
            (400, b"{}"),
            (200, b"<html>Sign in to this network</html>"),
            (200, b"[]"),
            (200, b""),
            (502, b"{}"),
        ];
        for (status, body) in not_its_answer {
            assert!(!tells(asked, &same, status, body), "{status}");
        }
    }
}
