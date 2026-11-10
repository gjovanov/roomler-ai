// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! #2187 — the control WS and DERP dial through `tunnel_core::ctl_dns`.
//!
//! `connect_async(url)` resolves the URL's host on every attempt through the OS
//! resolver, and with an exit's DNS steer in force that means through the exit.
//! An exit whose DNS stops answering while its carrier stays up is never torn
//! down, so a control WS that dropped could not reconnect at all: every attempt
//! waited out the resolver and failed, and the device stayed offline for as long
//! as the exit's DNS did.
//!
//! [`connect`] dials the remembered address instead and hands the URL itself to
//! the handshake, so TLS still verifies the server's NAME (SNI and certificate)
//! and the `Host` header is unchanged. Only where the address comes from moves.
//!
//! #3656 — and [`dial`] is how the daemon's long-lived credentials travel: the
//! agent JWT on `/ws` and the control plane's `/derp`, and a regional relay's
//! admission ticket. Each rides `Authorization: Bearer`, and the URL names the
//! endpoint, the role and the tenant only, because a request line is what
//! access logs and inspecting proxies record. A server or relay older than the
//! header gets one redial in the query form, and that endpoint keeps the query
//! form for [`upgrade_auth::QUERY_FORM_FOR`]; a verdict on the credential
//! itself never sends it into a URL.

use std::net::IpAddr;
use std::time::Duration;

use roomler_ai_remote_control::upgrade_auth::{self, Carrier};
use tokio::net::TcpStream;
use tokio::time::error::Elapsed;
use tokio_tungstenite::tungstenite;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::handshake::client::{Request, Response};
use tokio_tungstenite::tungstenite::http::{HeaderValue, Uri, header};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, client_async_tls_with_config, connect_async,
};
use tracing::{debug, info};

pub type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// One address's TCP connect; the caller's own timeout bounds the whole dial.
const PER_ADDR_CONNECT: Duration = Duration::from_secs(10);

/// FR-28 — the flat redials one outage may spend on one socket before the ladder
/// takes over. This spends 15 to 30 s of waiting, and the slowest of the 14 rolls
/// read had a median socket back at 26 s. A 502 that outlasts them is not a roll.
const FLAT_REDIALS_PER_OUTAGE: u32 = 30;

/// FR-28 — the wait before one flat redial, drawn per dial. At most 1 s, so a
/// socket is back within a second of its pod listening; drawn, because a roll
/// ends a whole fleet's sockets in the same millisecond.
const FLAT_REDIAL_WAIT_MS: std::ops::RangeInclusive<u64> = 500..=1000;

/// FR-28 — redial at a flat pace while the front load balancer answers 502.
///
/// The front pins a tenant to one pod with no failover (`max_fails=0`,
/// `proxy_next_upstream off`), so through a roll it answers 502 until the
/// replacement listens, and a socket on the ladder then waits out whichever step
/// comes next: the medians of 14 rolls sat on 7.5, 15 and 26 s.
///
/// ⚠️ #2221's rule binds: a server that accepts and drops must climb, or a fleet
/// reconnects to it every second. So only a 502 at the upgrade is flat, and an
/// accepted socket that does not last refills nothing: [`Self::connection_ended`]
/// keeps #2221's bar.
///
/// ⚠️ A 502 can also come from a corporate proxy, for hours. The allowance is what
/// bounds that: one outage costs one socket at most [`FLAT_REDIALS_PER_OUTAGE`]
/// extra dials, and then the ordinary ladder runs from the step it had reached.
///
/// A flat redial never moves the ladder, in either direction. [`Self::off`] spends
/// nothing and always answers `None`, so a loop that holds it runs exactly as it
/// did before this existed.
#[derive(Clone, Debug)]
pub struct FlatRedial {
    allowance: u32,
    left: u32,
    proven_after: Duration,
}

impl FlatRedial {
    /// The ladder, untouched: the default, and every regional relay.
    pub fn off() -> Self {
        Self::with_limits(0, crate::signaling::LADDER_PROVEN_AFTER)
    }

    pub fn on() -> Self {
        Self::with_limits(
            FLAT_REDIALS_PER_OUTAGE,
            crate::signaling::LADDER_PROVEN_AFTER,
        )
    }

    /// The switch: `ws_flat_redial` in the config, or `ROOMLERD_WS_FLAT_REDIAL`,
    /// read as every opt-in gate is: `1`, `true`, `yes` or `on`, and anything
    /// else, a typo included, leaves the ladder.
    pub fn from_config() -> Self {
        if tunnel_core::env::flag("WS_FLAT_REDIAL", false) {
            Self::on()
        } else {
            Self::off()
        }
    }

    /// A test's own allowance and bar, since the real bar is 30 s of waiting.
    #[doc(hidden)]
    pub fn with_limits(allowance: u32, proven_after: Duration) -> Self {
        Self {
            allowance,
            left: allowance,
            proven_after,
        }
    }

    /// The wait in place of the ladder's after a failed dial, spending one redial.
    /// `None` means the ladder decides: not a 502, or nothing left to spend.
    pub fn step(&mut self, lb_502: bool) -> Option<Duration> {
        if !lb_502 || self.left == 0 {
            return None;
        }
        self.left -= 1;
        Some(Duration::from_millis(rand::random_range(
            FLAT_REDIAL_WAIT_MS,
        )))
    }

    /// Flat redials this outage has left, for the log line.
    pub fn left(&self) -> u32 {
        self.left
    }

    /// An accepted connection ended after `lasted`. Only one past #2221's bar
    /// earns the next outage its redials back.
    pub fn connection_ended(&mut self, lasted: Duration) {
        if lasted >= self.proven_after {
            self.restart();
        }
    }

    /// The network moved, so the outage this allowance belonged to is over.
    pub fn restart(&mut self) {
        self.left = self.allowance;
    }
}

/// FR-28 — the answer [`FlatRedial`] is about: the load balancer has no process
/// behind this tenant's pin. Our own routes never answer 502 on these upgrades.
pub fn is_lb_502(e: &tungstenite::Error) -> bool {
    matches!(e, tungstenite::Error::Http(resp) if resp.status().as_u16() == 502)
}

/// #3656 — the long-lived credential one dial presents, and the name the query
/// form gives it. `Debug` never shows the value.
#[derive(Clone, PartialEq, Eq)]
pub struct Credential {
    value: String,
    query_name: &'static str,
}

impl Credential {
    /// The agent JWT, on `/ws` and the control plane's `/derp`: `token=` in
    /// the query form.
    pub fn token(value: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            query_name: "token",
        }
    }

    /// A regional relay's admission ticket: `ticket=` in the query form.
    pub fn ticket(value: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            query_name: "ticket",
        }
    }
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Credential({}=<redacted>)", self.query_name)
    }
}

/// #3656 — one native dial: a URL that names the endpoint, the role and the
/// tenant, and the credential that goes with it, which the URL never holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dial {
    pub url: String,
    pub credential: Credential,
}

/// #3656 — the upgrade request for `dial` with its credential in `carrier`:
/// an `Authorization: Bearer` header whose value is marked sensitive, or the
/// query form a server or relay older than the header reads. Never both.
pub fn request(dial: &Dial, carrier: Carrier) -> Result<Request, tungstenite::Error> {
    let Credential { value, query_name } = &dial.credential;
    match carrier {
        Carrier::Header => {
            let mut request = dial.url.as_str().into_client_request()?;
            let mut bearer = HeaderValue::from_str(&format!("Bearer {value}"))?;
            bearer.set_sensitive(true);
            request.headers_mut().insert(header::AUTHORIZATION, bearer);
            Ok(request)
        }
        Carrier::Query => {
            let sep = if dial.url.contains('?') { '&' } else { '?' };
            let value = crate::signaling::urlencode(value);
            format!("{}{sep}{query_name}={value}", dial.url).into_client_request()
        }
    }
}

/// #3656 — dial with the credential in a header, each attempt bounded by
/// `per_dial`, and hand back the last answer.
///
/// A header-only dial refused with no verdict on the credential (the bare 401
/// or 400 of a server or relay older than the header, or our own challenge
/// that saw no credential because something on the way dropped the header)
/// gets ONE redial in the query form. If that one upgrades, the endpoint is
/// noted and its dials take the query form for [`upgrade_auth::QUERY_FORM_FOR`].
/// A refusal that names an error is final: dialling again would only put a
/// refused credential into a URL to hear the same answer.
pub async fn dial(
    dial: &Dial,
    per_dial: Duration,
) -> Result<Result<(Ws, Response), tungstenite::Error>, Elapsed> {
    let notes = upgrade_auth::query_form_notes();
    let endpoint = upgrade_auth::endpoint_of(&dial.url);
    let first = notes.carrier_for(endpoint);
    let answer = dial_with(dial, first, per_dial).await;
    if first == Carrier::Query || !asks_for_query_form(&answer) {
        return answer;
    }
    debug!(
        %endpoint,
        "upgrade refused with no verdict on the credential (a server or relay older than \
         the header, or a header dropped on the way); dialling once more with it in the query"
    );
    let again = dial_with(dial, Carrier::Query, per_dial).await;
    if matches!(again, Ok(Ok(_))) {
        notes.note_query_form(endpoint);
        info!(
            %endpoint,
            "this endpoint took the credential only in the query: its dials carry it there \
             for the next 6 h (#3656)"
        );
    }
    again
}

async fn dial_with(
    dial: &Dial,
    carrier: Carrier,
    per_dial: Duration,
) -> Result<Result<(Ws, Response), tungstenite::Error>, Elapsed> {
    match request(dial, carrier) {
        Ok(request) => tokio::time::timeout(per_dial, connect(request)).await,
        Err(e) => Ok(Err(e)),
    }
}

/// Whether a dial's answer is a refusal [`upgrade_auth::asks_for_query_form`]
/// reads as a request for the query form.
fn asks_for_query_form(
    answer: &Result<Result<(Ws, Response), tungstenite::Error>, Elapsed>,
) -> bool {
    let Ok(Err(tungstenite::Error::Http(refused))) = answer else {
        return false;
    };
    let challenges = refused.headers().get_all(header::WWW_AUTHENTICATE);
    upgrade_auth::asks_for_query_form(
        refused.status().as_u16(),
        challenges.iter().filter_map(|v| v.to_str().ok()),
    )
}

/// `connect_async`, with the host's address from `ctl_dns`. An address literal,
/// or a name nothing knows, takes `connect_async`'s own path unchanged.
pub async fn connect(request: Request) -> Result<(Ws, Response), tungstenite::Error> {
    let Some((host, port)) = host_port(request.uri()) else {
        return connect_async(request).await;
    };
    if host.trim_matches(['[', ']']).parse::<IpAddr>().is_ok() {
        return connect_async(request).await;
    }
    let hostport = tunnel_core::ctl_dns::hostport(&host, port);
    let addrs = tunnel_core::ctl_dns::resolve(&hostport).await;
    if addrs.is_empty() {
        return connect_async(request).await;
    }
    let mut last = None;
    for addr in &addrs {
        match tokio::time::timeout(PER_ADDR_CONNECT, TcpStream::connect(addr)).await {
            Ok(Ok(stream)) => {
                let done = client_async_tls_with_config(request, stream, None, None).await;
                // An HTTP answer (a 401, say) proves the address; anything else
                // may be a server that moved, so look again behind the caller.
                if let Err(e) = &done
                    && !matches!(e, tungstenite::Error::Http(_))
                {
                    tunnel_core::ctl_dns::distrust(&hostport);
                }
                return done;
            }
            Ok(Err(e)) => last = Some(e),
            Err(_) => {
                last = Some(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!("connect to {addr} timed out"),
                ))
            }
        }
    }
    // Every remembered address failed: the next attempt gets a fresh look.
    tunnel_core::ctl_dns::distrust(&hostport);
    Err(tungstenite::Error::Io(last.unwrap_or_else(|| {
        std::io::Error::other("no address to dial")
    })))
}

/// Host and port of a `ws(s)://` URL, the scheme's default port when it has none.
fn host_port(uri: &Uri) -> Option<(String, u16)> {
    let host = uri.host()?.to_string();
    let port = uri.port_u16().unwrap_or(match uri.scheme_str() {
        Some("wss") | Some("https") => 443,
        _ => 80,
    });
    Some((host, port))
}

/// #3656 — a loopback front for the dial tests. It records what each upgrade
/// presented and answers it as the test says, so one test can stand in for a
/// server or relay older than the header, a newer one, or one in between.
#[cfg(test)]
pub(crate) mod test_front {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use futures::StreamExt;
    use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
    use tokio_tungstenite::tungstenite::http;

    /// What one dial presented.
    #[derive(Clone, Debug)]
    pub(crate) struct Seen {
        /// The request target, path and query.
        pub(crate) uri: String,
        /// The `Authorization` header, if any.
        pub(crate) authorization: Option<String>,
    }

    /// How the front answers one dial.
    #[derive(Clone, Copy, Debug)]
    pub(crate) enum Verdict {
        Accept,
        /// This status, with this `WWW-Authenticate` if any.
        Refuse(u16, Option<&'static str>),
    }

    pub(crate) type Log = Arc<Mutex<Vec<Seen>>>;

    /// Serve on loopback for the rest of the test: `ws://127.0.0.1:<port>`,
    /// and the log of what each dial presented, in order.
    pub(crate) async fn start(
        decide: impl Fn(&Seen) -> Verdict + Send + Sync + 'static,
    ) -> (String, Log) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("ws://{}", listener.local_addr().unwrap());
        let log: Log = Arc::default();
        let decide = Arc::new(decide);
        let seen = log.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let (seen, decide) = (seen.clone(), decide.clone());
                tokio::spawn(async move {
                    // tungstenite's handshake `Callback` fixes the closure's error type.
                    #[allow(clippy::result_large_err)]
                    let answer = |req: &Request,
                                  resp: Response|
                     -> Result<Response, ErrorResponse> {
                        let this = Seen {
                            uri: req.uri().to_string(),
                            authorization: req
                                .headers()
                                .get(http::header::AUTHORIZATION)
                                .and_then(|v| v.to_str().ok())
                                .map(str::to_string),
                        };
                        let verdict = decide(&this);
                        seen.lock().unwrap().push(this);
                        match verdict {
                            Verdict::Accept => Ok(resp),
                            Verdict::Refuse(status, challenge) => {
                                let mut refused = ErrorResponse::new(None);
                                *refused.status_mut() = http::StatusCode::from_u16(status).unwrap();
                                if let Some(c) = challenge {
                                    refused.headers_mut().insert(
                                        http::header::WWW_AUTHENTICATE,
                                        http::HeaderValue::from_static(c),
                                    );
                                }
                                Err(refused)
                            }
                        }
                    };
                    if let Ok(mut ws) = tokio_tungstenite::accept_hdr_async(stream, answer).await {
                        while let Some(Ok(_)) = ws.next().await {}
                    }
                });
            }
        });
        (base, log)
    }

    /// Every dial the front has seen so far.
    pub(crate) fn seen(log: &Log) -> Vec<Seen> {
        log.lock().unwrap().clone()
    }

    /// The dials the front has seen, once it has seen at least `n`.
    pub(crate) async fn until_seen(log: &Log, n: usize) -> Vec<Seen> {
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let now = seen(log);
                if now.len() >= n {
                    return now;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("the front saw {:?}, wanted {n} dial(s)", seen(log)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    /// FR-28 — only a 502 spends a redial, and the wait stays inside the bound
    /// that puts a socket back within a second of its pod listening.
    #[test]
    fn a_502_gets_a_wait_of_at_most_a_second_and_nothing_else_does() {
        let mut flat = FlatRedial::on();
        for _ in 0..FLAT_REDIALS_PER_OUTAGE {
            assert_eq!(flat.step(false), None);
            let wait = flat.step(true).expect("the allowance is not spent");
            assert!(
                wait >= Duration::from_millis(500) && wait <= Duration::from_secs(1),
                "{wait:?}"
            );
        }
    }

    /// FR-28 — the allowance is the bound on a 502 that never ends.
    #[test]
    fn the_allowance_runs_out_and_then_the_ladder_decides() {
        let mut flat = FlatRedial::with_limits(2, Duration::from_secs(30));
        assert!(flat.step(true).is_some());
        assert_eq!(flat.left(), 1);
        assert!(flat.step(true).is_some());
        assert_eq!(flat.left(), 0);
        assert_eq!(flat.step(true), None);
        assert_eq!(flat.left(), 0, "nothing below zero");
    }

    /// FR-28 and #2221 — a connection refills the allowance only past the bar:
    /// a server that accepts and drops at once must never earn more flat redials.
    #[test]
    fn only_a_connection_that_lived_refills_the_allowance() {
        let bar = Duration::from_secs(30);
        let mut flat = FlatRedial::with_limits(1, bar);
        assert!(flat.step(true).is_some());
        flat.connection_ended(bar - Duration::from_millis(1));
        assert_eq!(flat.step(true), None, "under the bar refills nothing");
        flat.connection_ended(bar);
        assert!(flat.step(true).is_some(), "at the bar it does");
    }

    /// FR-28 — off is off whatever happens to it: this is what makes a default
    /// build run the ladder it always ran.
    #[test]
    fn off_never_answers_a_502_however_it_is_restarted() {
        let mut flat = FlatRedial::off();
        assert_eq!(flat.step(true), None);
        flat.restart();
        flat.connection_ended(Duration::from_secs(3600));
        assert_eq!(flat.step(true), None);
    }

    fn http_answer(status: u16) -> tungstenite::Error {
        tungstenite::Error::Http(Box::new(
            tungstenite::http::Response::builder()
                .status(status)
                .body(None)
                .unwrap(),
        ))
    }

    /// FR-28 — only the load balancer's own answer. `/derp` answers 401 for any
    /// lookup error, a Mongo blip is a 500, and an unmounted module is a 503.
    #[test]
    fn only_a_502_reads_as_the_load_balancer_having_no_pod() {
        assert!(is_lb_502(&http_answer(502)));
        for other in [200, 401, 403, 404, 429, 500, 503, 504] {
            assert!(!is_lb_502(&http_answer(other)), "{other}");
        }
        assert!(!is_lb_502(&tungstenite::Error::ConnectionClosed));
    }

    #[test]
    fn host_port_reads_the_url() {
        let of = |url: &str| host_port(&url.parse().unwrap());
        assert_eq!(
            of("wss://roomler.ai/ws?role=agent&tid=y"),
            Some(("roomler.ai".into(), 443))
        );
        assert_eq!(
            of("ws://relay.example:8080/derp"),
            Some(("relay.example".into(), 8080))
        );
        assert_eq!(of("/ws?role=agent"), None, "no host");
    }

    /// #2187 — the field case in one process: a name the resolver can never
    /// answer (`.invalid`, RFC 2606), remembered from before. The dial goes to
    /// the remembered address and the handshake still carries the NAME, which
    /// the server sees in its `Host` header.
    #[tokio::test]
    // tungstenite's handshake `Callback` fixes the closure's error type.
    #[allow(clippy::result_large_err)]
    async fn a_remembered_name_connects_when_the_resolver_cannot_answer() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let local = listener.local_addr().unwrap();
        let host_seen = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let seen = std::sync::Arc::clone(&host_seen);
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let cb = |req: &tokio_tungstenite::tungstenite::handshake::server::Request,
                      resp: tokio_tungstenite::tungstenite::handshake::server::Response| {
                if let Some(h) = req.headers().get("host") {
                    *seen.lock().unwrap() = h.to_str().unwrap_or("").to_string();
                }
                Ok(resp)
            };
            let mut ws = tokio_tungstenite::accept_hdr_async(stream, cb)
                .await
                .unwrap();
            ws.send(Message::Text("hello".into())).await.unwrap();
        });

        let hostport = format!("ctl-dns-2187.invalid:{}", local.port());
        tunnel_core::ctl_dns::remember(&hostport, &[local]);
        let url = format!("ws://{hostport}/ws");
        let (mut ws, _) = tokio::time::timeout(
            Duration::from_secs(10),
            connect(url.into_client_request().unwrap()),
        )
        .await
        .expect("must not wait on the resolver")
        .expect("connects to the remembered address");
        let first = ws.next().await.unwrap().unwrap();
        assert_eq!(first, Message::Text("hello".into()));
        assert_eq!(*host_seen.lock().unwrap(), hostport, "Host keeps the name");
    }

    /// #3656 — the request each carrier makes: the header form holds the
    /// credential in a sensitive header and the URL unchanged, the query form
    /// in the URL alone, encoded as released clients encode it.
    #[test]
    fn each_carrier_puts_the_credential_in_one_place_only() {
        let control = Dial {
            url: "wss://roomler.ai/ws?role=agent&tid=t1".into(),
            credential: Credential::token("a.b/c+d="),
        };
        let header_form = request(&control, Carrier::Header).unwrap();
        assert_eq!(
            header_form.uri().to_string(),
            "wss://roomler.ai/ws?role=agent&tid=t1"
        );
        let bearer = header_form.headers().get(header::AUTHORIZATION).unwrap();
        assert_eq!(bearer, "Bearer a.b/c+d=");
        assert!(bearer.is_sensitive());
        assert!(
            !format!("{header_form:?}").contains("a.b/c"),
            "{header_form:?}"
        );

        let query_form = request(&control, Carrier::Query).unwrap();
        assert_eq!(
            query_form.uri().to_string(),
            "wss://roomler.ai/ws?role=agent&tid=t1&token=a.b%2Fc%2Bd%3D"
        );
        assert!(query_form.headers().get(header::AUTHORIZATION).is_none());

        let regional = Dial {
            url: "wss://relay.example/derp".into(),
            credential: Credential::ticket("the-ticket"),
        };
        assert_eq!(
            request(&regional, Carrier::Query)
                .unwrap()
                .uri()
                .to_string(),
            "wss://relay.example/derp?ticket=the-ticket"
        );
        assert!(!format!("{control:?}{regional:?}").contains("a.b/c"));
        assert!(!format!("{regional:?}").contains("the-ticket"));
    }

    /// Long enough for a loopback upgrade on a loaded CI runner.
    const PER_DIAL: Duration = Duration::from_secs(20);

    fn control_dial(base: &str) -> Dial {
        Dial {
            url: format!("{base}/ws?role=agent&tid=t1"),
            credential: Credential::token("tok/en"),
        }
    }

    /// The status a refused dial ended on.
    fn refused_with(answer: Result<Result<(Ws, Response), tungstenite::Error>, Elapsed>) -> u16 {
        match answer {
            Ok(Err(tungstenite::Error::Http(refused))) => refused.status().as_u16(),
            Ok(Err(e)) => panic!("refused some other way: {e}"),
            Ok(Ok(_)) => panic!("upgraded"),
            Err(_) => panic!("timed out"),
        }
    }

    /// #3656 — a server older than the header answers a bare 401, and one
    /// behind a middlebox that dropped the header answers our own challenge
    /// with no error. Either gets ONE redial in the query form, with no header,
    /// and the endpoint's next dial goes straight to the query form.
    #[tokio::test]
    async fn a_refusal_with_no_verdict_gets_one_query_redial_that_is_remembered() {
        use test_front::Verdict;
        for challenge in [None, Some(upgrade_auth::CHALLENGE)] {
            // One front each: the note is process-wide, keyed by endpoint.
            let (base, log) = test_front::start(move |seen| {
                if seen.uri.contains("token=") {
                    Verdict::Accept
                } else {
                    Verdict::Refuse(401, challenge)
                }
            })
            .await;
            let control = control_dial(&base);
            dial(&control, PER_DIAL)
                .await
                .unwrap()
                .expect("the query form upgrades");
            let seen = test_front::seen(&log);
            assert_eq!(seen.len(), 2, "{challenge:?}: {seen:?}");
            assert_eq!(seen[0].authorization.as_deref(), Some("Bearer tok/en"));
            assert_eq!(seen[0].uri, "/ws?role=agent&tid=t1");
            assert_eq!(seen[1].authorization, None, "never both: {seen:?}");
            assert_eq!(seen[1].uri, "/ws?role=agent&tid=t1&token=tok%2Fen");

            dial(&control, PER_DIAL)
                .await
                .unwrap()
                .expect("the noted endpoint upgrades");
            let seen = test_front::seen(&log);
            assert_eq!(seen.len(), 3, "one dial, in the query form: {seen:?}");
            assert_eq!(seen[2].authorization, None);
            assert!(seen[2].uri.ends_with("&token=tok%2Fen"), "{seen:?}");
        }
    }

    /// #3656 — a relay older than the header answers a bare 400, and the
    /// redial names the ticket as a released daemon does.
    #[tokio::test]
    async fn a_relay_older_than_the_header_gets_its_ticket_in_the_query() {
        use test_front::Verdict;
        let (base, log) = test_front::start(|seen| {
            if seen.uri.contains("ticket=") {
                Verdict::Accept
            } else {
                Verdict::Refuse(400, None)
            }
        })
        .await;
        let regional = Dial {
            url: format!("{base}/derp?tid=t1"),
            credential: Credential::ticket("the-ticket"),
        };
        dial(&regional, PER_DIAL)
            .await
            .unwrap()
            .expect("the query form upgrades");
        let seen = test_front::seen(&log);
        assert_eq!(seen.len(), 2, "{seen:?}");
        assert_eq!(seen[0].authorization.as_deref(), Some("Bearer the-ticket"));
        assert_eq!(seen[0].uri, "/derp?tid=t1");
        assert_eq!(seen[1].authorization, None);
        assert_eq!(seen[1].uri, "/derp?tid=t1&ticket=the-ticket");
    }

    /// #3656 — a verdict on the credential is final: one dial, and the
    /// credential never reaches a URL. Nor does any answer that is not about
    /// the credential start a redial.
    #[tokio::test]
    async fn a_verdict_or_another_answer_is_final() {
        use test_front::Verdict;
        for (status, challenge) in [
            (401, Some(upgrade_auth::CHALLENGE_REFUSED)),
            (400, Some(upgrade_auth::CHALLENGE_BAD_REQUEST)),
            (403, None),
            (500, None),
            (502, None),
            (503, None),
        ] {
            let (base, log) = test_front::start(move |_| Verdict::Refuse(status, challenge)).await;
            let control = control_dial(&base);
            assert_eq!(refused_with(dial(&control, PER_DIAL).await), status);
            let seen = test_front::seen(&log);
            assert_eq!(seen.len(), 1, "{status}: {seen:?}");
            assert_eq!(seen[0].uri, "/ws?role=agent&tid=t1");
            assert_eq!(seen[0].authorization.as_deref(), Some("Bearer tok/en"));
            assert_eq!(
                upgrade_auth::query_form_notes().carrier_for(&format!("{base}/ws")),
                Carrier::Header
            );
        }
    }

    /// #3656 — a redial refused too notes nothing, and its answer is the one
    /// the dial hands back: a credential that the old server refuses keeps
    /// being offered in the header first.
    #[tokio::test]
    async fn a_redial_refused_too_is_not_remembered() {
        use test_front::Verdict;
        let (base, log) = test_front::start(|_| Verdict::Refuse(401, None)).await;
        let control = control_dial(&base);
        assert_eq!(refused_with(dial(&control, PER_DIAL).await), 401);
        assert_eq!(refused_with(dial(&control, PER_DIAL).await), 401);
        let seen = test_front::seen(&log);
        let carried: Vec<bool> = seen.iter().map(|s| s.authorization.is_some()).collect();
        assert_eq!(carried, [true, false, true, false], "{seen:?}");
    }
}
