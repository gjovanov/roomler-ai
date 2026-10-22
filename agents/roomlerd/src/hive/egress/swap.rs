// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P3b-3c-1 — TLS terminated for a placeholder's audience, and the
//! placeholder swapped for its value on the way out. The design is the
//! spec's §3e, "P3b, `placeholder` egress, as designed"; where the swap is
//! made is decision 32.
//!
//! | Step | Rule |
//! |---|---|
//! | which connections | one to `host:port` while the run holds a placeholder whose audience it is, as it starts. Every other is carried as it comes, never decrypted |
//! | the upstream | TLS first, verified against this device's roots as `proxy` mode verifies them, as the host the client named, offering HTTP/1.1 alone. One that does not verify is refused before the client is told it is through: nothing is decrypted, and nothing is swapped toward a host nobody proved |
//! | the client's hello | read and not answered. One that names another host (SNI), offers no HTTP/1.1 (ALPN) or is no TLS hello at all is carried as it came, on a new dial |
//! | the client's TLS | under the run's CA, its leaf for the host ([`super::ca`]), offering HTTP/1.1 alone |
//! | a request | for this host and port alone (its `Host`, and an absolute form's authority), else `421`; sent on in origin form, less what is one leg's (hop-by-hop). An upgrade is not carried (`501`) |
//! | the swap | in `Authorization` alone: each placeholder of this audience the run holds, as written, and inside `Basic`'s credentials, decoded and encoded again. Never the URL, another header or the body, where a placeholder goes out as itself (decision 32); nor an AWS signature, which P3b-4 signs again |
//! | the value | read from its lease as each request goes ([`super::Values`]): a lease let go swaps nothing |
//! | the response | streamed back as it came, less what is one leg's |
//! | its end | the client's, or the run's. An upstream that closes is dialed again for the next request |
//!
//! ⚠️ The names must agree: the CONNECT's host, the hello's, each request's
//! `Host`, and the one the upstream proved. Without the `Host` check, a CDN
//! that serves many names on one address would route a request, the value
//! in it, to whichever name the session wrote.
//!
//! ⚠️ The audience is trusted with the value: it is the secret's own,
//! matched exactly. One that gives back what it was sent in `Authorization`
//! hands the value to the session. One that keeps what it is sent anywhere
//! else (a body, another header) cannot, since nothing is swapped there.
//!
//! ⚠️ The value is on the wire inside TLS the proxy verified, and passes
//! through the HTTP stack's buffers, which are not zeroed, as `proxy`
//! mode's does.

use std::convert::Infallible;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use base64::Engine as _;
use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper::client::conn::http1::SendRequest;
use hyper::header::{self, HeaderMap, HeaderValue};
use hyper::http::uri::{PathAndQuery, Scheme};
use hyper::{Method, Request, Response, StatusCode, Uri};
use hyper_util::rt::{TokioIo, TokioTimer};
use roomler_ai_remote_control::vault::canonical_net_host;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::rustls::pki_types::PrivateKeyDer;
use tokio_rustls::rustls::{self, ClientConfig, RootCertStore, ServerConfig};
use tokio_rustls::{TlsAcceptor, TlsConnector};
use tracing::{debug, warn};
use zeroize::Zeroizing;

use super::super::placeholder::{self, Bound, PREFIX, RANDOM_CHARS};
use super::{DIAL_SECS, Front, HANDSHAKE_SECS, Refusal, RunKey, RunState, Seams, lock};
use crate::vault_proxy::http::{Body, BoxError, answer, hop_by_hop};
use crate::vault_proxy::tls;

/// The one protocol either leg speaks: a swap reads the request.
const HTTP11: &[u8] = b"http/1.1";
/// The most a client's hello is read to, every record together.
pub const MAX_HELLO: usize = 64 * 1024;
/// How long a client has to send a request's headers.
const HEADER_SECS: u64 = 30;

/// The upstream's leg, its TLS verified.
type Upstream = tokio_rustls::client::TlsStream<TcpStream>;

/// Where a connection goes: its host in its one spelling, its port, and how
/// a log line and an answer name them.
pub(super) struct Target {
    pub(super) host: String,
    pub(super) port: u16,
    pub(super) shown: String,
}

/// The upstream's TLS for a run, from `roots`: verified, offering HTTP/1.1
/// alone.
pub(super) fn upstream_config(roots: Arc<RootCertStore>) -> Result<Arc<ClientConfig>, String> {
    let mut config =
        ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .map_err(|e| e.to_string())?
            .with_root_certificates(roots)
            .with_no_client_auth();
    config.alpn_protocols = vec![HTTP11.to_vec()];
    Ok(Arc::new(config))
}

/// The client's TLS, under the run's leaf for its host, offering HTTP/1.1
/// alone.
fn acceptor(leaf: &super::ca::Leaf) -> Result<TlsAcceptor, String> {
    let mut config =
        ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .map_err(|e| e.to_string())?
            .with_no_client_auth()
            .with_single_cert(
                leaf.chain.clone(),
                PrivateKeyDer::Pkcs8(leaf.key.clone_key()),
            )
            .map_err(|e| e.to_string())?;
    config.alpn_protocols = vec![HTTP11.to_vec()];
    Ok(TlsAcceptor::from(Arc::new(config)))
}

/// TLS to the upstream on `tcp`, as `host`, verified, within the deadline.
async fn verified(
    config: Arc<ClientConfig>,
    host: &str,
    tcp: TcpStream,
) -> Result<Upstream, String> {
    let name = tls::server_name(host)?;
    match tokio::time::timeout(
        Duration::from_secs(DIAL_SECS),
        TlsConnector::from(config).connect(name, tcp),
    )
    .await
    {
        Ok(Ok(tls)) => Ok(tls),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err(format!("no TLS within {DIAL_SECS} s")),
    }
}

/// One connection to a placeholder's audience: the upstream's TLS verified,
/// the client told it is through, its hello read, and then its TLS
/// terminated and its requests served; or, where its hello asks for anything
/// else, carried as it came.
#[allow(clippy::too_many_arguments)]
pub(super) async fn carry(
    front: Front,
    mut tcp: TcpStream,
    upstream: TcpStream,
    early: Vec<u8>,
    target: Target,
    run: RunKey,
    state: Arc<RunState>,
    seams: Seams,
) {
    let config = match state.upstream_tls(&seams.roots).await {
        Ok(config) => config,
        Err(why) => {
            unverified(front, &mut tcp, &target, run, why).await;
            return;
        }
    };
    let up = match verified(config.clone(), &target.host, upstream).await {
        Ok(up) => up,
        Err(why) => {
            unverified(front, &mut tcp, &target, run, why).await;
            return;
        }
    };
    if !super::accepted(front, &mut tcp).await {
        return;
    }
    let hello = tokio::time::timeout(
        Duration::from_secs(HANDSHAKE_SECS),
        read_hello(&mut tcp, early),
    )
    .await;
    let hello = match hello {
        Ok(Ok(hello)) => hello,
        Ok(Err(e)) => {
            debug!(session = %run.0, target = %target.shown, %e, "hive: an egress hello it could not read");
            return;
        }
        Err(_) => {
            debug!(session = %run.0, target = %target.shown, "hive: an egress hello that never came");
            return;
        }
    };
    if let Verdict::Through(why) = judge(hello.asks.as_ref(), &target.host) {
        // Carried as it came: never decrypted, so nothing is swapped. The
        // leg made for the swap was never used, so a new one carries it.
        drop(up);
        debug!(
            session = %run.0, fence = run.1, target = %target.shown, why = %why,
            "hive: egress to a placeholder's audience carried as it came"
        );
        let reached = super::reach(
            &state,
            run,
            &target.host,
            target.port,
            &*seams.decide,
            &*seams.dial,
        )
        .await;
        match reached {
            Ok((_, upstream)) => {
                let _ = super::relay(tcp, upstream, hello.bytes, state.stop.clone()).await;
            }
            Err(refusal) => debug!(
                session = %run.0, target = %target.shown, ?refusal,
                "hive: egress to carry as it came could not be dialed again"
            ),
        }
        return;
    }
    let made = state
        .ca
        .leaf_for(&target.host)
        .and_then(|leaf| acceptor(&leaf));
    let acceptor = match made {
        Ok(acceptor) => acceptor,
        Err(why) => {
            warn!(
                session = %run.0, fence = run.1, target = %target.shown, why = %why,
                "hive: egress to a placeholder's audience closed — no leaf for it"
            );
            return;
        }
    };
    let client = tokio::time::timeout(
        Duration::from_secs(HANDSHAKE_SECS),
        acceptor.accept(Prefixed::new(hello.bytes, tcp)),
    )
    .await;
    let client = match client {
        Ok(Ok(client)) => client,
        Ok(Err(e)) => {
            debug!(session = %run.0, target = %target.shown, %e, "hive: the client's TLS did not complete");
            return;
        }
        Err(_) => {
            debug!(session = %run.0, target = %target.shown, "hive: the client's TLS did not complete in time");
            return;
        }
    };
    let leg = match leg_over(up).await {
        Ok(leg) => leg,
        Err(why) => {
            debug!(session = %run.0, target = %target.shown, why = %why, "hive: the upstream's HTTP did not start");
            return;
        }
    };
    debug!(
        session = %run.0, fence = run.1, target = %target.shown,
        "hive: egress TLS terminated for a placeholder's audience"
    );
    let stop = state.stop.clone();
    let conn = Conn {
        run,
        target,
        state,
        seams,
        config,
        leg: Mutex::new(Some(leg)),
    };
    tokio::select! {
        served = serve(client, &conn) => {
            if let Err(e) = served {
                debug!(session = %run.0, target = %conn.target.shown, %e, "hive: a terminated egress connection ended with an error");
            }
        }
        () = stop.cancelled() => {}
    }
}

/// Refuse a connection whose upstream did not verify, as its front speaks.
async fn unverified(front: Front, tcp: &mut TcpStream, target: &Target, run: RunKey, why: String) {
    warn!(
        session = %run.0, fence = run.1, target = %target.shown, why = %why,
        "hive: egress refused — a placeholder's audience whose TLS did not verify"
    );
    super::refuse(front, tcp, &target.shown, &Refusal::Unverified(why)).await;
}

/// A client's first flight, read and not answered: its bytes, to go on as
/// they came, and what its hello asks for.
struct Hello {
    bytes: Vec<u8>,
    /// `None` where the bytes are no TLS hello.
    asks: Option<Asks>,
}

/// What a TLS hello asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Asks {
    /// The name it gives (SNI).
    name: Option<String>,
    /// The protocols it offers (ALPN), where it offers any.
    protocols: Option<Vec<Vec<u8>>>,
}

/// Read the client's hello, and not answer it: every byte kept, the first
/// `early` ones included. rustls reads it, as it would to answer it.
async fn read_hello(tcp: &mut TcpStream, mut bytes: Vec<u8>) -> io::Result<Hello> {
    let mut acceptor = rustls::server::Acceptor::default();
    let mut fed = 0;
    loop {
        while fed < bytes.len() {
            match acceptor.read_tls(&mut &bytes[fed..]) {
                Ok(0) => break,
                Ok(n) => fed += n,
                Err(_) => return Ok(Hello { bytes, asks: None }),
            }
        }
        match acceptor.accept() {
            Ok(Some(accepted)) => {
                let hello = accepted.client_hello();
                let asks = Asks {
                    name: hello.server_name().map(str::to_string),
                    protocols: hello.alpn().map(|p| p.map(<[u8]>::to_vec).collect()),
                };
                return Ok(Hello {
                    bytes,
                    asks: Some(asks),
                });
            }
            Ok(None) => {}
            Err(_) => return Ok(Hello { bytes, asks: None }),
        }
        if bytes.len() >= MAX_HELLO {
            return Ok(Hello { bytes, asks: None });
        }
        let mut chunk = [0u8; 4096];
        let n = tcp.read(&mut chunk).await?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        bytes.extend_from_slice(&chunk[..n]);
    }
}

/// Whether a connection is terminated, by what its hello asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Verdict {
    Terminate,
    /// Carried as it came, and why.
    Through(String),
}

/// Terminate only a TLS hello that names the host, or names none, and
/// offers HTTP/1.1, or offers nothing.
fn judge(asks: Option<&Asks>, host: &str) -> Verdict {
    let Some(asks) = asks else {
        return Verdict::Through("no TLS hello".into());
    };
    if let Some(name) = &asks.name
        && canonical_net_host(name).as_deref() != Some(host)
    {
        return Verdict::Through(format!("its hello names {name}"));
    }
    if let Some(protocols) = &asks.protocols
        && !protocols.iter().any(|p| p == HTTP11)
    {
        return Verdict::Through("its hello offers no HTTP/1.1".into());
    }
    Verdict::Terminate
}

/// The bytes already read from a stream, then the stream: how a hello read
/// ahead goes to TLS after all.
struct Prefixed<S> {
    head: Vec<u8>,
    at: usize,
    inner: S,
}

impl<S> Prefixed<S> {
    fn new(head: Vec<u8>, inner: S) -> Self {
        Self { head, at: 0, inner }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for Prefixed<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.at < this.head.len() {
            let n = buf.remaining().min(this.head.len() - this.at);
            buf.put_slice(&this.head[this.at..this.at + n]);
            this.at += n;
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut this.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Prefixed<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

/// One terminated connection: what each of its requests needs.
struct Conn {
    run: RunKey,
    target: Target,
    state: Arc<RunState>,
    seams: Seams,
    config: Arc<ClientConfig>,
    /// The upstream's leg, between requests: taken out while one is sent.
    leg: Mutex<Option<Leg>>,
}

/// The upstream's HTTP/1.1 connection, and its task, ended with it.
struct Leg {
    send: SendRequest<Incoming>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Leg {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// HTTP/1.1 over the upstream's verified TLS.
async fn leg_over(up: Upstream) -> Result<Leg, String> {
    let (send, conn) = hyper::client::conn::http1::handshake(TokioIo::new(up))
        .await
        .map_err(|e| e.to_string())?;
    let task = tokio::spawn(async move {
        if let Err(e) = conn.await {
            debug!(%e, "hive: an egress upstream connection ended with an error");
        }
    });
    Ok(Leg { send, task })
}

/// A new leg, as the first was made: the run's decision, the dial, TLS
/// verified.
async fn redial(conn: &Conn) -> Result<Leg, String> {
    let (_, tcp) = super::reach(
        &conn.state,
        conn.run,
        &conn.target.host,
        conn.target.port,
        &*conn.seams.decide,
        &*conn.seams.dial,
    )
    .await
    .map_err(|refusal| refusal.body(&conn.target.shown).trim_end().to_string())?;
    leg_over(verified(conn.config.clone(), &conn.target.host, tcp).await?).await
}

/// Serve the client's requests until it closes.
async fn serve<C>(client: C, conn: &Conn) -> Result<(), String>
where
    C: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let service =
        hyper::service::service_fn(
            |req| async move { Ok::<_, Infallible>(forward(req, conn).await) },
        );
    hyper::server::conn::http1::Builder::new()
        .timer(TokioTimer::new())
        .header_read_timeout(Duration::from_secs(HEADER_SECS))
        .serve_connection(TokioIo::new(client), service)
        .await
        .map_err(|e| e.to_string())
}

/// One request: checked, swapped, sent on and answered; or refused, with why.
async fn forward(req: Request<Incoming>, conn: &Conn) -> Response<Body> {
    let req = match outbound(req, &conn.target) {
        Ok(req) => req,
        Err((status, why)) => return answer(status, &why),
    };
    let req = match swap(req, conn) {
        Ok(req) => req,
        Err(why) => return answer(StatusCode::BAD_GATEWAY, &why),
    };
    let held = lock(&conn.leg).take();
    match send(held, req, conn).await {
        Ok((resp, kept)) => {
            *lock(&conn.leg) = Some(kept);
            inbound(resp)
        }
        Err(why) => answer(StatusCode::BAD_GATEWAY, &why),
    }
}

/// Send `req` on `held`, or on a new leg where there is none or the upstream
/// closed it; once more on a new one if the upstream closed it before the
/// request went out. The response, and the leg to keep.
async fn send(
    mut held: Option<Leg>,
    mut req: Request<Incoming>,
    conn: &Conn,
) -> Result<(Response<Incoming>, Leg), String> {
    for _ in 0..2 {
        let mut leg = match held.take() {
            Some(mut leg) => match leg.send.ready().await {
                Ok(()) => leg,
                Err(_) => redial(conn).await?,
            },
            None => redial(conn).await?,
        };
        match leg.send.try_send_request(req).await {
            Ok(resp) => return Ok((resp, leg)),
            Err(mut e) => match e.take_message() {
                // Never written: the upstream closed the leg first.
                Some(back) => req = back,
                None => return Err(format!("{}: {}", conn.target.shown, e.into_error())),
            },
        }
    }
    Err(format!(
        "{} closed every connection before the request went out",
        conn.target.shown
    ))
}

/// The request as the upstream gets it, or the status and why not: for this
/// host and port alone, in origin form, less what is one leg's.
fn outbound(
    req: Request<Incoming>,
    target: &Target,
) -> Result<Request<Incoming>, (StatusCode, String)> {
    if req.method() == Method::CONNECT {
        return Err((
            StatusCode::METHOD_NOT_ALLOWED,
            "roomler egress proxy: a CONNECT inside a connection is not carried".into(),
        ));
    }
    if req.headers().contains_key(header::UPGRADE) {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            format!(
                "roomler egress proxy: an upgrade is not carried on a connection to {}, where a \
                 placeholder is swapped",
                target.shown
            ),
        ));
    }
    let misdirected = || {
        (
            StatusCode::MISDIRECTED_REQUEST,
            format!(
                "roomler egress proxy: this connection is to {}: a request for another host goes \
                 on a connection of its own",
                target.shown
            ),
        )
    };
    let mut hosts = req.headers().get_all(header::HOST).iter();
    let (Some(named), None) = (hosts.next(), hosts.next()) else {
        return Err((
            StatusCode::BAD_REQUEST,
            "roomler egress proxy: a request names its host once".into(),
        ));
    };
    if !names(named.to_str().ok(), &target.host, target.port) {
        return Err(misdirected());
    }
    let uri = req.uri();
    let path = match (uri.scheme(), uri.authority(), uri.path_and_query()) {
        (None, None, Some(pq)) if pq.as_str().starts_with('/') => pq.clone(),
        (Some(scheme), Some(authority), pq) if *scheme == Scheme::HTTPS => {
            if !names(Some(authority.as_str()), &target.host, target.port) {
                return Err(misdirected());
            }
            pq.cloned()
                .unwrap_or_else(|| PathAndQuery::from_static("/"))
        }
        _ => {
            return Err((
                StatusCode::BAD_REQUEST,
                "roomler egress proxy: a request for a path".into(),
            ));
        }
    };
    let (mut parts, body) = req.into_parts();
    parts.uri = Uri::from(path);
    parts.version = hyper::Version::HTTP_11;
    parts.extensions = Default::default();
    hop_by_hop(&mut parts.headers);
    parts.headers.remove(header::EXPECT);
    Ok(Request::from_parts(parts, body))
}

/// Whether `authority` (`host`, `host:port`, `[address]` or
/// `[address]:port`) names `host` in its one spelling, and `port`: 443
/// where it names none.
fn names(authority: Option<&str>, host: &str, port: u16) -> bool {
    let Some(authority) = authority else {
        return false;
    };
    let (name, named_port) = match authority.strip_prefix('[') {
        Some(rest) => match rest.split_once(']') {
            Some((name, "")) => (name, None),
            Some((name, tail)) => match tail.strip_prefix(':') {
                Some(p) => (name, Some(p)),
                None => return false,
            },
            None => return false,
        },
        None => match authority.split_once(':') {
            Some((name, p)) if !p.contains(':') => (name, Some(p)),
            // An IPv6 address names its port only in brackets.
            Some(_) => return false,
            None => (authority, None),
        },
    };
    let port_ok = match named_port {
        None => port == 443,
        Some(p) => p.parse::<u16>().ok() == Some(port),
    };
    port_ok && canonical_net_host(name).as_deref() == Some(host)
}

/// The response as the client gets it.
fn inbound(resp: Response<Incoming>) -> Response<Body> {
    let (mut parts, body) = resp.into_parts();
    hop_by_hop(&mut parts.headers);
    Response::from_parts(parts, body.map_err(BoxError::from).boxed())
}

/// Each of this audience's placeholders in the request's `Authorization`,
/// swapped for its value, read now (decision 32). Err where a value is not
/// one the header can carry: the request is then never sent.
fn swap(mut req: Request<Incoming>, conn: &Conn) -> Result<Request<Incoming>, String> {
    if !req.headers().contains_key(header::AUTHORIZATION) {
        return Ok(req);
    }
    let bound = placeholder::bound(conn.run.0, conn.run.1, &conn.target.host, conn.target.port);
    if bound.is_empty() {
        return Ok(req);
    }
    let value_of = |lease: &str| super::Values::value(&*conn.seams.values, lease);
    let mut used = Vec::new();
    let mut swapped = Vec::new();
    for value in req.headers().get_all(header::AUTHORIZATION) {
        swapped.push(swap_authorization(value, &bound, &value_of, &mut used)?);
    }
    if used.is_empty() {
        return Ok(req);
    }
    let headers: &mut HeaderMap = req.headers_mut();
    headers.remove(header::AUTHORIZATION);
    for value in swapped {
        headers.append(header::AUTHORIZATION, value);
    }
    for lease in &used {
        debug!(
            session = %conn.run.0, fence = conn.run.1, target = %conn.target.shown, lease = %lease,
            "hive: a placeholder swapped for its value"
        );
    }
    Ok(req)
}

/// One `Authorization` value with each placeholder of `bound` in it swapped
/// for its value: as written, and inside `Basic`'s credentials, decoded and
/// encoded again (RFC 7617). Each lease whose value went in is added to
/// `used`. Err where the result is not one a header can carry.
fn swap_authorization(
    value: &HeaderValue,
    bound: &[Bound],
    value_of: &dyn Fn(&str) -> Option<Zeroizing<Vec<u8>>>,
    used: &mut Vec<String>,
) -> Result<HeaderValue, String> {
    let raw = value.as_bytes();
    // AWS's signature is made with the secret key: a value swapped in
    // without signing again only breaks it, and S3's answer to a broken one
    // names the access key it was sent. P3b-4 signs again; until then it
    // goes out as it is.
    if aws_signed(raw) {
        return Ok(value.clone());
    }
    // A placeholder never reads as base64 (`_` is no letter of its
    // alphabet), so one written plainly after `Basic` is swapped as written.
    if let Some(at) = basic_credentials(raw)
        && let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(&raw[at..])
    {
        let decoded = Zeroizing::new(decoded);
        let Some(out) = swapped(&decoded, bound, value_of, used) else {
            return Ok(value.clone());
        };
        let encoded =
            Zeroizing::new(base64::engine::general_purpose::STANDARD.encode(out.as_slice()));
        let mut header = Zeroizing::new(raw[..at].to_vec());
        header.extend_from_slice(encoded.as_bytes());
        return header_of(&header);
    }
    match swapped(raw, bound, value_of, used) {
        Some(out) => header_of(&out),
        None => Ok(value.clone()),
    }
}

/// Whether `raw` is AWS's signature (`AWS4-HMAC-SHA256 Credential=…`, or
/// the older `AWS …`): its scheme starts `AWS`, in any case.
fn aws_signed(raw: &[u8]) -> bool {
    raw.get(..3).is_some_and(|s| s.eq_ignore_ascii_case(b"aws"))
}

/// Where `Basic`'s credentials start in `raw`: after the scheme, in any
/// case, and its spaces.
fn basic_credentials(raw: &[u8]) -> Option<usize> {
    let scheme = raw.get(..5)?;
    if !scheme.eq_ignore_ascii_case(b"basic") {
        return None;
    }
    let spaces = raw[5..].iter().take_while(|b| **b == b' ').count();
    (spaces > 0 && raw.len() > 5 + spaces).then_some(5 + spaces)
}

/// `hay` with each placeholder of `bound` in it swapped for its value, in one
/// pass, so no value is read again for a placeholder; `None` where none was
/// swapped. A placeholder whose lease was let go stays as it is.
fn swapped(
    hay: &[u8],
    bound: &[Bound],
    value_of: &dyn Fn(&str) -> Option<Zeroizing<Vec<u8>>>,
    used: &mut Vec<String>,
) -> Option<Zeroizing<Vec<u8>>> {
    let len = PREFIX.len() + RANDOM_CHARS;
    let mut out = Zeroizing::new(Vec::with_capacity(hay.len()));
    let (mut copied, mut at, mut any) = (0, 0, false);
    while let Some(found) = find(&hay[at..], PREFIX.as_bytes()) {
        let start = at + found;
        let hit = hay
            .get(start..start + len)
            .and_then(|w| bound.iter().find(|b| b.token.as_bytes() == w))
            .and_then(|b| value_of(&b.lease_id).map(|v| (b, v)));
        match hit {
            Some((b, v)) => {
                out.extend_from_slice(&hay[copied..start]);
                out.extend_from_slice(&v);
                if !used.contains(&b.lease_id) {
                    used.push(b.lease_id.clone());
                }
                copied = start + len;
                at = copied;
                any = true;
            }
            None => at = start + 1,
        }
    }
    if !any {
        return None;
    }
    out.extend_from_slice(&hay[copied..]);
    Some(out)
}

/// Where `needle` first is in `hay`.
fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// A header's value, marked sensitive; Err where the bytes are not one a
/// header can carry.
fn header_of(bytes: &[u8]) -> Result<HeaderValue, String> {
    let mut value = HeaderValue::from_bytes(bytes).map_err(|_| {
        "roomler egress proxy: the secret's value is not one a header can carry (a control \
         character or a line break): the request was not sent"
            .to_string()
    })?;
    value.set_sensitive(true);
    Ok(value)
}

#[cfg(test)]
mod tests;
