// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P3b-3c-1 — termination and the swap, end to end on loopback. A test
//! audience (TLS under a CA of its own, HTTP/1.1, recording what it is sent)
//! stands in for the upstream. A client that trusts the run's CA alone, or
//! the audience's alone, shows whose TLS it met.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicUsize, Ordering};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use bson::oid::ObjectId;
use bytes::Bytes;
use http_body_util::Full;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio_rustls::client::TlsStream;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer, ServerName};

use super::super::{
    Admit, Decide, Decision, Dial, Fronts, Roots, Seams, Values, ca_of, drop_run, socks5,
    start_admitting,
};
use super::*;

/// The audience's host, and another name its certificate also carries.
const HOST: &str = "api.test";
const OTHER: &str = "other.test";
/// What a client offers, as curl and Go do.
const H1: &[&[u8]] = &[b"h2", b"http/1.1"];

/// One request, as the audience was sent it.
#[derive(Debug, Clone)]
struct Seen {
    target: String,
    host: String,
    authorization: Vec<String>,
    api_key: Option<String>,
    agent: Option<String>,
    body: String,
}

/// A test audience: TLS under a CA of its own, for both names; HTTP/1.1,
/// each request recorded and answered `200`, its connection closed after
/// each where `close` says so.
struct Audience {
    addr: SocketAddr,
    ca: CertificateDer<'static>,
    seen: Arc<Mutex<Vec<Seen>>>,
    connections: Arc<AtomicUsize>,
}

impl Audience {
    async fn start(close: bool) -> Self {
        let ca_key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        params.key_usages = vec![
            rcgen::KeyUsagePurpose::KeyCertSign,
            rcgen::KeyUsagePurpose::CrlSign,
        ];
        let ca = params.self_signed(&ca_key).unwrap();
        let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let leaf = rcgen::CertificateParams::new(vec![HOST.to_string(), OTHER.to_string()])
            .unwrap()
            .signed_by(&key, &ca, &ca_key)
            .unwrap();
        let config =
            ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_no_client_auth()
                .with_single_cert(
                    vec![leaf.der().clone()],
                    PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der())),
                )
                .unwrap();
        let acceptor = TlsAcceptor::from(Arc::new(config));
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let connections = Arc::new(AtomicUsize::new(0));
        let (seen_by, counted) = (seen.clone(), connections.clone());
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                counted.fetch_add(1, Ordering::SeqCst);
                let (acceptor, seen) = (acceptor.clone(), seen_by.clone());
                tokio::spawn(async move {
                    let Ok(tls) = acceptor.accept(tcp).await else {
                        return;
                    };
                    let service = hyper::service::service_fn(move |req: Request<Incoming>| {
                        let seen = seen.clone();
                        async move { Ok::<_, Infallible>(answered(req, &seen, close).await) }
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(tls), service)
                        .await;
                });
            }
        });
        Self {
            addr,
            ca: ca.der().clone(),
            seen,
            connections,
        }
    }

    fn seen(&self) -> Vec<Seen> {
        lock(&self.seen).clone()
    }

    fn connections(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }
}

/// A request recorded, and answered.
async fn answered(
    req: Request<Incoming>,
    seen: &Mutex<Vec<Seen>>,
    close: bool,
) -> Response<Full<Bytes>> {
    let (parts, body) = req.into_parts();
    let body = body
        .collect()
        .await
        .map(|b| b.to_bytes())
        .unwrap_or_default();
    let named = |name: &str| {
        parts
            .headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    lock(seen).push(Seen {
        target: parts.uri.to_string(),
        host: named("host").unwrap_or_default(),
        authorization: parts
            .headers
            .get_all(header::AUTHORIZATION)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .map(str::to_string)
            .collect(),
        api_key: named("x-api-key"),
        agent: named("user-agent"),
        body: String::from_utf8_lossy(&body).into_owned(),
    });
    let mut resp = Response::new(Full::new(Bytes::from_static(b"from the audience")));
    if close {
        resp.headers_mut()
            .insert(header::CONNECTION, HeaderValue::from_static("close"));
    }
    resp
}

/// Allows every host.
struct AllowAll;

#[async_trait::async_trait]
impl Decide for AllowAll {
    async fn decide(&self, _run: RunKey, _host: &str, _port: u16) -> Decision {
        Decision::Allowed
    }
}

/// Dials the audience, whatever the host.
struct ToAudience(SocketAddr);

#[async_trait::async_trait]
impl Dial for ToAudience {
    async fn dial(&self, _host: &str, _port: u16) -> Result<TcpStream, Refusal> {
        TcpStream::connect(self.0)
            .await
            .map_err(|e| Refusal::Unreachable(e.to_string()))
    }
}

/// Each lease's value.
#[derive(Default)]
struct Leases(Mutex<HashMap<String, Vec<u8>>>);

impl Values for Leases {
    fn value(&self, lease_id: &str) -> Option<Zeroizing<Vec<u8>>> {
        lock(&self.0)
            .get(lease_id)
            .map(|v| Zeroizing::new(v.clone()))
    }
}

/// A run proxied to an audience, its upstream verified against one root.
struct Rig {
    run: RunKey,
    fronts: Fronts,
    leases: Arc<Leases>,
    /// The run's CA.
    ca: CertificateDer<'static>,
}

impl Rig {
    async fn new(audience: &Audience, root: &CertificateDer<'static>) -> Self {
        let run = (ObjectId::new(), 1);
        let leases = Arc::new(Leases::default());
        let mut roots = RootCertStore::empty();
        roots.add(root.clone()).unwrap();
        let seams = Seams {
            decide: Arc::new(AllowAll),
            dial: Arc::new(ToAudience(audience.addr)),
            values: leases.clone(),
            roots: Roots::Given(Arc::new(roots)),
        };
        let admit: Admit = Arc::new(|_: &TcpStream, _: SocketAddr| Ok(()));
        let fronts = start_admitting(run, admit, seams).unwrap();
        let ca = ca_of(run.0, run.1).unwrap().der();
        Self {
            run,
            fronts,
            leases,
            ca,
        }
    }

    /// A placeholder bound to the run for `host`, port 443, its lease
    /// holding `value`.
    fn hold(&self, host: &str, value: &[u8]) -> String {
        let token = placeholder::bind_for_test(self.run.0, self.run.1, host, 443);
        lock(&self.leases.0).insert(format!("test-lease-{token}"), value.to_vec());
        token
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        drop_run(self.run.0, self.run.1);
        placeholder::drop_run(self.run.0, self.run.1);
    }
}

/// The status line of an answer, read to its blank line.
async fn status_line(tcp: &mut TcpStream) -> String {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    while !buf.ends_with(b"\r\n\r\n") {
        match tcp.read(&mut byte).await {
            Ok(1) => buf.push(byte[0]),
            _ => break,
        }
    }
    String::from_utf8_lossy(&buf)
        .lines()
        .next()
        .unwrap_or_default()
        .to_string()
}

/// Through the HTTP front (`CONNECT connect`), then TLS as `name`, trusting
/// `trust` alone and offering `alpn`: the stream, or the front's status line
/// or TLS's error.
async fn through(
    front: SocketAddr,
    connect: &str,
    name: &str,
    trust: &CertificateDer<'static>,
    alpn: &[&[u8]],
) -> Result<TlsStream<TcpStream>, String> {
    let mut tcp = TcpStream::connect(front).await.unwrap();
    tcp.write_all(format!("CONNECT {connect} HTTP/1.1\r\nHost: {connect}\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let status = status_line(&mut tcp).await;
    if status != "HTTP/1.1 200 Connection established" {
        return Err(status);
    }
    tls_over(tcp, name, trust, alpn).await
}

/// TLS as `name` on `tcp`, trusting `trust` alone, offering `alpn`.
async fn tls_over(
    tcp: TcpStream,
    name: &str,
    trust: &CertificateDer<'static>,
    alpn: &[&[u8]],
) -> Result<TlsStream<TcpStream>, String> {
    let mut roots = RootCertStore::empty();
    roots.add(trust.clone()).unwrap();
    let mut config =
        ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
    config.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    let name = ServerName::try_from(name.to_string()).unwrap();
    tokio::time::timeout(
        Duration::from_secs(10),
        TlsConnector::from(Arc::new(config)).connect(name, tcp),
    )
    .await
    .map_err(|_| "no TLS within 10 s".to_string())?
    .map_err(|e| e.to_string())
}

/// A SOCKS5 CONNECT by name, with no authentication: the reply's code, and
/// the stream.
async fn socks_connect(front: SocketAddr, host: &str, port: u16) -> (u8, TcpStream) {
    let mut tcp = TcpStream::connect(front).await.unwrap();
    tcp.write_all(&[5, 1, 0]).await.unwrap();
    let mut selected = [0u8; 2];
    tcp.read_exact(&mut selected).await.unwrap();
    let mut request = vec![5, 1, 0, 3, u8::try_from(host.len()).unwrap()];
    request.extend_from_slice(host.as_bytes());
    request.extend_from_slice(&port.to_be_bytes());
    tcp.write_all(&request).await.unwrap();
    let mut reply = [0u8; 10];
    tcp.read_exact(&mut reply).await.unwrap();
    (reply[1], tcp)
}

/// HTTP/1.1 over `tls`, its connection driven aside.
async fn http(tls: TlsStream<TcpStream>) -> SendRequest<Full<Bytes>> {
    let (send, conn) = hyper::client::conn::http1::handshake(TokioIo::new(tls))
        .await
        .unwrap();
    tokio::spawn(async move {
        let _ = conn.await;
    });
    send
}

/// One request: its status, and its body.
async fn ask(
    send: &mut SendRequest<Full<Bytes>>,
    req: Request<Full<Bytes>>,
) -> (StatusCode, String) {
    send.ready().await.unwrap();
    let resp = tokio::time::timeout(Duration::from_secs(10), send.send_request(req))
        .await
        .expect("an answer within 10 s")
        .unwrap();
    let status = resp.status();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&body).into_owned())
}

/// `GET path`, its `Host`, and its `Authorization` where it has one.
fn get(path: &str, host: &str, authorization: Option<&str>) -> Request<Full<Bytes>> {
    let mut req = Request::get(path).header(header::HOST, host);
    if let Some(a) = authorization {
        req = req.header(header::AUTHORIZATION, a);
    }
    req.body(Full::new(Bytes::new())).unwrap()
}

/// A connection to a placeholder's audience meets the run's CA, and each
/// placeholder of that audience in `Authorization` reaches it as its value:
/// as written, and inside `Basic`'s credentials. The requests share one
/// connection to the audience, and the answers come back as they were.
#[tokio::test]
async fn an_audiences_placeholder_is_swapped_in_authorization_under_the_runs_ca() {
    let audience = Audience::start(false).await;
    let rig = Rig::new(&audience, &audience.ca).await;
    let token = rig.hold(HOST, b"the-value");
    let tls = through(rig.fronts.http, "api.test:443", HOST, &rig.ca, H1)
        .await
        .expect("the run's CA, the client's one root, is what it meets");
    let mut send = http(tls).await;
    let (status, body) = ask(
        &mut send,
        get("/user", HOST, Some(&format!("token {token}"))),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "from the audience");
    let basic = |s: &str| format!("Basic {}", STANDARD.encode(s));
    let git = basic(&format!("x-access-token:{token}"));
    let (status, _) = ask(
        &mut send,
        get("/o/r.git/info/refs", "API.test:443", Some(&git)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let seen = audience.seen();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].authorization, ["token the-value"]);
    assert_eq!(
        (seen[0].target.as_str(), seen[0].host.as_str()),
        ("/user", HOST)
    );
    assert_eq!(seen[1].authorization, [basic("x-access-token:the-value")]);
    assert_eq!(
        audience.connections(),
        1,
        "one connection to the audience carried both"
    );
}

/// A placeholder anywhere but `Authorization` (the URL, another header, the
/// body) reaches the audience as itself (decision 32).
#[tokio::test]
async fn a_placeholder_anywhere_but_authorization_goes_out_as_itself() {
    let audience = Audience::start(false).await;
    let rig = Rig::new(&audience, &audience.ca).await;
    let token = rig.hold(HOST, b"the-value");
    let tls = through(rig.fronts.http, "api.test:443", HOST, &rig.ca, H1)
        .await
        .unwrap();
    let mut send = http(tls).await;
    let body = format!("{{\"body\":\"{token}\"}}");
    let req = Request::post(format!("/issues?token={token}"))
        .header(header::HOST, HOST)
        .header("x-api-key", token.as_str())
        .header(header::USER_AGENT, token.as_str())
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Full::new(Bytes::from(body.clone())))
        .unwrap();
    assert_eq!(ask(&mut send, req).await.0, StatusCode::OK);
    let seen = audience.seen();
    assert_eq!(
        seen[0].target,
        format!("/issues?token={token}"),
        "never the URL"
    );
    assert_eq!(
        seen[0].api_key.as_deref(),
        Some(token.as_str()),
        "nor another header"
    );
    assert_eq!(seen[0].agent.as_deref(), Some(token.as_str()));
    assert_eq!(seen[0].body, body, "nor the body");
    assert_eq!(
        seen[0].authorization,
        ["Bearer the-value"],
        "Authorization alone"
    );
}

/// Another audience's placeholder goes out as itself on this one's
/// connection, and an AWS signature goes out as it is: P3b-4 signs again.
#[tokio::test]
async fn another_audiences_placeholder_goes_out_as_itself() {
    let audience = Audience::start(false).await;
    let rig = Rig::new(&audience, &audience.ca).await;
    let mine = rig.hold(HOST, b"mine");
    let theirs = rig.hold(OTHER, b"theirs");
    let tls = through(rig.fronts.http, "api.test:443", HOST, &rig.ca, H1)
        .await
        .unwrap();
    let mut send = http(tls).await;
    ask(
        &mut send,
        get("/a", HOST, Some(&format!("x {mine} {theirs}"))),
    )
    .await;
    let aws = format!(
        "AWS4-HMAC-SHA256 Credential={mine}/20261022/us-east-1/s3/aws4_request, \
         SignedHeaders=host, Signature=00"
    );
    ask(&mut send, get("/b", HOST, Some(&aws))).await;
    let seen = audience.seen();
    assert_eq!(seen[0].authorization, [format!("x mine {theirs}")]);
    assert_eq!(seen[1].authorization, [aws], "P3b-4's");
}

/// A host the run holds no placeholder for is carried as it comes: the
/// client meets the audience's own TLS, never the run's CA, and what it
/// sends goes out as it is.
#[tokio::test]
async fn a_host_with_no_placeholder_is_carried_never_decrypted() {
    let audience = Audience::start(false).await;
    let rig = Rig::new(&audience, &audience.ca).await;
    let theirs = rig.hold(OTHER, b"theirs");
    assert!(
        through(rig.fronts.http, "api.test:443", HOST, &rig.ca, H1)
            .await
            .is_err(),
        "the run's CA is not what it meets"
    );
    let tls = through(rig.fronts.http, "api.test:443", HOST, &audience.ca, H1)
        .await
        .expect("the audience's own TLS");
    let mut send = http(tls).await;
    ask(&mut send, get("/a", HOST, Some(&format!("token {theirs}")))).await;
    assert_eq!(
        audience.seen()[0].authorization,
        [format!("token {theirs}")]
    );
}

/// A hello that names another host than the CONNECT's is carried as it came,
/// on a new dial: the client meets the audience's own TLS, as the name it
/// gave, and nothing is swapped.
#[tokio::test]
async fn a_hello_naming_another_host_is_carried_as_it_came() {
    let audience = Audience::start(false).await;
    let rig = Rig::new(&audience, &audience.ca).await;
    let token = rig.hold(HOST, b"the-value");
    assert!(
        through(rig.fronts.http, "api.test:443", OTHER, &rig.ca, H1)
            .await
            .is_err()
    );
    let tls = through(rig.fronts.http, "api.test:443", OTHER, &audience.ca, H1)
        .await
        .expect("the audience's own TLS, as the name the hello gave");
    let mut send = http(tls).await;
    ask(&mut send, get("/a", OTHER, Some(&format!("token {token}")))).await;
    assert_eq!(audience.seen()[0].authorization, [format!("token {token}")]);
}

/// A hello that offers protocols, HTTP/1.1 not among them, is carried as it
/// came.
#[tokio::test]
async fn a_hello_offering_no_http1_is_carried_as_it_came() {
    let audience = Audience::start(false).await;
    let rig = Rig::new(&audience, &audience.ca).await;
    let token = rig.hold(HOST, b"the-value");
    assert!(
        through(rig.fronts.http, "api.test:443", HOST, &rig.ca, &[b"h2"])
            .await
            .is_err()
    );
    let tls = through(
        rig.fronts.http,
        "api.test:443",
        HOST,
        &audience.ca,
        &[b"h2"],
    )
    .await
    .expect("the audience's own TLS");
    // The audience chooses no protocol, so the client speaks HTTP/1.1 to it.
    let mut send = http(tls).await;
    ask(&mut send, get("/a", HOST, Some(&format!("token {token}")))).await;
    assert_eq!(audience.seen()[0].authorization, [format!("token {token}")]);
}

/// An audience whose TLS does not verify is refused before the client is
/// told it is through, on either front: nothing is decrypted, and the
/// audience is sent no request.
#[tokio::test]
async fn an_audience_that_does_not_verify_is_refused_before_anything_is_decrypted() {
    let audience = Audience::start(false).await;
    let stranger = Audience::start(false).await;
    let rig = Rig::new(&audience, &stranger.ca).await;
    rig.hold(HOST, b"the-value");
    let refused = through(rig.fronts.http, "api.test:443", HOST, &rig.ca, H1)
        .await
        .err()
        .unwrap();
    assert_eq!(refused, "HTTP/1.1 502 Bad Gateway");
    let (reply, _tcp) = socks_connect(rig.fronts.socks, HOST, 443).await;
    assert_eq!(reply, socks5::REP_GENERAL_FAILURE);
    assert!(audience.seen().is_empty());
}

/// A request for another host or port on a terminated connection is
/// answered `421` and never sent; one asking to upgrade, `501`; one naming
/// its host twice, `400`. The connection serves its own host between them.
#[tokio::test]
async fn a_request_for_another_host_is_answered_and_never_sent() {
    let audience = Audience::start(false).await;
    let rig = Rig::new(&audience, &audience.ca).await;
    let token = rig.hold(HOST, b"the-value");
    let auth = format!("token {token}");
    let tls = through(rig.fronts.http, "api.test:443", HOST, &rig.ca, H1)
        .await
        .unwrap();
    let mut send = http(tls).await;
    for host in [OTHER, "api.test:8443", "api.test.evil"] {
        assert_eq!(
            ask(&mut send, get("/x", host, Some(&auth))).await.0,
            StatusCode::MISDIRECTED_REQUEST,
            "{host}"
        );
    }
    let absolute = Request::get("https://other.test/x")
        .header(header::HOST, HOST)
        .header(header::AUTHORIZATION, auth.as_str())
        .body(Full::new(Bytes::new()))
        .unwrap();
    assert_eq!(
        ask(&mut send, absolute).await.0,
        StatusCode::MISDIRECTED_REQUEST
    );
    assert!(audience.seen().is_empty(), "none was sent");
    assert_eq!(
        ask(&mut send, get("/x", HOST, Some(&auth))).await.0,
        StatusCode::OK
    );
    let twice = Request::get("/x")
        .header(header::HOST, HOST)
        .header(header::HOST, HOST)
        .body(Full::new(Bytes::new()))
        .unwrap();
    assert_eq!(ask(&mut send, twice).await.0, StatusCode::BAD_REQUEST);
    // Last: after an upgrade it did not take, a connection may close.
    let upgrade = Request::get("/ws")
        .header(header::HOST, HOST)
        .header(header::CONNECTION, "upgrade")
        .header(header::UPGRADE, "websocket")
        .header(header::AUTHORIZATION, auth.as_str())
        .body(Full::new(Bytes::new()))
        .unwrap();
    assert_eq!(ask(&mut send, upgrade).await.0, StatusCode::NOT_IMPLEMENTED);
    let seen = audience.seen();
    assert_eq!(seen.len(), 1, "only the request for its own host was sent");
    assert_eq!(seen[0].authorization, ["token the-value"]);
}

/// A value no header can carry refuses the request, which is never sent.
#[tokio::test]
async fn a_value_no_header_can_carry_is_never_sent() {
    let audience = Audience::start(false).await;
    let rig = Rig::new(&audience, &audience.ca).await;
    let token = rig.hold(HOST, b"bad\r\nx-injected: 1");
    let tls = through(rig.fronts.http, "api.test:443", HOST, &rig.ca, H1)
        .await
        .unwrap();
    let mut send = http(tls).await;
    let (status, body) = ask(
        &mut send,
        get("/user", HOST, Some(&format!("token {token}"))),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(body.contains("not one a header can carry"), "{body}");
    assert!(audience.seen().is_empty());
}

/// A placeholder's value is read as each request goes: once its lease is
/// let go, the placeholder goes out as itself.
#[tokio::test]
async fn a_placeholder_whose_lease_was_let_go_goes_out_as_itself() {
    let audience = Audience::start(false).await;
    let rig = Rig::new(&audience, &audience.ca).await;
    let token = rig.hold(HOST, b"the-value");
    let auth = format!("token {token}");
    let tls = through(rig.fronts.http, "api.test:443", HOST, &rig.ca, H1)
        .await
        .unwrap();
    let mut send = http(tls).await;
    ask(&mut send, get("/a", HOST, Some(&auth))).await;
    lock(&rig.leases.0).clear();
    ask(&mut send, get("/b", HOST, Some(&auth))).await;
    let seen = audience.seen();
    assert_eq!(seen[0].authorization, ["token the-value"]);
    assert_eq!(seen[1].authorization, [auth], "let go");
}

/// When the run ends, a terminated connection ends with it.
#[tokio::test]
async fn the_runs_end_ends_a_terminated_connection() {
    let audience = Audience::start(false).await;
    let rig = Rig::new(&audience, &audience.ca).await;
    rig.hold(HOST, b"the-value");
    let mut tls = through(rig.fronts.http, "api.test:443", HOST, &rig.ca, H1)
        .await
        .unwrap();
    assert!(drop_run(rig.run.0, rig.run.1));
    let mut byte = [0u8; 1];
    let read = tokio::time::timeout(Duration::from_secs(5), tls.read(&mut byte))
        .await
        .expect("the connection ends within 5 s");
    assert_eq!(read.unwrap_or(0), 0);
}

/// An audience that closes its connection after an answer is dialed again
/// for the next request, which is swapped as the first was.
#[tokio::test]
async fn an_upstream_that_closed_is_dialed_again_for_the_next_request() {
    let audience = Audience::start(true).await;
    let rig = Rig::new(&audience, &audience.ca).await;
    let token = rig.hold(HOST, b"the-value");
    let auth = format!("token {token}");
    let tls = through(rig.fronts.http, "api.test:443", HOST, &rig.ca, H1)
        .await
        .unwrap();
    let mut send = http(tls).await;
    for path in ["/a", "/b"] {
        assert_eq!(
            ask(&mut send, get(path, HOST, Some(&auth))).await.0,
            StatusCode::OK
        );
    }
    assert_eq!(audience.connections(), 2, "dialed again");
    assert!(
        audience
            .seen()
            .iter()
            .all(|s| s.authorization == ["token the-value"])
    );
}

/// The SOCKS5 front terminates as the HTTP one does.
#[tokio::test]
async fn the_socks_front_terminates_as_the_http_one_does() {
    let audience = Audience::start(false).await;
    let rig = Rig::new(&audience, &audience.ca).await;
    let token = rig.hold(HOST, b"the-value");
    let (reply, tcp) = socks_connect(rig.fronts.socks, HOST, 443).await;
    assert_eq!(reply, socks5::REP_SUCCESS);
    let tls = tls_over(tcp, HOST, &rig.ca, H1)
        .await
        .expect("the run's CA");
    let mut send = http(tls).await;
    ask(&mut send, get("/a", HOST, Some(&format!("token {token}")))).await;
    assert_eq!(audience.seen()[0].authorization, ["token the-value"]);
}

/// `Basic`'s credentials are decoded, swapped and encoded again, its scheme
/// in any case; a placeholder written plainly after `Basic` is swapped as
/// written; a value with none is left as it is, as is one whose lease was
/// let go; each lease is named once, however often its placeholder is.
#[test]
fn basic_credentials_are_decoded_swapped_and_encoded_again() {
    let token = placeholder::mint().unwrap();
    let bound = [Bound {
        token: token.clone(),
        lease_id: "l1".into(),
        host: HOST.into(),
        port: 443,
    }];
    let held = |lease: &str| (lease == "l1").then(|| Zeroizing::new(b"v@lue".to_vec()));
    let header = |s: String| HeaderValue::from_str(&s).unwrap();
    let mut used = Vec::new();
    let basic = header(format!("bAsIc  {}", STANDARD.encode(format!("u:{token}"))));
    let out = swap_authorization(&basic, &bound, &held, &mut used).unwrap();
    assert_eq!(
        out.to_str().unwrap(),
        format!("bAsIc  {}", STANDARD.encode("u:v@lue"))
    );
    assert!(out.is_sensitive());
    assert_eq!(used, ["l1"]);

    let mut used = Vec::new();
    let plain = header(format!("Basic {token}"));
    let out = swap_authorization(&plain, &bound, &held, &mut used).unwrap();
    assert_eq!(out.to_str().unwrap(), "Basic v@lue");

    let mut used = Vec::new();
    let twice = header(format!("x {token},{token}"));
    let out = swap_authorization(&twice, &bound, &held, &mut used).unwrap();
    assert_eq!(out.to_str().unwrap(), "x v@lue,v@lue");
    assert_eq!(used, ["l1"], "named once");

    let mut used = Vec::new();
    let none = header(format!("Basic {}", STANDARD.encode("u:p")));
    assert_eq!(
        swap_authorization(&none, &bound, &held, &mut used).unwrap(),
        none
    );
    let gone = |_: &str| -> Option<Zeroizing<Vec<u8>>> { None };
    let mut gone_used = Vec::new();
    assert_eq!(
        swap_authorization(&twice, &bound, &gone, &mut gone_used).unwrap(),
        twice
    );
    assert!(used.is_empty() && gone_used.is_empty());
}

/// A request names its connection's host, in any spelling of it, and its
/// port: 443 where it names none.
#[test]
fn a_request_names_its_connections_host_and_port() {
    for ok in ["api.test", "API.Test", "api.test.", "api.test:443"] {
        assert!(names(Some(ok), HOST, 443), "{ok}");
    }
    assert!(names(Some("api.test:8443"), HOST, 8443));
    for not in [
        "other.test",
        "api.test:8443",
        "api.test:",
        "api.test:443:1",
        "user@api.test",
        "api.test.evil",
        "",
    ] {
        assert!(!names(Some(not), HOST, 443), "{not}");
    }
    assert!(
        !names(Some("api.test"), HOST, 8443),
        "naming no port is 443"
    );
    assert!(!names(None, HOST, 443));
    assert!(names(Some("[2001:db8::1]:443"), "2001:db8::1", 443));
    assert!(names(Some("[2001:DB8::1]"), "2001:db8::1", 443));
    assert!(
        !names(Some("2001:db8::1"), "2001:db8::1", 443),
        "an address names its port in brackets"
    );
    assert!(names(Some("10.0.0.5:443"), "10.0.0.5", 443));
}

/// A hello is terminated only where it names the host, or none, and offers
/// HTTP/1.1, or nothing.
#[test]
fn a_hello_is_judged_by_its_name_and_its_protocols() {
    let asks = |name: Option<&str>, protocols: Option<&[&[u8]]>| Asks {
        name: name.map(str::to_string),
        protocols: protocols.map(|p| p.iter().map(|x| x.to_vec()).collect()),
    };
    assert_eq!(
        judge(Some(&asks(Some(HOST), Some(H1))), HOST),
        Verdict::Terminate
    );
    assert_eq!(
        judge(Some(&asks(Some("API.test"), None)), HOST),
        Verdict::Terminate
    );
    assert_eq!(judge(Some(&asks(None, None)), HOST), Verdict::Terminate);
    for through in [
        judge(Some(&asks(Some(OTHER), None)), HOST),
        judge(Some(&asks(Some(HOST), Some(&[b"h2"]))), HOST),
        judge(None, HOST),
    ] {
        assert!(matches!(through, Verdict::Through(_)), "{through:?}");
    }
}

/// A connected pair on loopback.
async fn pair() -> (TcpStream, TcpStream) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (near, far) = tokio::join!(TcpStream::connect(addr), listener.accept());
    (far.unwrap().0, near.unwrap())
}

/// A hello is read whole, however it arrives, and kept as it came, with
/// what it names and offers; bytes that are no TLS hello are kept as they
/// came.
#[tokio::test]
async fn a_hello_is_read_whole_and_kept_as_it_came() {
    let mut config =
        ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(RootCertStore::empty())
            .with_no_client_auth();
    config.alpn_protocols = H1.iter().map(|p| p.to_vec()).collect();
    let mut client =
        rustls::ClientConnection::new(Arc::new(config), ServerName::try_from(HOST).unwrap())
            .unwrap();
    let mut hello = Vec::new();
    client.write_tls(&mut hello).unwrap();
    // Its first bytes as those behind a CONNECT, the rest over the wire.
    let (mut read_end, mut write_end) = pair().await;
    write_end.write_all(&hello[10..]).await.unwrap();
    let read = read_hello(&mut read_end, hello[..10].to_vec())
        .await
        .unwrap();
    assert_eq!(read.bytes, hello);
    assert_eq!(
        read.asks,
        Some(Asks {
            name: Some(HOST.into()),
            protocols: Some(H1.iter().map(|p| p.to_vec()).collect()),
        })
    );

    let plain = b"GET / HTTP/1.1\r\nHost: api.test\r\n\r\n".to_vec();
    let (mut read_end, _write_end) = pair().await;
    let read = read_hello(&mut read_end, plain.clone()).await.unwrap();
    assert_eq!(read.bytes, plain);
    assert_eq!(read.asks, None);
}
