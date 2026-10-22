// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P3b-1b-1 — the egress proxy, end to end on loopback: a fake vault
//! decides, and a dialer that reaches one echo server stands in for the
//! system's (which would refuse a loopback address, which is the point).

use std::sync::atomic::{AtomicBool, Ordering};

use super::*;

/// A vault that answers by host: `allowed.example` allowed on every port,
/// `flaky.example` undecided at the first ask and allowed after, any other
/// host denied. Every ask is counted.
#[derive(Default)]
struct FakeVault {
    asks: Mutex<Vec<(String, u16)>>,
    flaky_answered: AtomicBool,
    delay_ms: u64,
}

impl FakeVault {
    fn asked(&self, host: &str) -> usize {
        lock(&self.asks).iter().filter(|(h, _)| h == host).count()
    }
}

#[async_trait::async_trait]
impl Decide for FakeVault {
    async fn decide(&self, _run: RunKey, host: &str, port: u16) -> Decision {
        lock(&self.asks).push((host.to_string(), port));
        if self.delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(self.delay_ms)).await;
        }
        match host {
            "allowed.example" => Decision::Allowed,
            "flaky.example" if self.flaky_answered.swap(true, Ordering::SeqCst) => {
                Decision::Allowed
            }
            "flaky.example" => Decision::Undecided("busy".into()),
            _ => Decision::Denied("denied".into()),
        }
    }
}

/// A dialer that reaches one echo server, whatever the host.
struct ToEcho(SocketAddr);

#[async_trait::async_trait]
impl Dial for ToEcho {
    async fn dial(&self, _host: &str, _port: u16) -> Result<TcpStream, Refusal> {
        TcpStream::connect(self.0)
            .await
            .map_err(|e| Refusal::Unreachable(e.to_string()))
    }
}

async fn echo() -> SocketAddr {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut tcp, _)) = listener.accept().await {
            tokio::spawn(async move {
                let (mut r, mut w) = tcp.split();
                let _ = tokio::io::copy(&mut r, &mut w).await;
            });
        }
    });
    addr
}

fn admit_all() -> Admit {
    Arc::new(|_: &TcpStream, _: SocketAddr| Ok(()))
}

fn run_key() -> RunKey {
    (ObjectId::new(), 1)
}

async fn serve(vault: Arc<FakeVault>) -> (RunKey, Fronts) {
    let run = run_key();
    let fronts = start_admitting(run, admit_all(), vault, Arc::new(ToEcho(echo().await))).unwrap();
    (run, fronts)
}

/// The status line of a response, read to its blank line.
async fn response_status(tcp: &mut TcpStream) -> String {
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

/// A CONNECT, with `early` right behind it: the status line, and the stream.
async fn http_connect(
    front: SocketAddr,
    target: &'static str,
    early: &'static [u8],
) -> (String, TcpStream) {
    let mut tcp = TcpStream::connect(front).await.unwrap();
    let mut request = format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n").into_bytes();
    request.extend_from_slice(early);
    tcp.write_all(&request).await.unwrap();
    let status = response_status(&mut tcp).await;
    (status, tcp)
}

/// A SOCKS5 CONNECT by name, with no authentication: the reply's code, and
/// the stream.
async fn socks_connect(front: SocketAddr, host: &str, port: u16) -> (u8, TcpStream) {
    let mut tcp = TcpStream::connect(front).await.unwrap();
    tcp.write_all(&[5, 1, 0]).await.unwrap();
    let mut selected = [0u8; 2];
    tcp.read_exact(&mut selected).await.unwrap();
    assert_eq!(selected, [5, 0], "no authentication");
    let mut request = vec![5, 1, 0, 3, u8::try_from(host.len()).unwrap()];
    request.extend_from_slice(host.as_bytes());
    request.extend_from_slice(&port.to_be_bytes());
    tcp.write_all(&request).await.unwrap();
    let mut reply = [0u8; 10];
    tcp.read_exact(&mut reply).await.unwrap();
    (reply[1], tcp)
}

async fn echoes(tcp: &mut TcpStream, bytes: &[u8]) {
    tcp.write_all(bytes).await.unwrap();
    let mut back = vec![0u8; bytes.len()];
    tokio::time::timeout(Duration::from_secs(5), tcp.read_exact(&mut back))
        .await
        .expect("an echo within 5 s")
        .unwrap();
    assert_eq!(back, bytes);
}

/// The proxy dials no address this host serves to itself alone, no
/// link-local one and no cloud metadata service; an IPv4 address written as
/// IPv6 is judged as itself.
#[test]
fn an_address_is_dialed_only_where_it_is_not_this_hosts_own() {
    for never in [
        "127.0.0.1",
        "127.1.2.3",
        "::1",
        "::ffff:127.0.0.1",
        "0.0.0.0",
        "0.1.2.3",
        "::",
        "169.254.169.254",
        "::ffff:169.254.169.254",
        "fe80::1",
        "febf::1",
        "224.0.0.1",
        "ff02::1",
        "255.255.255.255",
        "fd00:ec2::254",
    ] {
        assert!(!dialable(never.parse().unwrap()), "{never}");
    }
    for ok in [
        "10.0.0.5",
        "192.168.1.10",
        "100.64.0.7",
        "8.8.8.8",
        "2001:4860:4860::8888",
        "fd12:3456::1",
        "fec0::1",
    ] {
        assert!(dialable(ok.parse().unwrap()), "{ok}");
    }
}

/// A CONNECT names one host and one port; any other request is refused, a
/// plain-HTTP one as not CONNECT.
#[test]
fn a_connect_request_names_one_host_and_one_port() {
    let parse = |line: &str| parse_connect(format!("{line}\r\nHost: x\r\n\r\n").as_bytes());
    assert_eq!(
        parse("CONNECT pypi.org:443 HTTP/1.1"),
        Ok(("pypi.org".into(), 443))
    );
    assert_eq!(
        parse("CONNECT [2001:db8::1]:443 HTTP/1.1"),
        Ok(("2001:db8::1".into(), 443))
    );
    assert_eq!(
        parse("CONNECT 10.0.0.5:8443 HTTP/1.0"),
        Ok(("10.0.0.5".into(), 8443))
    );
    for line in [
        "GET http://pypi.org/simple HTTP/1.1",
        "connect pypi.org:443 HTTP/1.1",
    ] {
        assert!(matches!(parse(line), Err(Refusal::NotConnect(_))), "{line}");
    }
    for line in [
        "CONNECT pypi.org HTTP/1.1",
        "CONNECT pypi.org:0 HTTP/1.1",
        "CONNECT pypi.org:70000 HTTP/1.1",
        "CONNECT :443 HTTP/1.1",
        "CONNECT [2001:db8::1] HTTP/1.1",
        "CONNECT [::1]]:443 HTTP/1.1",
        "CONNECT pypi.org:443 HTTP/2",
        "CONNECT pypi.org:443",
        "CONNECT pypi.org:443 HTTP/1.1 more",
    ] {
        assert!(matches!(parse(line), Err(Refusal::BadRequest(_))), "{line}");
    }
}

/// An allow or a refusal the vault made is kept for the run; `busy`,
/// `unavailable`, `undecided`, no answer and an answer for something else
/// are not.
#[test]
fn a_vault_answer_is_kept_only_where_it_is_a_decision() {
    use crate::vault::LeaseError;
    assert_eq!(decision_of(Ok(())), Decision::Allowed);
    for word in [
        "denied",
        "no_role",
        "not_served",
        "ended",
        "a_word_from_a_newer_server",
    ] {
        assert_eq!(
            decision_of(Err(LeaseError::Refused(word.into()))),
            Decision::Denied(word.into()),
            "{word}"
        );
    }
    for word in ["busy", "unavailable", "undecided"] {
        assert_eq!(
            decision_of(Err(LeaseError::Refused(word.into()))),
            Decision::Undecided(word.into()),
            "{word}"
        );
    }
    for e in [
        LeaseError::NoAnswer,
        LeaseError::NotConnected,
        LeaseError::NotAsked,
        LeaseError::AwaitingApproval("a1".into()),
    ] {
        assert!(
            matches!(decision_of(Err(e.clone())), Decision::Undecided(_)),
            "{e:?}"
        );
    }
}

/// An allowed host is carried both ways, on either front, and what the
/// client sent behind its CONNECT reaches the host. The vault is asked once,
/// in the host's one spelling, whichever front asks and however the client
/// spells it.
#[tokio::test]
async fn an_allowed_host_is_carried_and_asked_about_once() {
    let vault = Arc::new(FakeVault::default());
    let (run, fronts) = serve(vault.clone()).await;
    let (status, mut tcp) = http_connect(fronts.http, "allowed.example:443", b"early").await;
    assert_eq!(status, "HTTP/1.1 200 Connection established");
    let mut early = [0u8; 5];
    tokio::time::timeout(Duration::from_secs(5), tcp.read_exact(&mut early))
        .await
        .expect("the early bytes, echoed")
        .unwrap();
    assert_eq!(
        &early, b"early",
        "the bytes behind the request reach the host"
    );
    echoes(&mut tcp, b"hello").await;

    let (reply, mut tcp) = socks_connect(fronts.socks, "ALLOWED.example.", 443).await;
    assert_eq!(reply, socks5::REP_SUCCESS);
    echoes(&mut tcp, b"again").await;
    assert_eq!(vault.asked("allowed.example"), 1, "kept for the run");
    assert!(
        lock(&vault.asks)
            .iter()
            .all(|(host, _)| host == "allowed.example"),
        "asked in its one spelling"
    );
    assert!(drop_run(run.0, run.1));
}

/// A denied host is refused on either front, and the denial kept for the
/// run; another port is another question. A request that got no decision is
/// refused, and the next connection asks again.
#[tokio::test]
async fn a_denial_is_kept_for_the_run_and_no_decision_is_not() {
    let vault = Arc::new(FakeVault::default());
    let (run, fronts) = serve(vault.clone()).await;
    let (status, _tcp) = http_connect(fronts.http, "denied.example:443", b"").await;
    assert_eq!(status, "HTTP/1.1 403 Forbidden");
    let (reply, _tcp) = socks_connect(fronts.socks, "denied.example", 443).await;
    assert_eq!(reply, REP_NOT_ALLOWED);
    assert_eq!(vault.asked("denied.example"), 1, "the denial is kept");
    let (status, _tcp) = http_connect(fronts.http, "denied.example:80", b"").await;
    assert_eq!(status, "HTTP/1.1 403 Forbidden");
    assert_eq!(
        vault.asked("denied.example"),
        2,
        "another port is asked about"
    );

    let (status, _tcp) = http_connect(fronts.http, "flaky.example:443", b"").await;
    assert_eq!(status, "HTTP/1.1 503 Service Unavailable");
    let (status, _tcp) = http_connect(fronts.http, "flaky.example:443", b"").await;
    assert_eq!(status, "HTTP/1.1 200 Connection established");
    assert_eq!(
        vault.asked("flaky.example"),
        2,
        "asked again after no decision"
    );
    assert!(drop_run(run.0, run.1));
}

/// Connections waiting on a host not yet decided wait for one ask.
#[tokio::test]
async fn connections_waiting_on_one_host_ask_the_vault_once() {
    let vault = Arc::new(FakeVault {
        delay_ms: 200,
        ..FakeVault::default()
    });
    let (run, fronts) = serve(vault.clone()).await;
    let waiting: Vec<_> = (0..8)
        .map(|_| tokio::spawn(http_connect(fronts.http, "allowed.example:443", b"")))
        .collect();
    for w in waiting {
        let (status, _tcp) = w.await.unwrap();
        assert_eq!(status, "HTTP/1.1 200 Connection established");
    }
    assert_eq!(vault.asked("allowed.example"), 1);
    assert!(drop_run(run.0, run.1));
}

/// A host no spelling decides and a request that is not CONNECT are refused
/// without asking the vault; a connection the admission refuses is closed
/// unanswered.
#[tokio::test]
async fn what_cannot_be_decided_is_never_asked() {
    let vault = Arc::new(FakeVault::default());
    let (run, fronts) = serve(vault.clone()).await;
    let (status, _tcp) = http_connect(fronts.http, "010.0.0.5:443", b"").await;
    assert_eq!(status, "HTTP/1.1 400 Bad Request");
    let mut tcp = TcpStream::connect(fronts.http).await.unwrap();
    tcp.write_all(b"GET http://allowed.example/ HTTP/1.1\r\nHost: allowed.example\r\n\r\n")
        .await
        .unwrap();
    assert_eq!(
        response_status(&mut tcp).await,
        "HTTP/1.1 405 Method Not Allowed"
    );
    assert!(lock(&vault.asks).is_empty());
    assert!(drop_run(run.0, run.1));

    let refusing: Admit = Arc::new(|_: &TcpStream, _: SocketAddr| Err("uid 4242".into()));
    let run = run_key();
    let fronts =
        start_admitting(run, refusing, vault.clone(), Arc::new(ToEcho(echo().await))).unwrap();
    let mut tcp = TcpStream::connect(fronts.http).await.unwrap();
    let _ = tcp
        .write_all(b"CONNECT allowed.example:443 HTTP/1.1\r\n\r\n")
        .await;
    let mut byte = [0u8; 1];
    let read = tokio::time::timeout(Duration::from_secs(5), tcp.read(&mut byte))
        .await
        .expect("closed within 5 s");
    assert_eq!(read.unwrap_or(0), 0, "closed, unanswered");
    assert!(
        lock(&vault.asks).is_empty(),
        "nothing asked for a connection that is not the account's"
    );
    assert!(drop_run(run.0, run.1));
}

/// When the run ends, its connections end and its fronts close.
#[tokio::test]
async fn the_run_ending_ends_its_fronts_and_its_connections() {
    let vault = Arc::new(FakeVault::default());
    let (run, fronts) = serve(vault.clone()).await;
    let (status, mut tcp) = http_connect(fronts.http, "allowed.example:443", b"").await;
    assert_eq!(status, "HTTP/1.1 200 Connection established");
    assert!(drop_run(run.0, run.1));
    assert!(!drop_run(run.0, run.1), "once");
    let mut byte = [0u8; 1];
    let read = tokio::time::timeout(Duration::from_secs(5), tcp.read(&mut byte))
        .await
        .expect("the connection ends");
    assert_eq!(read.unwrap_or(0), 0);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        TcpStream::connect(fronts.http).await.is_err(),
        "the front is closed"
    );
}

/// A run that asked about as many hosts as it may is refused a new one
/// unasked; a host it asked about keeps its decision.
#[tokio::test]
async fn a_run_asks_about_at_most_so_many_hosts() {
    let vault = FakeVault::default();
    let run = run_key();
    let state = RunState {
        stop: CancellationToken::new(),
        decisions: Mutex::default(),
        open: Arc::new(Semaphore::new(1)),
        ca: Arc::new(ca::SessionCa::new(run.0).unwrap()),
    };
    for i in 0..MAX_HOSTS {
        let _ = decided(&state, run, &format!("h{i}.example"), 443, &vault).await;
    }
    assert!(matches!(
        decided(&state, run, "one-more.example", 443, &vault).await,
        Decision::Undecided(_)
    ));
    assert_eq!(vault.asked("one-more.example"), 0);
    assert_eq!(
        decided(&state, run, "h0.example", 443, &vault).await,
        Decision::Denied("denied".into()),
        "kept"
    );
    assert_eq!(lock(&vault.asks).len(), MAX_HOSTS);
}

/// FR-90 P3b-3b — each run has a CA of its own from its start, and the CA
/// goes with its run.
#[tokio::test]
async fn a_run_has_its_own_ca_while_it_is_served() {
    let vault = Arc::new(FakeVault::default());
    let (a, _) = serve(vault.clone()).await;
    let (b, _) = serve(vault).await;
    let (ca_a, ca_b) = (ca_of(a.0, a.1).unwrap(), ca_of(b.0, b.1).unwrap());
    assert_ne!(ca_a.der(), ca_b.der(), "each run its own");
    assert!(drop_run(a.0, a.1));
    assert!(ca_of(a.0, a.1).is_none(), "gone with its run");
    assert!(ca_of(b.0, b.1).is_some(), "the other's stays");
    assert!(drop_run(b.0, b.1));
}
