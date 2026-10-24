// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! Helpers for the in-memory grid feeds (the members list, the devices
//! listing), whose rows are composed from several collections and sorted in
//! Rust rather than by the database.

use std::cmp::Ordering;

/// Order two RFC3339 timestamps by the instant they name.
///
/// ⚠️ Never compare the strings. `bson`'s `try_to_rfc3339_string` formats
/// through `time`, which DROPS a zero fraction and trims trailing zeros, so
/// within one second the string order is not the time order: `…:10Z`
/// (10.000 s) sorts after `…:10.25Z`, and `…:10.123Z` before `…:10.12Z`.
/// That flipped the members grid's default order (`joined_at` ascending, the
/// creator first) whenever the creator joined on a whole second.
///
/// A value that does not parse (a row's empty fallback) sorts first, as the
/// empty string did.
pub(crate) fn cmp_instants(a: &str, b: &str) -> Ordering {
    let at = |s: &str| chrono::DateTime::parse_from_rfc3339(s).ok();
    at(a).cmp(&at(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_whole_second_sorts_before_the_rest_of_that_second() {
        // The members grid's flake: the creator joined at 10.000 s, the
        // member 250 ms later.
        let creator = "2026-10-23T23:46:10Z";
        let member = "2026-10-23T23:46:10.25Z";
        assert!(creator > member, "the trap: the strings sort the other way");
        assert_eq!(cmp_instants(creator, member), Ordering::Less);
        assert_eq!(cmp_instants(member, creator), Ordering::Greater);
    }

    #[test]
    fn trimmed_fractions_sort_by_their_value() {
        let a = "2026-10-23T23:46:10.12Z"; // 120 ms
        let b = "2026-10-23T23:46:10.123Z"; // 123 ms
        assert!(a > b, "the trap: the strings sort the other way");
        assert_eq!(cmp_instants(a, b), Ordering::Less);
    }

    #[test]
    fn the_same_instant_is_equal_whatever_its_spelling() {
        assert_eq!(
            cmp_instants("2026-10-23T23:46:10.5Z", "2026-10-24T01:46:10.500+02:00"),
            Ordering::Equal
        );
    }

    #[test]
    fn an_unparseable_value_sorts_first() {
        assert_eq!(cmp_instants("", "2026-10-23T23:46:10Z"), Ordering::Less);
        assert_eq!(cmp_instants("2026-10-23T23:46:10Z", ""), Ordering::Greater);
        assert_eq!(cmp_instants("", ""), Ordering::Equal);
    }

    #[test]
    fn the_rows_own_formatter_sorts_by_time_across_a_second() {
        // Every millisecond of one second against a whole second and the
        // next, formatted exactly as the grids format their rows.
        let fmt = |ms: i64| {
            bson::DateTime::from_millis(1_761_263_170_000 + ms)
                .try_to_rfc3339_string()
                .unwrap()
        };
        for ms in 0..1000 {
            for later in [ms + 1, ms + 10, ms + 100, 1000] {
                if later <= ms {
                    continue;
                }
                assert_eq!(
                    cmp_instants(&fmt(ms), &fmt(later)),
                    Ordering::Less,
                    "{} must sort before {}",
                    fmt(ms),
                    fmt(later)
                );
            }
        }
    }
}
