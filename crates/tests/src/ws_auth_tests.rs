//! `/ws` authentication — the session-cookie path and the origin guard that
//! has to come with it.
//!
//! Why these are integration tests and not unit tests: the unit tests can only
//! prove that `session_cookie` parses a header and that `is_trusted` compares
//! two strings. What matters is whether the UPGRADE actually honours a cookie,
//! and whether it actually refuses a foreign page — and that is a property of
//! the axum handler wiring, not of either helper.

use crate::fixtures::test_app::TestApp;
use futures::StreamExt;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::{COOKIE, ORIGIN};
use tokio_tungstenite::tungstenite::http::{HeaderValue, StatusCode};

/// The origin the test server considers its own. Set explicitly rather than
/// relying on the fixture default, so a change to that default surfaces here
/// as a failing assertion rather than as a mysteriously-passing test.
const OURS: &str = "http://localhost:5173";

async fn spawn() -> TestApp {
    TestApp::spawn_with_settings(|s| {
        s.app.frontend_url = OURS.to_string();
        s.app.cors_origins = vec![];
    })
    .await
}

/// Build a `/ws` handshake with the given credential placement.
fn handshake(
    addr: impl std::fmt::Display,
    query_token: Option<&str>,
    cookie: Option<&str>,
    origin: Option<&str>,
) -> tokio_tungstenite::tungstenite::handshake::client::Request {
    let url = match query_token {
        Some(t) => format!("ws://{addr}/ws?token={t}"),
        None => format!("ws://{addr}/ws"),
    };
    let mut req = url.into_client_request().expect("ws request");
    if let Some(c) = cookie {
        req.headers_mut().insert(
            COOKIE,
            HeaderValue::from_str(&format!("access_token={c}")).unwrap(),
        );
    }
    if let Some(o) = origin {
        req.headers_mut()
            .insert(ORIGIN, HeaderValue::from_str(o).unwrap());
    }
    req
}

/// The HTTP status a failed handshake came back with, so a test can assert
/// WHY it was refused rather than merely that it was.
fn refusal_status(err: tokio_tungstenite::tungstenite::Error) -> Option<StatusCode> {
    match err {
        tokio_tungstenite::tungstenite::Error::Http(resp) => Some(resp.status()),
        _ => None,
    }
}

#[tokio::test]
async fn a_session_cookie_authenticates_a_browser_socket() {
    // The point of the whole change: a browser on our own origin can open the
    // socket with no credential in the URL at all.
    let app = spawn().await;
    let seeded = app.seed_tenant("wsauth-cookie").await;

    let req = handshake(
        &app.addr,
        None,
        Some(&seeded.member.access_token),
        Some(OURS),
    );
    let (mut ws, resp) = connect_async(req).await.expect("cookie-authed ws connect");
    assert_eq!(resp.status(), StatusCode::SWITCHING_PROTOCOLS);

    // Prove it is a real session, not just an accepted handshake.
    let first = tokio::time::timeout(std::time::Duration::from_secs(5), ws.next())
        .await
        .expect("server sent a frame")
        .expect("stream open")
        .expect("frame ok");
    let text = first.into_text().expect("text frame");
    assert!(
        text.contains("connected"),
        "expected the `connected` frame, got: {text}"
    );
}

#[tokio::test]
async fn a_foreign_origin_cannot_spend_the_cookie() {
    // A WebSocket upgrade is not subject to CORS, so any page may attempt
    // this. `SameSite=Lax` means a real browser would not attach the cookie —
    // this asserts the server does not rely on that alone.
    let app = spawn().await;
    let seeded = app.seed_tenant("wsauth-foreign").await;

    let req = handshake(
        &app.addr,
        None,
        Some(&seeded.member.access_token),
        Some("https://evil.example"),
    );
    let err = connect_async(req)
        .await
        .expect_err("a foreign origin must not open a cookie-authed socket");
    assert_eq!(
        refusal_status(err),
        Some(StatusCode::FORBIDDEN),
        "expected 403 for a foreign origin"
    );
}

#[tokio::test]
async fn a_cookie_with_no_origin_at_all_is_refused() {
    // Browsers ALWAYS send Origin on a WS handshake, so an absent one means
    // the caller is not a browser — and a non-browser holding a browser
    // session cookie is not a case to accommodate.
    let app = spawn().await;
    let seeded = app.seed_tenant("wsauth-noorigin").await;

    let req = handshake(&app.addr, None, Some(&seeded.member.access_token), None);
    let err = connect_async(req)
        .await
        .expect_err("no Origin must not open a cookie-authed socket");
    assert_eq!(refusal_status(err), Some(StatusCode::FORBIDDEN));
}

#[tokio::test]
async fn the_query_token_still_works_and_needs_no_origin() {
    // The compatibility guarantee this change rests on: the server must accept
    // the cookie BEFORE the UI stops sending the token, because during a
    // rolling deploy a browser can hit either pod. A regression here would
    // break every currently-cached bundle, and every native client.
    let app = spawn().await;
    let seeded = app.seed_tenant("wsauth-legacy").await;

    let req = handshake(&app.addr, Some(&seeded.member.access_token), None, None);
    let (mut ws, resp) = connect_async(req).await.expect("legacy ws connect");
    assert_eq!(resp.status(), StatusCode::SWITCHING_PROTOCOLS);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), ws.next()).await;
}

#[tokio::test]
async fn a_foreign_origin_does_not_block_the_query_token() {
    // The origin guard is scoped to the AMBIENT credential. A query token is
    // one the caller had to obtain, so gating it on Origin would buy nothing
    // and would break native clients, which send no Origin.
    let app = spawn().await;
    let seeded = app.seed_tenant("wsauth-legacy-origin").await;

    let req = handshake(
        &app.addr,
        Some(&seeded.member.access_token),
        None,
        Some("https://evil.example"),
    );
    let (_ws, resp) = connect_async(req)
        .await
        .expect("a query token is not origin-gated");
    assert_eq!(resp.status(), StatusCode::SWITCHING_PROTOCOLS);
}

#[tokio::test]
async fn no_credential_at_all_is_refused() {
    let app = spawn().await;
    let req = handshake(&app.addr, None, None, Some(OURS));
    let err = connect_async(req)
        .await
        .expect_err("an anonymous socket must be refused");
    assert_eq!(refusal_status(err), Some(StatusCode::UNAUTHORIZED));
}

#[tokio::test]
async fn a_garbage_cookie_is_refused_even_from_our_own_origin() {
    // The origin guard answers "who is asking", never "is this a valid
    // session" — the token still has to verify.
    let app = spawn().await;
    let _ = app.seed_tenant("wsauth-garbage").await;

    let req = handshake(&app.addr, None, Some("not-a-jwt"), Some(OURS));
    let err = connect_async(req)
        .await
        .expect_err("an invalid session cookie must be refused");
    assert_eq!(refusal_status(err), Some(StatusCode::UNAUTHORIZED));
}
