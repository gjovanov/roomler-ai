// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! Reading cookies off a request, in one place.
//!
//! There were three hand-rolled parsers before this — in the auth extractor,
//! in the OAuth callback, and in the `/ws` upgrade — and a fourth was about to
//! be written for the refresh cookie. They agreed, but only by coincidence:
//! each was a slightly different spelling of split-on-`;`, trim, match a
//! prefix, and the interesting case is one none of them stated. `strip_prefix`
//! on an untrimmed segment matches `other_access_token=…` as readily as
//! `access_token=…`, so the difference between "correct" and "authenticates
//! the wrong value" is whether the author remembered to split on `=` or to
//! anchor the name. That is not a thing to re-decide per call site.

use axum::http::{HeaderMap, header};

/// The value of cookie `name`, or `None` if it is absent or empty.
///
/// Matches the cookie NAME exactly — a cookie called `x_access_token` does not
/// satisfy a request for `access_token`.
pub fn get(headers: &HeaderMap, name: &str) -> Option<String> {
    let raw = headers.get(header::COOKIE)?.to_str().ok()?;
    raw.split(';')
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(k, _)| *k == name)
        // Trim the VALUE too: a stray space would ride into JWT verification
        // and fail it, which reads as "your session is invalid" rather than
        // "a cookie header was formatted oddly".
        .map(|(_, v)| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderValue, header::COOKIE};

    fn h(raw: &str) -> HeaderMap {
        let mut m = HeaderMap::new();
        m.insert(COOKIE, HeaderValue::from_str(raw).unwrap());
        m
    }

    #[test]
    fn finds_a_cookie_among_others() {
        assert_eq!(
            get(
                &h("theme=dark; access_token=abc.def; lang=en"),
                "access_token"
            )
            .as_deref(),
            Some("abc.def")
        );
        assert_eq!(
            get(&h("access_token=only"), "access_token").as_deref(),
            Some("only")
        );
    }

    #[test]
    fn the_name_must_match_exactly() {
        // The case every hand-rolled `strip_prefix` version got right only by
        // accident: a DIFFERENT cookie whose name ends with the one we want.
        assert_eq!(get(&h("other_access_token=nope"), "access_token"), None);
        assert_eq!(get(&h("xaccess_token=nope"), "access_token"), None);
        // ...and one that merely starts with it.
        assert_eq!(get(&h("access_token_v2=nope"), "access_token"), None);
    }

    #[test]
    fn absent_and_empty_are_both_none() {
        assert_eq!(get(&HeaderMap::new(), "access_token"), None);
        assert_eq!(get(&h("theme=dark"), "access_token"), None);
        assert_eq!(get(&h("access_token="), "access_token"), None);
    }

    #[test]
    fn a_value_containing_equals_survives_intact() {
        // JWTs are base64url and carry no `=`, but a padded base64 cookie
        // would — splitting on every `=` instead of the first would corrupt it.
        assert_eq!(
            get(&h("t=YWJj==; access_token=a.b.c"), "t").as_deref(),
            Some("YWJj==")
        );
    }

    #[test]
    fn whitespace_around_pairs_and_values_is_ignored() {
        assert_eq!(
            get(&h("  theme=dark ;   access_token=tok  "), "access_token").as_deref(),
            Some("tok")
        );
        // Whitespace-only is empty, not a credential.
        assert_eq!(get(&h("access_token=   "), "access_token"), None);
    }
}
