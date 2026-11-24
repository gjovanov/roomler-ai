// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! #4402 — the request layer's lines at production's level.
//!
//! `TraceLayer`'s defaults wrote two DEBUG lines for every request the server
//! answered, "started processing request" and "finished processing
//! request": 30 % of the busier pod's log, a pair for every poll of every
//! device and page. The started line now writes at TRACE; the finished line
//! carries the same span (the method and the uri). The finished line keeps
//! tower-http's wording, target and fields, and writes at DEBUG only for an
//! answer worth reading: a refusal or an error (4xx, 5xx), or a slow one.
//! Every other finished line writes at TRACE, so `tower_http=trace` in
//! `RUST_LOG` shows every request again. A 5xx also keeps `on_failure`'s
//! ERROR line, unchanged.

use axum::http::{Response, StatusCode};
use std::time::Duration;
use tower_http::trace::OnResponse;
use tracing::Span;

/// An answer that took at least this long writes its line at DEBUG.
pub(crate) const SLOW_ANSWER: Duration = Duration::from_secs(1);

/// The target tower-http's own finished line has, kept so a read that
/// filters or counts by it reads these lines as before, and so the
/// `tower_http` directive of the filter still decides them.
const TARGET: &str = "tower_http::trace::on_response";

/// Does a request's answer write its finished line at DEBUG (production's
/// level for `tower_http`)? A refusal, an error or a slow answer does; a
/// quick success, a redirect or a protocol switch writes at TRACE.
pub(crate) fn answer_worth_a_line(status: StatusCode, latency: Duration) -> bool {
    status.is_client_error() || status.is_server_error() || latency >= SLOW_ANSWER
}

/// A latency as tower-http's default finished line writes it: "12 ms".
struct Millis(Duration);

impl std::fmt::Display for Millis {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ms", self.0.as_millis())
    }
}

/// The server's [`OnResponse`]: tower-http's "finished processing request"
/// line, at DEBUG only when [`answer_worth_a_line`] says so, else at TRACE.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ResponseLine;

impl<B> OnResponse<B> for ResponseLine {
    fn on_response(self, response: &Response<B>, latency: Duration, _span: &Span) {
        let status = response.status().as_u16();
        let worth_a_line = answer_worth_a_line(response.status(), latency);
        let latency = Millis(latency);
        if worth_a_line {
            tracing::debug!(target: TARGET, %latency, status, "finished processing request");
        } else {
            tracing::trace!(target: TARGET, %latency, status, "finished processing request");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #4402 — a quick success writes at TRACE; a refusal, an error or a
    /// slow answer at DEBUG; the bound itself is slow.
    #[test]
    fn only_a_refusal_an_error_or_a_slow_answer_is_worth_a_line() {
        let quick = Duration::from_millis(3);
        assert!(!answer_worth_a_line(StatusCode::OK, quick));
        assert!(!answer_worth_a_line(StatusCode::NO_CONTENT, quick));
        assert!(!answer_worth_a_line(StatusCode::SWITCHING_PROTOCOLS, quick));
        assert!(!answer_worth_a_line(StatusCode::NOT_MODIFIED, quick));
        assert!(answer_worth_a_line(StatusCode::UNAUTHORIZED, quick));
        assert!(answer_worth_a_line(StatusCode::FORBIDDEN, quick));
        assert!(answer_worth_a_line(StatusCode::NOT_FOUND, quick));
        assert!(answer_worth_a_line(StatusCode::TOO_MANY_REQUESTS, quick));
        assert!(answer_worth_a_line(
            StatusCode::INTERNAL_SERVER_ERROR,
            quick
        ));
        assert!(answer_worth_a_line(StatusCode::SERVICE_UNAVAILABLE, quick));
        assert!(!answer_worth_a_line(
            StatusCode::OK,
            SLOW_ANSWER - Duration::from_millis(1)
        ));
        assert!(answer_worth_a_line(StatusCode::OK, SLOW_ANSWER));
        assert!(answer_worth_a_line(StatusCode::OK, Duration::from_secs(30)));
    }

    /// #4402 — the latency reads as tower-http's own line writes it.
    #[test]
    fn the_latency_reads_as_tower_http_writes_it() {
        assert_eq!(Millis(Duration::from_micros(12_400)).to_string(), "12 ms");
        assert_eq!(Millis(Duration::from_micros(400)).to_string(), "0 ms");
    }
}
