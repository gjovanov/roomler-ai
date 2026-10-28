// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-129 A3 — the device-pairing client: the wizard's (and, in A4, the
//! daemon's) half of `POST /api/pair/start` and `POST /api/pair/poll`
//! (`docs/fr/FR-129-guided-cross-platform-installer.md`, Key design A).
//!
//! RFC 8628's split, on the wire this crate speaks:
//!
//! * [`start`] asks the server for a pairing and gets two codes back. The
//!   **`device_code`** is the pairing's secret — the only thing that can
//!   retrieve a token — and lives here as [`DeviceCode`], which has no
//!   `Display` and a `Debug` that prints nothing of it: it is sent in a POST
//!   body and nowhere else, never shown, never logged, never in a URL. The
//!   **`user_code`** (`BCDF-GHJK`) names the pairing and retrieves nothing;
//!   the person reads it off the wizard and types it in the browser.
//! * [`poll`] asks whether the browser has approved it. The server answers
//!   with a status from a fixed vocabulary ([`PollOutcome`]); a spelling this
//!   build does not know is [`PollOutcome::Unknown`], never a parse error — a
//!   newer server may add one, and an older client must keep polling or
//!   stop on the clock, not crash.
//! * Delivery is once: the first poll that sees `approved` carries the
//!   enrollment token(s), and the next answers `expired_token`.
//!
//! A `404` on `start` means the server has no pairing — the `[pairing]
//! enabled = false` kill switch, or a server from before FR-129 — and is
//! [`StartOutcome::Unavailable`], not an error: the caller falls back to the
//! pasted token silently.
//!
//! The HTTP client is built exactly as [`crate::enrollment::enroll`] builds
//! its own (`reqwest::Client::new()`), so pairing reaches every server the
//! enrollment reaches — corporate proxies and TLS-inspecting middleboxes
//! included — and the server URL takes the same `https://` promotion.

use std::{fmt, time::Duration};

use anyhow::{Context, Result, bail};
use roomler_ai_remote_control::models::OsKind;
use serde::{Deserialize, Serialize};

/// `POST {server}/api/pair/start`.
pub const START_ROUTE: &str = "/api/pair/start";
/// `POST {server}/api/pair/poll`.
pub const POLL_ROUTE: &str = "/api/pair/poll";

/// How long one request may take, connect included. The server answers a
/// poll at once, so this only bounds a hung connection — a caller's poll
/// loop must not wait on the TCP stack's own patience.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// RFC 8628 §3.2 — the poll interval when the server names none.
pub const DEFAULT_INTERVAL_SECS: u64 = 5;

/// What the pairing is for: the token kind the approval mints. `daemon`
/// pairs for an agent `Enrollment` token, `tunnel` for a `TunnelEnrollment`
/// one (the spec's Key design A, item 2).
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PairRole {
    Daemon,
    Tunnel,
}

impl PairRole {
    /// The wire spelling, for a log line or a label.
    pub fn as_str(&self) -> &'static str {
        match self {
            PairRole::Daemon => "daemon",
            PairRole::Tunnel => "tunnel",
        }
    }
}

/// The body of `POST /api/pair/start`. Everything here is shown to the
/// person approving — the `/pair` page lists the device's name, OS and
/// version before Approve (AC4) — so it carries nothing secret.
#[derive(Clone, Debug, Serialize)]
pub struct StartRequest {
    /// The device's name as the wizard's Server page has it.
    pub machine_name: String,
    /// `linux` / `macos` / `windows` — [`host_os`].
    pub os: OsKind,
    /// `x86_64` / `aarch64` — [`host_arch`].
    pub arch: String,
    pub role: PairRole,
    /// How many enrollments one approval covers: `1` everywhere today; B2's
    /// macOS root half makes it `2`.
    pub halves: u8,
    /// Who is asking: `wizard` (A3), `roomlerd` / `install.sh` (A4).
    pub client: String,
    /// The asking client's version — shown on the `/pair` page.
    pub client_version: String,
}

/// The pairing's secret. Sent in a POST body, held in memory, and nothing
/// else: no `Display`, and a `Debug` that prints only its presence, so a
/// `{:?}` of any struct carrying it can never leak it into a log line
/// (AC3).
#[derive(Clone, Eq, PartialEq)]
pub struct DeviceCode(String);

impl DeviceCode {
    pub fn new(code: impl Into<String>) -> Self {
        Self(code.into())
    }

    /// The bytes, for the one place that may see them: the poll body.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for DeviceCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DeviceCode(<redacted>)")
    }
}

/// A pairing the server opened.
#[derive(Clone, Debug)]
pub struct Started {
    pub device_code: DeviceCode,
    /// `BCDF-GHJK` — what the person types.
    pub user_code: String,
    /// Where they type it. Carries no code (operator decision 1).
    pub verification_uri: String,
    /// Seconds until the pairing expires.
    pub expires_in: u64,
    /// Seconds between polls.
    pub interval: u64,
}

/// What [`start`] came back with.
#[derive(Clone, Debug)]
pub enum StartOutcome {
    Started(Started),
    /// The server answered 404: pairing is switched off there, or the
    /// server predates it. Fall back to a pasted token, silently.
    Unavailable,
}

/// An approval the server delivered, with the token(s) it minted. The
/// tokens are ordinary single-use enrollment tokens and go through the
/// unchanged enroll path; they are secrets, so `Debug` counts them and
/// prints none.
#[derive(Clone, Eq, PartialEq)]
pub struct Approved {
    /// One per half (`halves` in the start request).
    pub tokens: Vec<String>,
    /// The org the approver chose — shown before the install (AC5). A
    /// display name: NOT unique, and chosen by whoever made the org.
    pub org_name: String,
    /// The org's slug — unique, so the person can tell their org from one
    /// merely named like it. `None` from a server before A1's review fix.
    pub org_slug: Option<String>,
    /// Who approved — shown beside it. A display name: free text.
    pub approver_name: String,
    /// The approver's username — unique, so a display name copied from the
    /// real admin does not pass as them. `None` from an older server.
    pub approver_username: Option<String>,
}

impl fmt::Debug for Approved {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Approved")
            .field("tokens", &format_args!("<{} redacted>", self.tokens.len()))
            .field("org_name", &self.org_name)
            .field("org_slug", &self.org_slug)
            .field("approver_name", &self.approver_name)
            .field("approver_username", &self.approver_username)
            .finish()
    }
}

/// The server's answer to one poll.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PollOutcome {
    /// Nobody has decided yet: poll again after the interval.
    AuthorizationPending,
    /// Polled too fast: poll again after this (longer) interval.
    SlowDown { interval: u64 },
    /// HTTP 429 — not the pairing's own `slow_down` but the global per-IP
    /// governor, which several installers behind one corporate NAT trip
    /// together. Treated like `slow_down` by the caller: the interval grows
    /// by 5 s, never shrinks, and a `Retry-After` (seconds) is honoured.
    RateLimited { retry_after: Option<u64> },
    /// The person denied it in the browser. Final.
    AccessDenied,
    /// The pairing ran out, or its token was already delivered. Final.
    ExpiredToken,
    /// Approved, and here are the token(s). Delivered once.
    Approved(Approved),
    /// A status this build has never heard of, verbatim. Not an error: a
    /// newer server may add one, and the caller's clock still ends the loop.
    Unknown(String),
}

impl PollOutcome {
    /// The status's wire spelling — a log line's `status` field.
    pub fn status(&self) -> &str {
        match self {
            PollOutcome::AuthorizationPending => "authorization_pending",
            PollOutcome::SlowDown { .. } => "slow_down",
            PollOutcome::RateLimited { .. } => "rate_limited",
            PollOutcome::AccessDenied => "access_denied",
            PollOutcome::ExpiredToken => "expired_token",
            PollOutcome::Approved(_) => "approved",
            PollOutcome::Unknown(s) => s,
        }
    }
}

/// RFC 8628 §3.5 — what a poll interval grows by on `slow_down`, and what
/// this client adds on its own for a 429 (which carries no interval).
pub const SLOW_DOWN_STEP_SECS: u64 = 5;

/// The next poll interval after a 429: the current one plus the slow-down
/// step, or the server's `Retry-After` when that is longer — never shorter
/// than what was polled with.
pub fn interval_after_rate_limit(current: u64, retry_after: Option<u64>) -> u64 {
    let grown = current.saturating_add(SLOW_DOWN_STEP_SECS);
    grown.max(retry_after.unwrap_or(0))
}

/// The next poll interval after a `slow_down`: the server's, unless the
/// current one is already longer.
pub fn interval_after_slow_down(current: u64, server: u64) -> u64 {
    current.max(server)
}

// ─── The wire shapes, private: the public types above are what callers hold ──

#[derive(Deserialize)]
struct StartWire {
    device_code: String,
    user_code: String,
    verification_uri: String,
    expires_in: u64,
    #[serde(default = "default_interval")]
    interval: u64,
}

fn default_interval() -> u64 {
    DEFAULT_INTERVAL_SECS
}

#[derive(Serialize)]
struct PollWire<'a> {
    device_code: &'a str,
}

/// Every field but `status` is optional: the vocabulary says which ones a
/// status carries, and a server that omits one on a status that needs it is
/// an error at the mapping, not a parse failure of the whole body.
#[derive(Deserialize)]
struct PollAnswerWire {
    status: String,
    #[serde(default)]
    interval: Option<u64>,
    #[serde(default)]
    tokens: Option<Vec<String>>,
    #[serde(default)]
    org_name: Option<String>,
    #[serde(default)]
    org_slug: Option<String>,
    #[serde(default)]
    approver_name: Option<String>,
    #[serde(default)]
    approver_username: Option<String>,
}

impl PollAnswerWire {
    fn into_outcome(self) -> Result<PollOutcome> {
        Ok(match self.status.as_str() {
            "authorization_pending" => PollOutcome::AuthorizationPending,
            "slow_down" => PollOutcome::SlowDown {
                interval: self.interval.unwrap_or(DEFAULT_INTERVAL_SECS),
            },
            "access_denied" => PollOutcome::AccessDenied,
            "expired_token" => PollOutcome::ExpiredToken,
            "approved" => {
                let tokens = self.tokens.unwrap_or_default();
                if tokens.is_empty() || tokens.iter().any(|t| t.trim().is_empty()) {
                    bail!("pairing approved but the server delivered no token");
                }
                PollOutcome::Approved(Approved {
                    tokens,
                    org_name: self.org_name.unwrap_or_default(),
                    org_slug: self.org_slug.filter(|s| !s.trim().is_empty()),
                    approver_name: self.approver_name.unwrap_or_default(),
                    approver_username: self.approver_username.filter(|s| !s.trim().is_empty()),
                })
            }
            _ => PollOutcome::Unknown(self.status),
        })
    }
}

/// A `Retry-After` header's delay in seconds. Only the delta-seconds form
/// is read; the HTTP-date form (rare from a governor) reads as absent, and
/// the caller's own step still applies.
fn retry_after_secs(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
}

// ─── The two calls ──────────────────────────────────────────────────────────

/// Open a pairing. `server` takes the enrollment's own `https://` promotion
/// ([`crate::enrollment`]): the device code must travel over TLS.
pub async fn start(server: &str, request: &StartRequest) -> Result<StartOutcome> {
    let server = crate::enrollment::normalize_server_url(server);
    let url = format!("{server}{START_ROUTE}");
    tracing::info!(
        %url,
        role = request.role.as_str(),
        os = ?request.os,
        arch = %request.arch,
        halves = request.halves,
        "starting a device pairing"
    );

    let resp = reqwest::Client::new()
        .post(&url)
        .timeout(REQUEST_TIMEOUT)
        .json(request)
        .send()
        .await
        .context("POST /api/pair/start")?;

    let status = resp.status();
    if status.as_u16() == 404 {
        tracing::info!("pairing unavailable on this server (404): paste a token instead");
        return Ok(StartOutcome::Unavailable);
    }
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        bail!("pairing start rejected (status {status}): {body}");
    }

    let wire: StartWire = resp
        .json()
        .await
        .context("parsing the pair/start response")?;
    if wire.device_code.trim().is_empty() || wire.user_code.trim().is_empty() {
        bail!("pairing start answered without a code");
    }
    tracing::info!(
        expires_in = wire.expires_in,
        interval = wire.interval,
        "device pairing started"
    );
    Ok(StartOutcome::Started(Started {
        device_code: DeviceCode(wire.device_code),
        user_code: wire.user_code,
        verification_uri: wire.verification_uri,
        expires_in: wire.expires_in,
        interval: wire.interval,
    }))
}

/// Ask once whether the pairing was approved. The device code goes in the
/// body and nowhere else.
pub async fn poll(server: &str, device_code: &DeviceCode) -> Result<PollOutcome> {
    let server = crate::enrollment::normalize_server_url(server);
    let url = format!("{server}{POLL_ROUTE}");

    let resp = reqwest::Client::new()
        .post(&url)
        .timeout(REQUEST_TIMEOUT)
        .json(&PollWire {
            device_code: device_code.expose(),
        })
        .send()
        .await
        .context("POST /api/pair/poll")?;

    let status = resp.status();
    if status.as_u16() == 429 {
        // The global per-IP governor, not the pairing: typed, so the caller
        // backs off like a `slow_down` instead of retrying at the same pace.
        let retry_after = retry_after_secs(resp.headers());
        tracing::info!(?retry_after, "pairing poll rate-limited (429)");
        return Ok(PollOutcome::RateLimited { retry_after });
    }
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        bail!("pairing poll rejected (status {status}): {body}");
    }
    let wire: PollAnswerWire = resp
        .json()
        .await
        .context("parsing the pair/poll response")?;
    let outcome = wire.into_outcome()?;
    tracing::debug!(status = outcome.status(), "device pairing polled");
    Ok(outcome)
}

// ─── What this host is ───────────────────────────────────────────────────────

/// This build's OS, in the enroll's spelling.
pub fn host_os() -> OsKind {
    crate::enrollment::detect_os()
}

/// This build's CPU architecture, as the server lists it (`x86_64`,
/// `aarch64`). Rust's own spelling is the wire spelling.
pub fn host_arch() -> String {
    std::env::consts::ARCH.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
    };

    /// One request the loopback server saw: its path and its body.
    #[derive(Clone, Debug)]
    struct Seen {
        path: String,
        body: String,
    }

    /// A loopback HTTP server that reads each whole request, records its
    /// path and body, and answers the next scripted `(status line, body)` —
    /// the last one again once the script is used up. Returns its base URL
    /// and the record.
    async fn scripted_server(
        script: Vec<(&'static str, &'static str)>,
    ) -> (String, Arc<Mutex<Vec<Seen>>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let record = seen.clone();
        let answers = Arc::new(Mutex::new(
            script
                .into_iter()
                .map(|(s, b)| (s.to_string(), b.to_string()))
                .collect::<VecDeque<_>>(),
        ));
        tokio::spawn(async move {
            while let Ok((mut s, _)) = listener.accept().await {
                let record = record.clone();
                let answers = answers.clone();
                tokio::spawn(async move {
                    let mut req: Vec<u8> = Vec::new();
                    let mut chunk = [0u8; 8192];
                    let mut want: Option<usize> = None;
                    loop {
                        if let Some(total) = want
                            && req.len() >= total
                        {
                            break;
                        }
                        let n = s.read(&mut chunk).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        req.extend_from_slice(&chunk[..n]);
                        if want.is_none()
                            && let Some(end) = req.windows(4).position(|w| w == b"\r\n\r\n")
                        {
                            let head = String::from_utf8_lossy(&req[..end]).to_ascii_lowercase();
                            let len = head
                                .lines()
                                .find_map(|l| l.strip_prefix("content-length:"))
                                .and_then(|v| v.trim().parse::<usize>().ok())
                                .unwrap_or(0);
                            want = Some(end + 4 + len);
                        }
                    }
                    let text = String::from_utf8_lossy(&req).to_string();
                    let path = text
                        .lines()
                        .next()
                        .and_then(|l| l.split_whitespace().nth(1))
                        .unwrap_or_default()
                        .to_string();
                    let body = text
                        .split_once("\r\n\r\n")
                        .map(|(_, b)| b.to_string())
                        .unwrap_or_default();
                    record.lock().unwrap().push(Seen { path, body });
                    let (status, reply_body) = {
                        let mut a = answers.lock().unwrap();
                        if a.len() > 1 {
                            a.pop_front().unwrap()
                        } else {
                            a.front().cloned().unwrap_or_else(|| {
                                ("500 Internal Server Error".to_string(), String::new())
                            })
                        }
                    };
                    let reply = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply_body}",
                        reply_body.len()
                    );
                    let _ = s.write_all(reply.as_bytes()).await;
                });
            }
        });
        (base, seen)
    }

    fn request() -> StartRequest {
        StartRequest {
            machine_name: "reception-pc".into(),
            os: OsKind::Windows,
            arch: "x86_64".into(),
            role: PairRole::Daemon,
            halves: 1,
            client: "wizard".into(),
            client_version: "0.4.180".into(),
        }
    }

    const STARTED: &str = r#"{"device_code":"dc-SECRET-0123456789","user_code":"BCDF-GHJK","verification_uri":"https://example.invalid/pair","expires_in":900,"interval":5}"#;

    /// The start body is the contract's seven fields with their wire
    /// spellings — `os` and `role` lowercase — and the answer's codes land
    /// in the typed outcome.
    #[tokio::test]
    async fn start_posts_the_contract_and_reads_the_codes() {
        let (base, seen) = scripted_server(vec![("200 OK", STARTED)]).await;
        let outcome = start(&base, &request()).await.unwrap();
        let StartOutcome::Started(s) = outcome else {
            panic!("expected Started, got {outcome:?}");
        };
        assert_eq!(s.device_code.expose(), "dc-SECRET-0123456789");
        assert_eq!(s.user_code, "BCDF-GHJK");
        assert_eq!(s.verification_uri, "https://example.invalid/pair");
        assert_eq!(s.expires_in, 900);
        assert_eq!(s.interval, 5);

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].path, START_ROUTE);
        let body: serde_json::Value = serde_json::from_str(&seen[0].body).unwrap();
        assert_eq!(
            body,
            serde_json::json!({
                "machine_name": "reception-pc",
                "os": "windows",
                "arch": "x86_64",
                "role": "daemon",
                "halves": 1,
                "client": "wizard",
                "client_version": "0.4.180",
            })
        );
    }

    /// A 404 is the kill switch or an older server: `Unavailable`, not an
    /// error — the wizard then shows the paste box only (AC10).
    #[tokio::test]
    async fn a_404_on_start_is_unavailable_not_an_error() {
        let (base, _) = scripted_server(vec![("404 Not Found", "")]).await;
        assert!(matches!(
            start(&base, &request()).await.unwrap(),
            StartOutcome::Unavailable
        ));
    }

    /// Any other refusal is an error that names the status.
    #[tokio::test]
    async fn a_refusal_on_start_is_an_error_with_the_status() {
        let (base, _) =
            scripted_server(vec![("429 Too Many Requests", r#"{"error":"slow"}"#)]).await;
        let e = start(&base, &request()).await.unwrap_err().to_string();
        assert!(e.contains("429"), "{e}");
    }

    /// RFC 8628 §3.2: a start that names no interval polls every 5 s.
    #[tokio::test]
    async fn a_missing_interval_defaults_to_five_seconds() {
        let (base, _) = scripted_server(vec![(
            "200 OK",
            r#"{"device_code":"dc","user_code":"BCDF-GHJK","verification_uri":"https://example.invalid/pair","expires_in":900}"#,
        )])
        .await;
        let StartOutcome::Started(s) = start(&base, &request()).await.unwrap() else {
            panic!("expected Started");
        };
        assert_eq!(s.interval, DEFAULT_INTERVAL_SECS);
    }

    /// The poll body is the device code and nothing else, and each status
    /// in the vocabulary maps to its arm.
    #[tokio::test]
    async fn poll_sends_only_the_device_code_and_maps_every_status() {
        let (base, seen) = scripted_server(vec![
            ("200 OK", r#"{"status":"authorization_pending"}"#),
            ("200 OK", r#"{"status":"slow_down","interval":10}"#),
            ("200 OK", r#"{"status":"access_denied"}"#),
            ("200 OK", r#"{"status":"expired_token"}"#),
            (
                "200 OK",
                r#"{"status":"approved","tokens":["eyJ.a.b"],"org_name":"Acme","org_slug":"acme","approver_name":"Dana","approver_username":"dana"}"#,
            ),
            // An older server: the two unique identifiers absent.
            (
                "200 OK",
                r#"{"status":"approved","tokens":["eyJ.a.b"],"org_name":"Acme","approver_name":"Dana"}"#,
            ),
            ("200 OK", r#"{"status":"needs_a_second_factor"}"#),
        ])
        .await;
        let code = DeviceCode::new("dc-SECRET");

        assert_eq!(
            poll(&base, &code).await.unwrap(),
            PollOutcome::AuthorizationPending
        );
        assert_eq!(
            poll(&base, &code).await.unwrap(),
            PollOutcome::SlowDown { interval: 10 }
        );
        assert_eq!(poll(&base, &code).await.unwrap(), PollOutcome::AccessDenied);
        assert_eq!(poll(&base, &code).await.unwrap(), PollOutcome::ExpiredToken);
        assert_eq!(
            poll(&base, &code).await.unwrap(),
            PollOutcome::Approved(Approved {
                tokens: vec!["eyJ.a.b".into()],
                org_name: "Acme".into(),
                org_slug: Some("acme".into()),
                approver_name: "Dana".into(),
                approver_username: Some("dana".into()),
            })
        );
        assert_eq!(
            poll(&base, &code).await.unwrap(),
            PollOutcome::Approved(Approved {
                tokens: vec!["eyJ.a.b".into()],
                org_name: "Acme".into(),
                org_slug: None,
                approver_name: "Dana".into(),
                approver_username: None,
            }),
            "a server without the unique identifiers still approves"
        );
        // A spelling from a newer server is carried verbatim, never a
        // parse failure.
        assert_eq!(
            poll(&base, &code).await.unwrap(),
            PollOutcome::Unknown("needs_a_second_factor".into())
        );

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 7);
        for s in seen.iter() {
            assert_eq!(s.path, POLL_ROUTE);
            let body: serde_json::Value = serde_json::from_str(&s.body).unwrap();
            assert_eq!(body, serde_json::json!({ "device_code": "dc-SECRET" }));
        }
    }

    /// `slow_down` without an interval still backs off to the default, and
    /// `approved` without a token is an error rather than an empty install.
    #[tokio::test]
    async fn poll_edge_shapes() {
        let (base, _) = scripted_server(vec![
            ("200 OK", r#"{"status":"slow_down"}"#),
            ("200 OK", r#"{"status":"approved","tokens":[]}"#),
            ("200 OK", r#"{"status":"approved"}"#),
            ("400 Bad Request", r#"{"error":"bad device code"}"#),
        ])
        .await;
        let code = DeviceCode::new("dc");
        assert_eq!(
            poll(&base, &code).await.unwrap(),
            PollOutcome::SlowDown {
                interval: DEFAULT_INTERVAL_SECS
            }
        );
        let e = poll(&base, &code).await.unwrap_err().to_string();
        assert!(e.contains("no token"), "{e}");
        let e = poll(&base, &code).await.unwrap_err().to_string();
        assert!(e.contains("no token"), "{e}");
        let e = poll(&base, &code).await.unwrap_err().to_string();
        assert!(e.contains("400"), "{e}");
    }

    /// A 429 is the global per-IP governor (several installers behind one
    /// NAT), not the pairing: a typed outcome the caller backs off on, with
    /// `Retry-After`'s seconds when the header carries them. The HTTP-date
    /// form reads as absent; the caller's own step still applies. (The
    /// scripted status line carries the header after its own `\r\n`.)
    #[tokio::test]
    async fn a_429_on_poll_is_rate_limited_with_retry_after() {
        let (base, _) = scripted_server(vec![
            ("429 Too Many Requests\r\nRetry-After: 12", ""),
            ("429 Too Many Requests", r#"{"error":"too many requests"}"#),
            (
                "429 Too Many Requests\r\nRetry-After: Wed, 21 Oct 2026 07:28:00 GMT",
                "",
            ),
            ("429 Too Many Requests\r\nRetry-After:  3 ", ""),
        ])
        .await;
        let code = DeviceCode::new("dc");
        assert_eq!(
            poll(&base, &code).await.unwrap(),
            PollOutcome::RateLimited {
                retry_after: Some(12)
            }
        );
        assert_eq!(
            poll(&base, &code).await.unwrap(),
            PollOutcome::RateLimited { retry_after: None }
        );
        assert_eq!(
            poll(&base, &code).await.unwrap(),
            PollOutcome::RateLimited { retry_after: None }
        );
        assert_eq!(
            poll(&base, &code).await.unwrap(),
            PollOutcome::RateLimited {
                retry_after: Some(3)
            }
        );
        assert_eq!(
            PollOutcome::RateLimited { retry_after: None }.status(),
            "rate_limited"
        );
    }

    /// The back-off arithmetic: a 429 grows the interval by the slow-down
    /// step, or to `Retry-After` when that is longer, and never shrinks it;
    /// a `slow_down` takes the server's interval unless the current one is
    /// already longer.
    #[test]
    fn the_interval_only_grows() {
        assert_eq!(interval_after_rate_limit(5, None), 10);
        assert_eq!(interval_after_rate_limit(5, Some(3)), 10);
        assert_eq!(interval_after_rate_limit(5, Some(30)), 30);
        assert_eq!(interval_after_rate_limit(30, Some(12)), 35);
        assert_eq!(interval_after_rate_limit(u64::MAX, Some(1)), u64::MAX);
        assert_eq!(interval_after_slow_down(5, 10), 10);
        assert_eq!(interval_after_slow_down(15, 10), 15);
        assert_eq!(interval_after_slow_down(5, 5), 5);
    }

    /// AC3 — the device code appears in no `Debug` output: not its own, not
    /// the pairing's, and the delivered tokens are counted, not printed.
    #[test]
    fn debug_output_carries_no_secret() {
        let code = DeviceCode::new("dc-SECRET-0123456789");
        assert_eq!(format!("{code:?}"), "DeviceCode(<redacted>)");

        let started = Started {
            device_code: code,
            user_code: "BCDF-GHJK".into(),
            verification_uri: "https://example.invalid/pair".into(),
            expires_in: 900,
            interval: 5,
        };
        let text = format!("{started:?}");
        assert!(!text.contains("SECRET"), "{text}");
        assert!(text.contains("BCDF-GHJK"), "{text}");
        let text = format!("{:?}", StartOutcome::Started(started));
        assert!(!text.contains("SECRET"), "{text}");

        let approved = Approved {
            tokens: vec!["eyJ.TOKEN.sig".into(), "eyJ.TOKEN2.sig".into()],
            org_name: "Acme".into(),
            org_slug: Some("acme".into()),
            approver_name: "Dana".into(),
            approver_username: Some("dana".into()),
        };
        let text = format!("{approved:?}");
        assert!(!text.contains("TOKEN"), "{text}");
        assert!(text.contains("<2 redacted>"), "{text}");
        assert!(text.contains("Acme") && text.contains("Dana"), "{text}");
        assert!(text.contains("acme") && text.contains("dana"), "{text}");
        let text = format!("{:?}", PollOutcome::Approved(approved));
        assert!(!text.contains("TOKEN"), "{text}");
    }

    #[test]
    fn role_wire_spellings() {
        assert_eq!(
            serde_json::to_string(&PairRole::Daemon).unwrap(),
            "\"daemon\""
        );
        assert_eq!(
            serde_json::to_string(&PairRole::Tunnel).unwrap(),
            "\"tunnel\""
        );
        assert_eq!(PairRole::Daemon.as_str(), "daemon");
        assert_eq!(PairRole::Tunnel.as_str(), "tunnel");
    }

    #[test]
    fn host_facts_are_the_wire_spellings() {
        let os = serde_json::to_string(&host_os()).unwrap();
        assert!(
            matches!(os.as_str(), "\"linux\"" | "\"macos\"" | "\"windows\""),
            "{os}"
        );
        let arch = host_arch();
        assert!(!arch.is_empty());
        assert_eq!(arch, std::env::consts::ARCH);
    }

    #[test]
    fn poll_outcome_status_spellings() {
        assert_eq!(
            PollOutcome::AuthorizationPending.status(),
            "authorization_pending"
        );
        assert_eq!(PollOutcome::SlowDown { interval: 1 }.status(), "slow_down");
        assert_eq!(
            PollOutcome::RateLimited { retry_after: None }.status(),
            "rate_limited"
        );
        assert_eq!(PollOutcome::AccessDenied.status(), "access_denied");
        assert_eq!(PollOutcome::ExpiredToken.status(), "expired_token");
        assert_eq!(PollOutcome::Unknown("x".into()).status(), "x");
    }
}
