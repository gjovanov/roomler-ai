// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-102 P7 — the search pullers against fakes of Google, Bing and the
//! blog's feed (spec §3k).
//!
//! * **Search Console** (`saas::search_pull::run_once`, an injected clock):
//!   the service account's assertion and the token exchange (the fake
//!   checks the claims), the token cached across a run, the property as one
//!   encoded path segment, only FINAL days pulled — a day from
//!   `firstIncompleteDate` on stays due and stores nothing — the four
//!   breakdowns, the impression-weighted position, and a refused token or a
//!   missing permission recorded per day, never a 0.
//! * **The feed and URL Inspection** (`saas::content_pull::run_once`): the
//!   blog's Atom feed into `growth_content`, a post that leaves it kept as
//!   `in_feed: false`, each post's verdict and last crawl, a 403 recorded
//!   with the last good verdict kept, and a missing feed read as no blog.
//! * **Bing** (`saas::bing_pull::run_once`): the daily totals and the weekly
//!   buckets at Bing's own grain, Bing's `/Date(…)/` read with its offset,
//!   and a refused key recorded.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use axum::{
    Form, Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use bson::{Document, doc};
use chrono::{Duration, NaiveDate};
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Validation};
use roomler_ai_config::GrowthSettings;
use roomler_ai_mod_saas::{
    bing_pull::{self, BingClient},
    content_pull,
    search_console::{SCOPE, ScClient},
    search_pull,
};
use roomler_ai_services::dao::{
    growth::{BING_SOURCE, BING_WEEKLY_SOURCE, GROWTH_DAILY, GROWTH_PULLS, SEARCH_CONSOLE_SOURCE},
    growth_content::GROWTH_CONTENT,
};
use serde_json::{Value, json};

use crate::{
    fixtures::test_app::TestApp,
    growth_campaigns_tests::{at_day, count, ds, today},
};

const SECRET: &[u8] = b"fake-google-signing-secret";
const SA: &str = "growth-reader@roomler-test.iam.gserviceaccount.com";
const SITE: &str = "sc-domain:roomler.ai";

// ── A fake Google ───────────────────────────────────────────────────────

/// Day → the answers of that day: `(the breakdown's dimensions, a row)`.
type DayData = HashMap<String, Vec<(Vec<&'static str>, Value)>>;

#[derive(Clone, Default)]
struct Google {
    /// The token endpoint's own URL — the audience a correct assertion names.
    token_uri: Arc<Mutex<String>>,
    tokens_issued: Arc<Mutex<u32>>,
    /// Search Analytics requests: `(decoded site, body)`.
    queries: Arc<Mutex<Vec<(String, Value)>>>,
    inspected: Arc<Mutex<Vec<String>>>,
    /// The first day Google calls not final.
    first_incomplete: Arc<Mutex<Option<String>>>,
    /// Day → `(dimensions, rows)` answers; a day absent has no data.
    data: Arc<Mutex<DayData>>,
    /// Refuse every API call 403, as Google refuses a non-user.
    forbid: Arc<Mutex<bool>>,
    /// Refuse URL Inspection only, as a Restricted user may be refused.
    forbid_inspection: Arc<Mutex<bool>>,
}

async fn g_token(
    State(g): State<Google>,
    Form(form): Form<HashMap<String, String>>,
) -> (StatusCode, Json<Value>) {
    let bad = |e: &str| (StatusCode::BAD_REQUEST, Json(json!({ "error": e })));
    if form.get("grant_type").map(String::as_str)
        != Some("urn:ietf:params:oauth:grant-type:jwt-bearer")
    {
        return bad("unsupported_grant_type");
    }
    let Some(assertion) = form.get("assertion") else {
        return bad("invalid_request");
    };
    let aud = g.token_uri.lock().unwrap().clone();
    let mut v = Validation::new(Algorithm::HS256);
    v.set_audience(&[aud]);
    let Ok(data) = jsonwebtoken::decode::<Value>(assertion, &DecodingKey::from_secret(SECRET), &v)
    else {
        return bad("invalid_grant");
    };
    let c = data.claims;
    let lifetime = c["exp"].as_i64().unwrap_or(0) - c["iat"].as_i64().unwrap_or(0);
    if c["iss"] != SA || c["scope"] != SCOPE || !(1..=3600).contains(&lifetime) {
        return bad("invalid_grant");
    }
    *g.tokens_issued.lock().unwrap() += 1;
    (
        StatusCode::OK,
        Json(json!({ "access_token": "ya29.fake", "expires_in": 3599, "token_type": "Bearer" })),
    )
}

fn authorised(headers: &HeaderMap) -> bool {
    headers.get("authorization").and_then(|v| v.to_str().ok()) == Some("Bearer ya29.fake")
}

fn forbidden() -> (StatusCode, Json<Value>) {
    (
        StatusCode::FORBIDDEN,
        Json(json!({ "error": { "code": 403,
            "message": "User does not have sufficient permission for site 'sc-domain:roomler.ai'.",
            "status": "PERMISSION_DENIED" } })),
    )
}

async fn g_query(
    State(g): State<Google>,
    Path(site): Path<String>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    if !authorised(&headers) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": { "code": 401 } })),
        );
    }
    g.queries.lock().unwrap().push((site, body.clone()));
    if *g.forbid.lock().unwrap() {
        return forbidden();
    }
    let dims: Vec<String> = body["dimensions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d.as_str().unwrap().to_string())
        .collect();
    let (from, to) = (
        body["startDate"].as_str().unwrap(),
        body["endDate"].as_str().unwrap(),
    );
    let data = g.data.lock().unwrap().clone();
    let mut rows = Vec::new();
    if dims == ["date"] {
        // Property totals per date; `all` includes the unsettled days.
        let mut days: Vec<&String> = data
            .keys()
            .filter(|d| d.as_str() >= from && d.as_str() <= to)
            .collect();
        days.sort();
        for d in days {
            for (dset, r) in &data[d] {
                if *dset == ["date"] {
                    let mut r = r.clone();
                    r["keys"] = json!([d]);
                    rows.push(r);
                }
            }
        }
        let mut out = json!({ "rows": rows, "responseAggregationType": "byProperty" });
        if let Some(f) = g.first_incomplete.lock().unwrap().clone() {
            out["metadata"] = json!({ "firstIncompleteDate": f });
        }
        return (StatusCode::OK, Json(out));
    }
    assert_eq!(from, to, "a breakdown is asked one day at a time");
    assert_eq!(body["dataState"], "final");
    for (dset, r) in data.get(from).cloned().unwrap_or_default() {
        if dset == dims.iter().map(String::as_str).collect::<Vec<_>>() {
            rows.push(r);
        }
    }
    if rows.is_empty() {
        // Google leaves `rows` out of an empty answer.
        return (
            StatusCode::OK,
            Json(json!({ "responseAggregationType": "byPage" })),
        );
    }
    (
        StatusCode::OK,
        Json(json!({ "rows": rows, "responseAggregationType": "byPage" })),
    )
}

async fn g_inspect(
    State(g): State<Google>,
    Path(action): Path<String>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    assert_eq!(action, "index:inspect");
    if !authorised(&headers) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": { "code": 401 } })),
        );
    }
    if *g.forbid_inspection.lock().unwrap() || *g.forbid.lock().unwrap() {
        return forbidden();
    }
    assert_eq!(body["siteUrl"], SITE);
    let url = body["inspectionUrl"].as_str().unwrap().to_string();
    g.inspected.lock().unwrap().push(url.clone());
    let status = if url.ends_with("/blog/a/") {
        json!({ "verdict": "PASS", "coverageState": "Submitted and indexed",
            "indexingState": "INDEXING_ALLOWED", "pageFetchState": "SUCCESSFUL",
            "robotsTxtState": "ALLOWED", "lastCrawlTime": "2026-10-12T08:00:00Z",
            "googleCanonical": url })
    } else {
        json!({ "verdict": "NEUTRAL", "coverageState": "Crawled - currently not indexed",
            "lastCrawlTime": "2026-10-10T08:00:00Z" })
    };
    (
        StatusCode::OK,
        Json(
            json!({ "inspectionResult": { "inspectionResultLink": "https://search.google.com/x",
            "indexStatusResult": status } }),
        ),
    )
}

/// The blog's feed: `None` answers 404 (a site with no post).
#[derive(Clone, Default)]
struct Feed(Arc<Mutex<Option<String>>>);

async fn feed(State(f): State<Feed>) -> (StatusCode, String) {
    match f.0.lock().unwrap().clone() {
        Some(xml) => (StatusCode::OK, xml),
        None => (StatusCode::NOT_FOUND, String::new()),
    }
}

async fn fake_google() -> (String, Google, Feed) {
    let g = Google::default();
    let f = Feed::default();
    let router = Router::new()
        .route("/token", post(g_token))
        .route(
            "/webmasters/v3/sites/{site}/searchAnalytics/query",
            post(g_query),
        )
        .route("/v1/urlInspection/{action}", post(g_inspect))
        .with_state(g.clone())
        .merge(
            Router::new()
                .route("/blog/feed.xml", get(feed))
                .with_state(f.clone()),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let base = format!("http://{addr}");
    *g.token_uri.lock().unwrap() = format!("{base}/token");
    (base, g, f)
}

fn sc(base: &str, secret: &[u8]) -> ScClient {
    ScClient::with_signer(
        SITE,
        base,
        &format!("{base}/token"),
        SA,
        Some("kid-1".into()),
        EncodingKey::from_secret(secret),
        Algorithm::HS256,
    )
}

fn row(keys: &[&str], clicks: f64, impressions: f64, position: f64) -> Value {
    json!({ "keys": keys, "clicks": clicks, "impressions": impressions,
        "ctr": if impressions > 0.0 { clicks / impressions } else { 0.0 }, "position": position })
}

async fn daily(
    app: &TestApp,
    source: &str,
    day: &str,
    metric: &str,
    extra: Document,
) -> Option<i64> {
    let mut filter = doc! { "source": source, "day": day, "metric": metric };
    for (k, v) in extra {
        filter.insert(format!("dims.{k}"), v);
    }
    app.db
        .collection::<Document>(GROWTH_DAILY)
        .find_one(filter)
        .await
        .unwrap()
        .and_then(|d| d.get_i64("value").ok())
}

async fn pull(app: &TestApp, source: &str, day: &str) -> Option<(String, Option<String>)> {
    app.db
        .collection::<Document>(GROWTH_PULLS)
        .find_one(doc! { "_id": format!("{source}:{day}") })
        .await
        .unwrap()
        .map(|d| {
            (
                d.get_str("status").unwrap().to_string(),
                d.get_str("error").ok().map(str::to_string),
            )
        })
}

// ── Search Console ──────────────────────────────────────────────────────

/// Only final days: the run asks the `date` breakdown once (with
/// `dataState: all`) to learn where Google's unsettled days start, pulls
/// every final day of the horizon with three breakdowns each, leaves the
/// unsettled ones due with nothing stored, and stores the position weighted
/// by impressions. One token serves the run.
#[tokio::test]
async fn search_console_pulls_final_days_only_and_weights_position() {
    let (base, g, _) = fake_google().await;
    let app = TestApp::spawn().await;
    let client = sc(&base, SECRET);
    let t = today();
    let d1 = ds(t - Duration::days(3));
    let unsettled = [ds(t - Duration::days(2)), ds(t - Duration::days(1))];
    *g.first_incomplete.lock().unwrap() = Some(unsettled[0].clone());
    {
        let mut data = g.data.lock().unwrap();
        data.insert(
            d1.clone(),
            vec![
                (vec!["date"], row(&[], 5.0, 200.0, 6.5)),
                (
                    vec!["page"],
                    row(&["https://roomler.ai/blog/a/"], 4.0, 150.0, 5.0),
                ),
                (
                    vec!["page"],
                    row(&["https://roomler.ai/blog/b/"], 1.0, 50.0, 11.0),
                ),
                (vec!["query"], row(&["remote desktop"], 3.0, 100.0, 4.0)),
                (
                    vec!["page", "query"],
                    row(
                        &["https://roomler.ai/blog/a/", "remote desktop"],
                        3.0,
                        90.0,
                        4.0,
                    ),
                ),
            ],
        );
        // An unsettled day Google already has partial numbers for.
        data.insert(
            unsettled[0].clone(),
            vec![(vec!["date"], row(&[], 1.0, 20.0, 9.0))],
        );
    }

    let r = search_pull::run_once(&app.state.core, &client, at_day(t, 3))
        .await
        .unwrap();
    assert_eq!(r.due.len(), 30, "the horizon, on the first cycle: {r:?}");
    assert_eq!(r.pending, unsettled.to_vec(), "{r:?}");
    assert_eq!((r.pulled, r.failed), (28, 0), "{r:?}");
    assert_eq!(*g.tokens_issued.lock().unwrap(), 1, "one token for the run");
    let queries = g.queries.lock().unwrap().clone();
    assert_eq!(queries.len(), 1 + 28 * 3);
    assert!(
        queries.iter().all(|(site, _)| site == SITE),
        "the property, decoded"
    );
    assert_eq!(queries[0].1["dimensions"], json!(["date"]));
    assert_eq!(queries[0].1["dataState"], "all");

    // The final day: every breakdown, the position weighted ×100.
    let site = doc! { "by": "site" };
    assert_eq!(
        daily(&app, SEARCH_CONSOLE_SOURCE, &d1, "clicks", site.clone()).await,
        Some(5)
    );
    assert_eq!(
        daily(
            &app,
            SEARCH_CONSOLE_SOURCE,
            &d1,
            "impressions",
            site.clone()
        )
        .await,
        Some(200)
    );
    assert_eq!(
        daily(&app, SEARCH_CONSOLE_SOURCE, &d1, "position_x100", site).await,
        Some(130_000),
        "6.5 × 200 × 100"
    );
    let a = doc! { "by": "page", "page": "https://roomler.ai/blog/a/" };
    assert_eq!(
        daily(&app, SEARCH_CONSOLE_SOURCE, &d1, "position_x100", a).await,
        Some(75_000)
    );
    let pq = doc! { "by": "page_query", "page": "https://roomler.ai/blog/a/", "query": "remote desktop" };
    assert_eq!(
        daily(&app, SEARCH_CONSOLE_SOURCE, &d1, "impressions", pq).await,
        Some(90)
    );
    assert_eq!(
        pull(&app, SEARCH_CONSOLE_SOURCE, &d1).await,
        Some(("ok".into(), None))
    );
    // A final day with no data is pulled `ok` with no row: a zero.
    let quiet = ds(t - Duration::days(10));
    assert_eq!(
        pull(&app, SEARCH_CONSOLE_SOURCE, &quiet).await,
        Some(("ok".into(), None))
    );
    // The unsettled days: nothing stored, no pull row — pending, not 0.
    for d in &unsettled {
        assert_eq!(pull(&app, SEARCH_CONSOLE_SOURCE, d).await, None, "{d}");
        assert_eq!(
            count(
                &app,
                GROWTH_DAILY,
                doc! { "source": SEARCH_CONSOLE_SOURCE, "day": d }
            )
            .await,
            0
        );
    }

    // Later in the same cycle: the settled days are done; the unsettled
    // ones are asked about again, and still left alone.
    let before = g.queries.lock().unwrap().len();
    let again = search_pull::run_once(&app.state.core, &client, at_day(t, 4))
        .await
        .unwrap();
    assert_eq!(again.due, unsettled.to_vec(), "{again:?}");
    assert_eq!(again.pulled, 0);
    assert_eq!(
        g.queries.lock().unwrap().len(),
        before + 1,
        "only the date breakdown"
    );

    // The next cycle: Google calls the first of them final now.
    *g.first_incomplete.lock().unwrap() = Some(unsettled[1].clone());
    let next = search_pull::run_once(&app.state.core, &client, at_day(t + Duration::days(1), 3))
        .await
        .unwrap();
    assert!(next.pending.contains(&unsettled[1]), "{next:?}");
    assert_eq!(
        pull(&app, SEARCH_CONSOLE_SOURCE, &unsettled[0]).await,
        Some(("ok".into(), None))
    );
    assert_eq!(
        daily(
            &app,
            SEARCH_CONSOLE_SOURCE,
            &unsettled[0],
            "clicks",
            doc! { "by": "site" }
        )
        .await,
        Some(1)
    );
}

/// A refused assertion and a property the account is not a user of are
/// both recorded on every day the run owed, with the reason; nothing is
/// stored.
#[tokio::test]
async fn a_refused_token_or_permission_is_recorded_never_zeroed() {
    let (base, g, _) = fake_google().await;
    let t = today();

    let app = TestApp::spawn().await;
    let r = search_pull::run_once(&app.state.core, &sc(&base, b"the-wrong-key"), at_day(t, 3))
        .await
        .unwrap();
    assert_eq!(r.failed, 30, "{r:?}");
    let reason = "search console unreachable: token refused (invalid_grant)";
    assert_eq!(r.error.as_deref(), Some(reason));
    assert_eq!(
        pull(&app, SEARCH_CONSOLE_SOURCE, &ds(t - Duration::days(5))).await,
        Some(("failed".into(), Some(reason.into())))
    );
    assert_eq!(count(&app, GROWTH_DAILY, doc! {}).await, 0);
    assert_eq!(*g.tokens_issued.lock().unwrap(), 0);

    *g.forbid.lock().unwrap() = true;
    let app = TestApp::spawn().await;
    let r = search_pull::run_once(&app.state.core, &sc(&base, SECRET), at_day(t, 3))
        .await
        .unwrap();
    assert_eq!(r.failed, 30, "{r:?}");
    let e = r.error.unwrap();
    assert!(
        e.starts_with(
            "search console unreachable: forbidden (403) User does not have sufficient permission"
        ),
        "{e}"
    );
    assert_eq!(count(&app, GROWTH_DAILY, doc! {}).await, 0);
}

// ── The feed and URL Inspection ─────────────────────────────────────────

fn feed_xml(posts: &[(&str, &str, &str)]) -> String {
    let mut s = String::from(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<feed xmlns=\"http://www.w3.org/2005/Atom\" xml:lang=\"en\">\n  <id>https://roomler.ai/blog/</id>\n  <title>Roomler blog</title>\n",
    );
    for (slug, title, published) in posts {
        s.push_str(&format!(
            "  <entry>\n    <id>https://roomler.ai/blog/{slug}/</id>\n    <title>{title}</title>\n    <link rel=\"alternate\" type=\"text/html\" href=\"https://roomler.ai/blog/{slug}/\"/>\n    <published>{published}</published>\n    <updated>{published}</updated>\n    <author><name>Goran</name></author>\n  </entry>\n"
        ));
    }
    s.push_str("</feed>\n");
    s
}

/// The feed fills `growth_content`; URL Inspection adds each post's verdict
/// and last crawl; a post that leaves the feed is kept as `in_feed: false`;
/// a refused inspection keeps the last good verdict, records the error and
/// fails the day's inspection row; a missing feed is no blog, not a failure.
#[tokio::test]
async fn the_feed_and_url_inspection_fill_growth_content() {
    let (base, g, f) = fake_google().await;
    let app = TestApp::spawn().await;
    let client = sc(&base, SECRET);
    let feed_url = format!("{base}/blog/feed.xml");
    let t = today();
    *f.0.lock().unwrap() = Some(feed_xml(&[
        ("a", "Post &amp; A", "2026-09-25T22:45:13Z"),
        ("b", "Post B", "2026-10-01T08:00:00Z"),
    ]));

    let r = content_pull::run_once(&app.state.core, &feed_url, Some(&client), at_day(t, 3))
        .await
        .unwrap();
    assert!(r.feed_due && r.inspection_due, "{r:?}");
    assert_eq!(
        (r.posts, r.inspected, r.inspection_failed),
        (2, 2, 0),
        "{r:?}"
    );
    let posts = app.db.collection::<Document>(GROWTH_CONTENT);
    let a = posts
        .find_one(doc! { "_id": "/blog/a/" })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(a.get_str("title").unwrap(), "Post & A");
    assert!(a.get_bool("in_feed").unwrap());
    let ia = a.get_document("inspection").unwrap();
    assert_eq!(ia.get_str("verdict").unwrap(), "PASS");
    assert_eq!(
        ia.get_str("coverage_state").unwrap(),
        "Submitted and indexed"
    );
    assert!(ia.get_datetime("last_crawl_time").is_ok());
    let b = posts
        .find_one(doc! { "_id": "/blog/b/" })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        b.get_document("inspection")
            .unwrap()
            .get_str("coverage_state")
            .unwrap(),
        "Crawled - currently not indexed"
    );
    let today_s = ds(t);
    assert_eq!(
        pull(&app, content_pull::CONTENT_SOURCE, &today_s).await,
        Some(("ok".into(), None))
    );
    assert_eq!(
        pull(&app, content_pull::INSPECTION_SOURCE, &today_s).await,
        Some(("ok".into(), None))
    );

    // Once per cycle: a later tick reads and inspects nothing.
    let again = content_pull::run_once(&app.state.core, &feed_url, Some(&client), at_day(t, 5))
        .await
        .unwrap();
    assert!(!again.feed_due && !again.inspection_due);
    assert_eq!(g.inspected.lock().unwrap().len(), 2);

    // The next day: B left the feed, and inspection is refused.
    *f.0.lock().unwrap() = Some(feed_xml(&[("a", "Post A", "2026-09-25T22:45:13Z")]));
    *g.forbid_inspection.lock().unwrap() = true;
    let next = t + Duration::days(1);
    let r = content_pull::run_once(&app.state.core, &feed_url, Some(&client), at_day(next, 3))
        .await
        .unwrap();
    assert_eq!((r.posts, r.left_feed), (1, 1), "{r:?}");
    let b = posts
        .find_one(doc! { "_id": "/blog/b/" })
        .await
        .unwrap()
        .unwrap();
    assert!(!b.get_bool("in_feed").unwrap(), "kept, out of the feed");
    let a = posts
        .find_one(doc! { "_id": "/blog/a/" })
        .await
        .unwrap()
        .unwrap();
    let ia = a.get_document("inspection").unwrap();
    assert_eq!(
        ia.get_str("verdict").unwrap(),
        "PASS",
        "the last good verdict stays"
    );
    assert!(ia.get_str("error").unwrap().contains("forbidden (403)"));
    let (status, error) = pull(&app, content_pull::INSPECTION_SOURCE, &ds(next))
        .await
        .unwrap();
    assert_eq!(status, "failed");
    assert!(error.unwrap().contains("forbidden (403)"));

    // No feed at all is no blog — an empty read, not a failure. Without a
    // Search Console client nothing is inspected.
    *f.0.lock().unwrap() = None;
    let other = TestApp::spawn().await;
    let r = content_pull::run_once(&other.state.core, &feed_url, None, at_day(t, 3))
        .await
        .unwrap();
    assert_eq!((r.posts, r.inspection_due), (0, false), "{r:?}");
    assert_eq!(
        pull(&other, content_pull::CONTENT_SOURCE, &ds(t)).await,
        Some(("ok".into(), None))
    );
}

// ── Bing ────────────────────────────────────────────────────────────────

#[derive(Clone, Default)]
struct BingFake {
    /// `(method, siteUrl)` per request.
    seen: Arc<Mutex<Vec<(String, String)>>>,
}

/// Bing's date for the Pacific day `d`: midnight there, -0700.
fn bing_date(d: NaiveDate) -> String {
    let ms = d.and_hms_opt(7, 0, 0).unwrap().and_utc().timestamp_millis();
    format!("/Date({ms}-0700)/")
}

async fn bing_call(
    State(b): State<BingFake>,
    Path(method): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> (StatusCode, Json<Value>) {
    b.seen.lock().unwrap().push((
        method.clone(),
        q.get("siteUrl").cloned().unwrap_or_default(),
    ));
    if q.get("apikey").map(String::as_str) != Some("good") {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "ErrorCode": 3, "Message": "ERROR!!! InvalidApiKey" })),
        );
    }
    let t = today();
    let d = |age: i64| bing_date(t - Duration::days(age));
    let body = match method.as_str() {
        "GetRankAndTrafficStats" => json!({ "d": [
            { "__type": "RankAndTrafficStats:#Microsoft.Bing.Webmaster.Api", "Date": d(2), "Clicks": 3, "Impressions": 90 },
            { "__type": "RankAndTrafficStats:#Microsoft.Bing.Webmaster.Api", "Date": d(40), "Clicks": 1, "Impressions": 10 },
        ] }),
        "GetQueryStats" => json!({ "d": [
            { "Query": "roomler", "Date": d(6), "Clicks": 2, "Impressions": 30, "AvgImpressionPosition": 4, "AvgClickPosition": 3 },
        ] }),
        "GetPageStats" => json!({ "d": [
            { "Query": "https://roomler.ai/blog/a/", "Date": d(6), "Clicks": 1, "Impressions": 12, "AvgImpressionPosition": 9, "AvgClickPosition": -1 },
        ] }),
        _ => return (StatusCode::NOT_FOUND, Json(json!({}))),
    };
    (StatusCode::OK, Json(body))
}

async fn fake_bing() -> (String, BingFake) {
    let b = BingFake::default();
    let router = Router::new()
        .route("/{method}", get(bing_call))
        .with_state(b.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (format!("http://{addr}"), b)
}

fn bing(base: &str, key: &str) -> BingClient {
    BingClient::from_settings(&GrowthSettings {
        bing_api_key: key.into(),
        bing_api_url: base.into(),
        ..Default::default()
    })
    .unwrap()
}

/// The daily totals and the weekly buckets land at Bing's own grain inside
/// their horizons, under Bing's (Pacific) dates; a refused key is recorded
/// on the cycle's row with Bing's code.
#[tokio::test]
async fn bing_lands_at_its_own_grain_and_a_refused_key_is_recorded() {
    let (base, fake) = fake_bing().await;
    let app = TestApp::spawn().await;
    let t = today();
    let r = bing_pull::run_once(&app.state.core, &bing(&base, "good"), at_day(t, 4))
        .await
        .unwrap();
    assert!(r.due && r.ok, "{r:?}");
    let seen = fake.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 3);
    assert!(
        seen.iter().all(|(_, site)| site == "https://roomler.ai/"),
        "{seen:?}"
    );

    let d2 = ds(t - Duration::days(2));
    assert_eq!(
        daily(&app, BING_SOURCE, &d2, "clicks", doc! { "by": "site" }).await,
        Some(3)
    );
    assert_eq!(
        count(
            &app,
            GROWTH_DAILY,
            doc! { "source": BING_SOURCE, "day": ds(t - Duration::days(40)) }
        )
        .await,
        0,
        "older than the horizon: frozen, not written"
    );
    let w = ds(t - Duration::days(6));
    assert_eq!(
        daily(
            &app,
            BING_WEEKLY_SOURCE,
            &w,
            "impressions",
            doc! { "by": "query", "query": "roomler" }
        )
        .await,
        Some(30)
    );
    assert_eq!(
        daily(
            &app,
            BING_WEEKLY_SOURCE,
            &w,
            "avg_impression_position_x10",
            doc! { "by": "query" }
        )
        .await,
        Some(40)
    );
    assert_eq!(
        daily(
            &app,
            BING_WEEKLY_SOURCE,
            &w,
            "avg_click_position_x10",
            doc! { "by": "page" }
        )
        .await,
        None,
        "Bing's -1 is no position"
    );
    let y = ds(t - Duration::days(1));
    assert_eq!(pull(&app, BING_SOURCE, &y).await, Some(("ok".into(), None)));

    // Once per cycle.
    let again = bing_pull::run_once(&app.state.core, &bing(&base, "good"), at_day(t, 6))
        .await
        .unwrap();
    assert!(!again.due);

    let bad_app = TestApp::spawn().await;
    let r = bing_pull::run_once(&bad_app.state.core, &bing(&base, "revoked"), at_day(t, 4))
        .await
        .unwrap();
    assert!(r.due && !r.ok);
    let reason = "bing unreachable: refused (ErrorCode 3) ERROR!!! InvalidApiKey";
    assert_eq!(r.error.as_deref(), Some(reason));
    assert_eq!(
        pull(&bad_app, BING_SOURCE, &y).await,
        Some(("failed".into(), Some(reason.into())))
    );
    assert_eq!(count(&bad_app, GROWTH_DAILY, doc! {}).await, 0);
}
