// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! #3432 — the one failure an install ends with: the step it failed at, what
//! kind of failure it was, words a person can act on, and every cause under
//! them.
//!
//! [`Failure::build`] is the only place the words are made, from the error
//! itself, so no error site formats its cause into a string the person then
//! reads without the cause. The kind is decided by walking the error's causes
//! and asking each for its TYPE, never by reading its text:
//!
//! | kind | decided on |
//! |---|---|
//! | `cancelled` | the operator's cancel flag, when the step failed |
//! | `refused` | a [`Refusal`] in the chain, or words with no cause ([`Said`]) at its root |
//! | `name_not_resolved` | [`NameNotResolved`], from the wizard's own resolver |
//! | `secure_connection_failed` | a `native_tls::Error`: the secure connection could not be set up. An untrusted certificate is the usual cause and a handshake the network cut another, so the words name the first as the usual one |
//! | `status` | a server's answer: [`HttpStatus`], [`EnrollRejected`], `reqwest::Error::status()` |
//! | `connection_refused` | an `io::Error` of kind `ConnectionRefused` under a request |
//! | `timed_out` | `reqwest::Error::is_timeout()`, or an `io::Error` of kind `TimedOut` under a request |
//! | `bad_answer` | `reqwest::Error::is_decode()`: something answered, not with what was asked for |
//! | `unreachable` | any other request error (a reset, an unreachable network, a proxy's refused tunnel) |
//! | `local_file` | an `io::Error` no request raised |
//! | `other` | none of these |
//!
//! A site adds what it was doing with `anyhow::Context`, and may add a
//! sentence of advice only it knows ([`Failure::advise`]); it never replaces
//! the words or the chain.

use std::fmt;

use roomler_ai_remote_control::setup_diagnostics::ErrorCode;
use roomlerd::enrollment::EnrollRejected;
use serde::Serialize;
use tauri::ipc::Channel;
use wizard_shared::http::{HttpStatus, NameNotResolved};
use wizard_shared::progress::{ProgressEvent, replay_log};

use crate::diagnostics::Diagnostics;
use crate::package_step::Refusal;
use crate::steps::StepId;

/// What kind of failure it was, for the page and for #3509's retry rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Cancelled,
    Refused,
    NameNotResolved,
    SecureConnectionFailed,
    ConnectionRefused,
    TimedOut,
    Status,
    BadAnswer,
    Unreachable,
    LocalFile,
    Other,
}

impl Kind {
    /// Its name on the wire.
    pub fn wire(self) -> &'static str {
        match self {
            Kind::Cancelled => "cancelled",
            Kind::Refused => "refused",
            Kind::NameNotResolved => "name_not_resolved",
            Kind::SecureConnectionFailed => "secure_connection_failed",
            Kind::ConnectionRefused => "connection_refused",
            Kind::TimedOut => "timed_out",
            Kind::Status => "status",
            Kind::BadAnswer => "bad_answer",
            Kind::Unreachable => "unreachable",
            Kind::LocalFile => "local_file",
            Kind::Other => "other",
        }
    }

    /// Whether the same request can work a moment later: the network's and
    /// a server's passing failures. A refusal, a certificate and a 4xx stay as
    /// they are until something changes.
    pub fn transient(self, status: Option<u16>) -> bool {
        match self {
            Kind::ConnectionRefused
            | Kind::TimedOut
            | Kind::Unreachable
            | Kind::NameNotResolved => true,
            Kind::Status => status.is_some_and(|s| s >= 500 || s == 429),
            _ => false,
        }
    }
}

/// Words a site composed itself, with no error under them: a refusal the
/// wizard decided on, such as a role this run cannot install. The only way
/// words without a cause become a [`Failure`]: a failure that has a cause
/// keeps it, with `.context(…)` on the error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Said(pub String);

impl fmt::Display for Said {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Said {}

/// Why an install stopped.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Failure {
    /// The step that was running, from the tracker.
    pub step: StepId,
    pub kind: Kind,
    /// The server's status, for [`Kind::Status`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    /// Its code in the opt-in diagnostics.
    #[serde(skip)]
    pub code: ErrorCode,
    /// What failed, why, and what to try.
    pub words: String,
    /// Every cause, outermost first.
    pub chain: Vec<String>,
}

impl Failure {
    /// The one builder: the kind, the words and the chain, from the error.
    pub fn build(step: StepId, code: ErrorCode, cancelled: bool, err: &anyhow::Error) -> Failure {
        let found = Found::of(err, cancelled);
        let words = found.words(step, err);
        Failure {
            step,
            kind: found.kind,
            status: found.status,
            code,
            words,
            chain: err.chain().map(|c| c.to_string()).collect(),
        }
    }

    /// Add a sentence only the site knows to say, after the words (nothing,
    /// for `None`).
    pub fn advise<'a>(mut self, advice: impl Into<Option<&'a str>>) -> Failure {
        if let Some(advice) = advice.into() {
            self.words.push(' ');
            self.words.push_str(advice);
        }
        self
    }

    /// The terminal event the page draws.
    pub fn event(&self) -> ProgressEvent {
        ProgressEvent::Error {
            step: self.step.wire().to_string(),
            kind: self.kind.wire().to_string(),
            status: self.status,
            words: self.words.clone(),
            chain: self.chain.clone(),
        }
    }

    /// The terminal event, into the replay log, and the same words in the
    /// wizard's log; handed back for the page's channel.
    pub fn report(&self) -> ProgressEvent {
        let event = self.event();
        replay_log().push(event.clone());
        self.log();
        event
    }

    /// The words, the step, the kind and the chain, in the wizard's log.
    pub fn log(&self) {
        tracing::error!(
            step = self.step.wire(),
            kind = self.kind.wire(),
            status = self.status,
            chain = ?self.chain,
            "{}",
            self.words
        );
    }
}

/// The end of a run that failed, the same for every pipeline: the event to
/// the page and the replay log, and the words to the wizard's log.
pub fn conclude(on_event: &Channel<ProgressEvent>, failure: &Failure) {
    // Every step a failure ends is closed by the tracker; this closes one a
    // site might have left open, so the run's last step is never unrecorded.
    Diagnostics::global().abandon(failure.code, failure.kind == Kind::Cancelled);
    let _ = on_event.send(failure.report());
}

/// What the walk over an error's causes found.
struct Found {
    kind: Kind,
    status: Option<u16>,
    /// The server, or the name, the failure is about.
    host: Option<String>,
    /// A refusal's own words, or what a server said with its status.
    said: Option<String>,
}

/// The first cause in `err` of type `T`.
fn find<T: std::error::Error + 'static>(err: &anyhow::Error) -> Option<&T> {
    err.chain().find_map(|c| c.downcast_ref::<T>())
}

/// Whether a cause in `err` is an `io::Error` of `kind`.
fn has_io(err: &anyhow::Error, kind: std::io::ErrorKind) -> bool {
    err.chain()
        .filter_map(|c| c.downcast_ref::<std::io::Error>())
        .any(|e| e.kind() == kind)
}

/// The server a request in `err` was sent to.
fn host_of(err: &anyhow::Error) -> Option<String> {
    err.chain().find_map(|c| {
        let url = if let Some(r) = c.downcast_ref::<reqwest::Error>() {
            r.url().cloned()
        } else if let Some(s) = c.downcast_ref::<HttpStatus>() {
            reqwest::Url::parse(&s.url).ok()
        } else {
            None
        };
        url.and_then(|u| u.host_str().map(str::to_string))
    })
}

impl Found {
    fn of(err: &anyhow::Error, cancelled: bool) -> Found {
        // A refusal's own words, which a cancel that has some keeps too.
        let refused = find::<Refusal>(err)
            .map(Refusal::words)
            .or_else(|| err.root_cause().downcast_ref::<Said>().map(|s| s.0.clone()));
        let mut found = Found {
            kind: Kind::Other,
            status: None,
            host: host_of(err),
            said: refused.clone(),
        };
        found.kind = if cancelled {
            Kind::Cancelled
        } else if refused.is_some() {
            Kind::Refused
        } else if let Some(n) = find::<NameNotResolved>(err) {
            found.host = Some(n.name.clone());
            Kind::NameNotResolved
        } else if find::<native_tls::Error>(err).is_some() {
            Kind::SecureConnectionFailed
        } else if let Some(s) = find::<HttpStatus>(err) {
            found.status = Some(s.status);
            found.said = Some(s.said.clone());
            Kind::Status
        } else if let Some(r) = find::<EnrollRejected>(err) {
            found.status = Some(r.status.as_u16());
            found.said = Some(wizard_shared::http::said_of(&r.body));
            Kind::Status
        } else if let Some(r) = find::<reqwest::Error>(err) {
            if has_io(err, std::io::ErrorKind::ConnectionRefused) {
                Kind::ConnectionRefused
            } else if r.is_timeout() || has_io(err, std::io::ErrorKind::TimedOut) {
                Kind::TimedOut
            } else if let Some(s) = r.status() {
                found.status = Some(s.as_u16());
                Kind::Status
            } else if r.is_decode() {
                Kind::BadAnswer
            } else {
                Kind::Unreachable
            }
        } else if find::<std::io::Error>(err).is_some() {
            Kind::LocalFile
        } else {
            Kind::Other
        };
        found
    }

    fn words(&self, step: StepId, err: &anyhow::Error) -> String {
        let host = self.host.as_deref().unwrap_or("the server");
        let doing = step.doing();
        match self.kind {
            Kind::Cancelled => self
                .said
                .clone()
                .unwrap_or_else(|| "The install was cancelled. Retry starts it again.".to_string()),
            Kind::Refused => self.said.clone().unwrap_or_else(|| because(err)),
            Kind::NameNotResolved => format!(
                "Roomler Setup could not find {host}: the name did not resolve. Check the \
                 server's address and this computer's network or DNS, then Retry."
            ),
            Kind::SecureConnectionFailed => format!(
                "Roomler Setup could not set up a secure connection to {host}. The usual cause \
                 is a certificate this computer does not trust, as on a network that inspects \
                 secure connections (a proxy or a security product): ask whoever runs it to let \
                 {host} through, then Retry."
            ),
            Kind::ConnectionRefused => format!(
                "Roomler Setup could not reach {host}: the connection was refused. Check this \
                 computer's network or proxy, then Retry."
            ),
            Kind::TimedOut => format!(
                "Roomler Setup could not reach {host}: it did not answer in time. Check this \
                 computer's network or proxy, then Retry."
            ),
            Kind::Unreachable => format!(
                "Roomler Setup could not reach {host}: {}. Check this computer's network or \
                 proxy, then Retry.",
                clause(&err.root_cause().to_string())
            ),
            Kind::BadAnswer => format!(
                "Roomler Setup could not {doing}: {host} answered, but not with what Roomler \
                 Setup asked for, as a proxy's or a security product's block page does. Check \
                 this computer's network or proxy, then Retry."
            ),
            Kind::Status => self.status_words(host, doing),
            Kind::LocalFile => {
                let advice = match find::<std::io::Error>(err).map(|e| e.kind()) {
                    Some(std::io::ErrorKind::PermissionDenied) => {
                        "Check that this account may change it, then Retry."
                    }
                    Some(std::io::ErrorKind::StorageFull | std::io::ErrorKind::QuotaExceeded) => {
                        "Free some space on the disk, then Retry."
                    }
                    _ => {
                        "Check that this account may write there and that the disk has room, \
                         then Retry."
                    }
                };
                format!(
                    "Roomler Setup could not {doing}: {}. {advice}",
                    because(err)
                )
            }
            Kind::Other => format!("Roomler Setup could not {doing}: {}.", because(err)),
        }
    }

    fn status_words(&self, host: &str, doing: &str) -> String {
        let status = self.status.unwrap_or_default();
        let answered = match reqwest::StatusCode::from_u16(status)
            .ok()
            .and_then(|s| s.canonical_reason())
        {
            Some(reason) => format!("{status} {reason}"),
            None => status.to_string(),
        };
        let said = self.said.as_deref().filter(|s| !s.is_empty());
        if status >= 500 {
            format!(
                "Roomler Setup could not {doing}: {host} answered {answered}, so it could not \
                 serve this right now. Retry in a few minutes."
            )
        } else if status == 429 {
            format!(
                "Roomler Setup could not {doing}: {host} answered {answered}. Retry in a few \
                 minutes."
            )
        } else if let Some(said) = said {
            format!(
                "Roomler Setup could not {doing}: {host} answered {answered}: {}.",
                clause(said)
            )
        } else {
            format!("Roomler Setup could not {doing}: {host} answered {answered}.")
        }
    }
}

/// The outermost and the innermost cause, which say what was being done and
/// what stopped it; the chain keeps the rest.
fn because(err: &anyhow::Error) -> String {
    let outer = err.to_string();
    let root = err.root_cause().to_string();
    if outer == root {
        clause(&root).to_string()
    } else {
        format!("{}: {}", clause(&outer), clause(&root))
    }
}

/// `s` without the full stop it may end with, to go inside a sentence.
fn clause(s: &str) -> &str {
    s.trim().trim_end_matches('.')
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;

    /// The resolve step's failure against `base`, through the wizard's own
    /// client and the resolve request itself, with the error's root cause.
    async fn resolve_against(base: &str, timeout: Duration) -> (Failure, String) {
        let client = wizard_shared::http::builder()
            .timeout(timeout)
            .build()
            .unwrap();
        let err = wizard_shared::asset_resolver::resolve_on(&client, base, "permachine", "latest")
            .await
            .expect_err("nothing there serves an installer");
        let root = err.root_cause().to_string();
        let failure = Failure::build(StepId::Resolve, ErrorCode::Resolve, false, &err);
        (failure, root)
    }

    /// What each case of the table shows: words of its own, its kind, and the
    /// chain from the request down to the cause.
    fn holds(f: &Failure, kind: Kind, phrase: &str, request: &str, root: &str) {
        // What the case reads (shown with `--nocapture`): the table's evidence.
        println!("{}: {}\n  chain: {:#?}", f.kind.wire(), f.words, f.chain);
        assert!(
            f.words.contains(phrase),
            "the words {:?} do not say {phrase:?}",
            f.words
        );
        assert_eq!(f.kind, kind, "{f:?}");
        assert!(
            f.chain.first().is_some_and(|c| c.starts_with(request)),
            "the chain {:?} does not start at {request:?}",
            f.chain
        );
        assert_eq!(
            f.chain.last().map(String::as_str),
            Some(root),
            "the chain {:?} does not end at the cause",
            f.chain
        );
    }

    #[tokio::test]
    async fn a_refused_connection_says_the_connection_was_refused() {
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let base = format!("http://127.0.0.1:{port}/api/agent/installer");
        let (f, root) = resolve_against(&base, Duration::from_secs(30)).await;
        holds(
            &f,
            Kind::ConnectionRefused,
            "could not reach 127.0.0.1: the connection was refused",
            &format!("GET {base}/"),
            &root,
        );
        assert!(f.chain.len() >= 3, "{:?}", f.chain);
    }

    #[tokio::test]
    async fn a_name_that_does_not_resolve_says_so_and_names_it() {
        let base = "https://roomler-setup-test.invalid/api/agent/installer";
        let (f, root) = resolve_against(base, Duration::from_secs(30)).await;
        holds(
            &f,
            Kind::NameNotResolved,
            "could not find roomler-setup-test.invalid: the name did not resolve",
            &format!("GET {base}/"),
            &root,
        );
        assert!(f.chain.len() >= 3, "{:?}", f.chain);
    }

    #[tokio::test]
    async fn a_certificate_this_computer_does_not_trust_says_so() {
        use tokio_rustls::rustls;
        use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

        let made = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let cert = CertificateDer::from(made.cert.der().to_vec());
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(made.key_pair.serialize_der()));
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    let _ = acceptor.accept(tcp).await;
                });
            }
        });
        let base = format!("https://127.0.0.1:{port}/api/agent/installer");
        let (f, root) = resolve_against(&base, Duration::from_secs(30)).await;
        holds(
            &f,
            Kind::SecureConnectionFailed,
            "could not set up a secure connection to 127.0.0.1.",
            &format!("GET {base}/"),
            &root,
        );
        // The type also covers a handshake the network cut, so the words
        // name the certificate as the usual cause, never as the fact.
        assert!(
            f.words
                .contains("The usual cause is a certificate this computer does not trust"),
            "{}",
            f.words
        );
        assert!(f.chain.len() >= 3, "{:?}", f.chain);
    }

    #[tokio::test]
    async fn a_server_that_never_answers_says_it_did_not_answer_in_time() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((tcp, _)) = listener.accept().await {
                held.push(tcp);
            }
        });
        let base = format!("http://127.0.0.1:{port}/api/agent/installer");
        let (f, root) = resolve_against(&base, Duration::from_millis(300)).await;
        holds(
            &f,
            Kind::TimedOut,
            "could not reach 127.0.0.1: it did not answer in time",
            &format!("GET {base}/"),
            &root,
        );
        assert!(f.chain.len() >= 2, "{:?}", f.chain);
    }

    #[tokio::test]
    async fn a_server_error_says_what_it_answered_and_to_retry_later() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut tcp, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut request = [0u8; 4096];
                    let _ = tcp.read(&mut request).await;
                    let _ = tcp
                        .write_all(
                            b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\
                              Connection: close\r\n\r\n",
                        )
                        .await;
                    let _ = tcp.shutdown().await;
                });
            }
        });
        let base = format!("http://127.0.0.1:{port}/api/agent/installer");
        let (f, root) = resolve_against(&base, Duration::from_secs(30)).await;
        holds(
            &f,
            Kind::Status,
            "127.0.0.1 answered 503 Service Unavailable, so it could not serve this right now. \
             Retry in a few minutes.",
            &format!("GET {base}/"),
            &root,
        );
        assert_eq!(f.status, Some(503));
        assert!(f.kind.transient(f.status));
    }

    #[test]
    fn words_with_no_cause_are_a_refusal_in_those_words() {
        let err = anyhow::Error::new(Said("This role is not installed by this run.".into()));
        let f = Failure::build(StepId::Preflight, ErrorCode::Preflight, false, &err);
        assert_eq!(f.kind, Kind::Refused);
        assert_eq!(f.words, "This role is not installed by this run.");
        assert!(!f.kind.transient(None));
    }

    #[test]
    fn a_refused_enrollment_is_a_status_with_what_the_server_said() {
        let err = anyhow::Error::new(EnrollRejected {
            status: reqwest::StatusCode::UNAUTHORIZED,
            body: "Token expired".into(),
        });
        let f = Failure::build(StepId::Enroll, ErrorCode::EnrollRejected, false, &err);
        assert_eq!((f.kind, f.status), (Kind::Status, Some(401)));
        assert_eq!(
            f.words,
            "Roomler Setup could not enroll this device: the server answered 401 \
             Unauthorized: Token expired."
        );
        assert_eq!(
            f.chain,
            vec!["enrollment rejected (status 401 Unauthorized): Token expired"]
        );
        let f = f.advise("An enrollment token works once.");
        assert!(
            f.words
                .ends_with("Token expired. An enrollment token works once.")
        );
    }

    #[test]
    fn a_local_file_error_says_which_and_what_to_try() {
        let err = anyhow::Error::new(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
            .context("creating staging file C:\\stage\\roomler.msi");
        let f = Failure::build(StepId::Download, ErrorCode::Download, false, &err);
        assert_eq!(f.kind, Kind::LocalFile);
        assert!(
            f.words.starts_with(
                "Roomler Setup could not download the installer: creating staging file \
                 C:\\stage\\roomler.msi: "
            ),
            "{}",
            f.words
        );
        assert!(
            f.words
                .ends_with("Check that this account may change it, then Retry.")
        );
        assert_eq!(f.chain.len(), 2);
    }

    #[test]
    fn a_cancel_reads_as_one_whatever_the_error_says() {
        let err = anyhow::anyhow!("download cancelled").context("download installer");
        let f = Failure::build(StepId::Download, ErrorCode::Download, true, &err);
        assert_eq!(f.kind, Kind::Cancelled);
        assert_eq!(f.words, "The install was cancelled. Retry starts it again.");
        assert_eq!(f.chain, vec!["download installer", "download cancelled"]);
    }

    /// The writer the test's log goes to.
    #[derive(Clone, Default)]
    struct Logged(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Logged {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn the_terminal_event_names_the_step_and_the_log_says_the_same() {
        let (steps, _) = crate::steps::tests::tracker();
        steps.started(&[], None);
        steps.begin(StepId::Preflight);
        steps.ok();
        steps.begin(StepId::Download);
        let err = anyhow::Error::new(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
            .context("creating staging file C:\\stage\\roomler.msi");
        let failure = steps.fail_as(ErrorCode::Download, false, err);

        let logged = Logged::default();
        let writer = logged.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        let event = tracing::subscriber::with_default(subscriber, || failure.report());

        let ProgressEvent::Error {
            step,
            kind,
            words,
            chain,
            ..
        } = &event
        else {
            panic!("not the terminal event: {event:?}");
        };
        assert_eq!(step, "download");
        assert_eq!(kind, "local_file");
        assert_eq!(words, &failure.words);
        assert_eq!(chain, &failure.chain);
        assert!(replay_log().snapshot().contains(&event));
        let logged = String::from_utf8(logged.0.lock().unwrap().clone()).unwrap();
        assert!(logged.contains(&failure.words), "{logged}");
        assert!(
            logged.contains("download") && logged.contains("local_file"),
            "{logged}"
        );
    }
}
