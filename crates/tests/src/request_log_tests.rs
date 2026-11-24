// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! #4402 — a request writes a line at production's level only when its
//! answer is worth reading: a refusal, an error, or a slow answer.
//!
//! `tower_http::trace` wrote two DEBUG lines for every request the server
//! answered ("started processing request" and "finished processing
//! request"), 30 % of the busier pod's log. Production keeps `tower_http` at
//! DEBUG (`crates/api/src/main.rs`), so this test reads at that level: a
//! quick answer writes nothing there, and a refusal writes the one line it
//! always wrote, with the same wording, target and fields.

use crate::fixtures::test_app::TestApp;
use crate::session_media_plan_tests::Logged;

const STARTED: &str = "started processing request";
const FINISHED: &str = "finished processing request";
/// The finished line's target, which a read of the log filters by.
const TARGET: &str = "tower_http::trace::on_response";
/// Production's level for the request lines (`crates/api/src/main.rs`).
const PRODUCTION: &str = "warn,tower_http=debug";

/// The captured lines that hold `what`.
fn lines_with(logged: &Logged, what: &str) -> Vec<String> {
    logged
        .text()
        .lines()
        .filter(|l| l.contains(what))
        .map(str::to_string)
        .collect()
}

/// #4402 — a quick 200 writes no request line at production's level, and a
/// 401 writes its "finished processing request" line, as it always did.
#[tokio::test]
async fn a_quick_answer_writes_no_line_and_a_refusal_writes_one() {
    let app = TestApp::spawn().await;
    let logged = Logged::default();
    let _log = logged.install_filter(PRODUCTION);

    let ok = app.client.get(app.url("/health")).send().await.unwrap();
    assert_eq!(ok.status().as_u16(), 200);
    let quick = (
        lines_with(&logged, STARTED).len(),
        lines_with(&logged, FINISHED).len(),
    );

    // No token: the route answers 401 at once.
    let refused = app
        .client
        .get(app.url("/api/auth/me"))
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status().as_u16(), 401);
    let started = lines_with(&logged, STARTED);
    let finished = lines_with(&logged, FINISHED);

    eprintln!(
        "W56-RESULT requests: quick_200 started={} finished={}; both started={} finished={}",
        quick.0,
        quick.1,
        started.len(),
        finished.len()
    );
    assert_eq!(
        quick,
        (0, 0),
        "a quick 200 writes no request line at production's level"
    );
    assert!(
        started.is_empty(),
        "no request writes its started line at production's level: {started:?}"
    );
    assert_eq!(
        finished.len(),
        1,
        "the refusal writes one line: {finished:?}"
    );
    for field in [TARGET, "status=401", "latency="] {
        assert!(
            finished[0].contains(field),
            "the refusal's line lacks {field}: {}",
            finished[0]
        );
    }
}
