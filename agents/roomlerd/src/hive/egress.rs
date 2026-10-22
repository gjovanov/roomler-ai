// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P3b-1b-1 — a session's egress proxy: the two fronts Claude Code's
//! sandbox sends a session's commands through (`sandbox.network.httpProxyPort`
//! and `socksProxyPort`), each on loopback, each connection served only to the
//! session's account (the check `vault_connect`'s endpoints make). The design
//! is the spec's §3e, "P3b, `placeholder` egress, as designed"; the allowlist
//! is decision 27, the vault's `net:egress` (`docs/vault.md` §24).
//!
//! | Step | Rule |
//! |---|---|
//! | the front | HTTP `CONNECT host:port`, or a SOCKS5 `CONNECT` with no authentication. Nothing else: no plain-HTTP request, no UDP |
//! | the host | in its one spelling (`canonical_net_host`); one it cannot spell is refused unasked |
//! | the decision | the vault's, asked once per host, port and run (`rc:vault.egress`) and kept for the run, an allow or a denial. A request that got no decision (no answer, busy, unavailable) is refused, and the next connection asks again |
//! | the dial | from this device, never through a tunnel exit: the name resolved by the OS's resolver, which MagicDNS steers, and never to an address of this host's loopback, a link-local, an unspecified, a multicast or a broadcast one, nor a cloud's metadata service, whatever the vault allowed |
//! | the bytes | relayed as they are: no TLS is terminated here (P3b-3 adds it, for an audience host of a placeholder) |
//! | its CA | made with the run (P3b-3b, [`ca`]): what P3b-3c terminates TLS under, and what the session's tools are told to trust ([`ca_of`]) |
//! | its end | the run's ([`drop_run`]): both listeners close, and every connection on them ends |
//!
//! ⚠️ A front admits the session's account and nobody else. On Linux the
//! sandbox's bridge, running as that account, is what connects; a command
//! inside the sandbox reaches no other port of this host's loopback.
//!
//! ⚠️ An empty allowlist refuses everything: with no vault to ask, no host
//! is reached.
//!
//! ⚠️ The proxy dials as the daemon, SYSTEM or root, so it never dials this
//! host's loopback: a session must not reach, as the daemon, what this host
//! serves only to itself. Nor a link-local address, where a cloud's metadata
//! service answers.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
use std::time::Duration;

use bson::oid::ObjectId;
use roomler_ai_remote_control::vault::{LeaseRefusal, canonical_net_host};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OnceCell, Semaphore};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use tunnel_core::flow_listener::Admit;
use tunnel_core::socks5;

/// FR-90 P3b-3b — the run's own CA, and the trust the session is told of.
pub mod ca;

/// A session's run, by its session and fence.
pub type RunKey = (ObjectId, u64);

/// The account a front serves: a uid on Unix, a SID on Windows.
#[cfg(unix)]
pub type Peer = u32;
#[cfg(windows)]
pub type Peer = String;

/// The most hosts, each with its port, one run asks about. Past it a new one
/// is refused unasked, so no run makes the vault decide without end.
pub const MAX_HOSTS: usize = 1024;
/// The most connections one run has open at once.
pub const MAX_CONNECTIONS: usize = 256;
/// How long a client has to say where it goes.
pub const HANDSHAKE_SECS: u64 = 10;
/// How long a dial may take, every address together.
pub const DIAL_SECS: u64 = 10;
/// The longest request head the HTTP front reads.
pub const MAX_HEAD: usize = 8 * 1024;

/// AWS's instance metadata service on IPv6: a unique-local address, so no
/// rule of the address space refuses it, and the same service the link-local
/// `169.254.169.254` is.
const AWS_METADATA_V6: Ipv6Addr = Ipv6Addr::new(0xfd00, 0x0ec2, 0, 0, 0, 0, 0, 0x0254);

// SOCKS5 replies beyond the two `tunnel_core::socks5` names (RFC 1928 §6).
const REP_NOT_ALLOWED: u8 = 0x02;
const REP_HOST_UNREACHABLE: u8 = 0x04;
const REP_REFUSED: u8 = 0x05;

/// What the vault answered about one host for one run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Allowed; kept for the run.
    Allowed,
    /// Refused by the vault's decision, in its word; kept for the run.
    Denied(String),
    /// No decision: no answer, or one that says to ask again. Never kept.
    Undecided(String),
}

/// What decides a host for a run: the vault, in the daemon ([`VaultDecider`]).
#[async_trait::async_trait]
pub trait Decide: Send + Sync + 'static {
    /// `host` in its one spelling.
    async fn decide(&self, run: RunKey, host: &str, port: u16) -> Decision;
}

/// What dials a host the vault allowed: the system, in the daemon
/// ([`SystemDialer`]).
#[async_trait::async_trait]
pub trait Dial: Send + Sync + 'static {
    /// `host` in its one spelling, already allowed.
    async fn dial(&self, host: &str, port: u16) -> Result<TcpStream, Refusal>;
}

/// Why a connection was not carried: what its client is told, and what the
/// log says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The front could not read where the client goes, or named a host no
    /// spelling decides.
    BadRequest(String),
    /// The HTTP front was asked for something other than `CONNECT`.
    NotConnect(String),
    /// The vault refused this host for this run, in its word.
    Denied(String),
    /// No decision now; the next connection asks again.
    Undecided(String),
    /// The run asked about too many hosts, or has too many connections open.
    Busy(&'static str),
    /// The name resolves to no address this proxy dials.
    NotDialable(String),
    /// The name did not resolve, or no address answered.
    Unreachable(String),
    /// An address answered, refusing the connection.
    ConnectionRefused(String),
}

impl Refusal {
    /// The HTTP front's status for it.
    fn status(&self) -> (u16, &'static str) {
        match self {
            Self::BadRequest(_) => (400, "Bad Request"),
            Self::NotConnect(_) => (405, "Method Not Allowed"),
            Self::Denied(_) | Self::NotDialable(_) => (403, "Forbidden"),
            Self::Undecided(_) | Self::Busy(_) => (503, "Service Unavailable"),
            Self::Unreachable(_) | Self::ConnectionRefused(_) => (502, "Bad Gateway"),
        }
    }

    /// The SOCKS5 front's reply for it.
    fn socks(&self) -> u8 {
        match self {
            Self::Denied(_) | Self::NotDialable(_) => REP_NOT_ALLOWED,
            Self::Unreachable(_) => REP_HOST_UNREACHABLE,
            Self::ConnectionRefused(_) => REP_REFUSED,
            Self::BadRequest(_) | Self::NotConnect(_) | Self::Undecided(_) | Self::Busy(_) => {
                socks5::REP_GENERAL_FAILURE
            }
        }
    }

    /// What the HTTP front's client reads: enough for a session to know what
    /// to ask for, nothing of the device.
    fn body(&self, target: &str) -> String {
        match self {
            Self::BadRequest(why) => {
                format!("roomler egress proxy: a request it cannot read ({why})\n")
            }
            Self::NotConnect(method) => format!(
                "roomler egress proxy: only CONNECT is served, not {method}: use an https URL\n"
            ),
            Self::Denied(word) => format!(
                "roomler egress proxy: the organization's vault does not allow {target} for this \
                 session ({word}). An administrator allows a host with a net:egress policy for \
                 the session's role.\n"
            ),
            Self::Undecided(why) => format!(
                "roomler egress proxy: the vault did not decide {target} ({why}); try again\n"
            ),
            Self::Busy(why) => format!("roomler egress proxy: {why}\n"),
            Self::NotDialable(_) => format!(
                "roomler egress proxy: {target} resolves only to addresses this proxy never dials\n"
            ),
            Self::Unreachable(_) | Self::ConnectionRefused(_) => {
                format!("roomler egress proxy: {target} could not be reached\n")
            }
        }
    }
}

/// Where a run's fronts listen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fronts {
    pub http: SocketAddr,
    pub socks: SocketAddr,
}

/// The two fronts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Front {
    Http,
    Socks,
}

/// A host, in its one spelling, and its port: what a run's decisions are
/// kept by.
type HostPort = (String, u16);

/// One host's decision for a run: set once the vault decides, and left empty
/// when it did not, so the next connection asks again.
type Kept = Arc<OnceCell<Decision>>;

/// One run's proxy: its end, its decisions, its open connections, its CA.
struct RunState {
    stop: CancellationToken,
    /// Each host and port asked about.
    decisions: Mutex<HashMap<HostPort, Kept>>,
    open: Arc<Semaphore>,
    /// FR-90 P3b-3b — made with the run, gone with it.
    ca: Arc<ca::SessionCa>,
}

/// Every run's proxy: ended with the run ([`drop_run`]).
static RUNS: LazyLock<Mutex<HashMap<RunKey, Arc<RunState>>>> = LazyLock::new(Mutex::default);

fn runs() -> MutexGuard<'static, HashMap<RunKey, Arc<RunState>>> {
    RUNS.lock().unwrap_or_else(|e| e.into_inner())
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Serve `run`'s egress: two loopback listeners at ports the system picks,
/// admitting `peer`'s processes alone, deciding each host with `decide` and
/// dialing with `dial`. A run already served is stopped first: one run, one
/// pair of fronts. Refused where this system cannot say who connects to a
/// loopback port.
///
/// Not `async`: a session's launch, which starts it, is not. It must be
/// called within the daemon's runtime, which serves the fronts.
pub fn start(
    run: RunKey,
    peer: Peer,
    decide: Arc<dyn Decide>,
    dial: Arc<dyn Dial>,
) -> Result<Fronts, String> {
    let admit = super::vault_connect::session_admit(peer)?;
    start_admitting(run, admit, decide, dial)
}

/// A loopback listener at a port the system picks, for the runtime.
fn loopback(front: &str) -> Result<TcpListener, String> {
    let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .and_then(|l| l.set_nonblocking(true).map(|()| l))
        .map_err(|e| format!("the egress proxy's {front} front: {e}"))?;
    TcpListener::from_std(listener).map_err(|e| format!("the egress proxy's {front} front: {e}"))
}

/// [`start`], with the admission given.
fn start_admitting(
    run: RunKey,
    admit: Admit,
    decide: Arc<dyn Decide>,
    dial: Arc<dyn Dial>,
) -> Result<Fronts, String> {
    let http = loopback("HTTP")?;
    let socks = loopback("SOCKS5")?;
    let fronts = Fronts {
        http: http
            .local_addr()
            .map_err(|e| format!("the egress proxy's HTTP front: {e}"))?,
        socks: socks
            .local_addr()
            .map_err(|e| format!("the egress proxy's SOCKS5 front: {e}"))?,
    };
    let state = Arc::new(RunState {
        stop: CancellationToken::new(),
        decisions: Mutex::default(),
        open: Arc::new(Semaphore::new(MAX_CONNECTIONS)),
        ca: Arc::new(ca::SessionCa::new(run.0)?),
    });
    if let Some(old) = runs().insert(run, state.clone()) {
        old.stop.cancel();
    }
    for (listener, front) in [(http, Front::Http), (socks, Front::Socks)] {
        tokio::spawn(accept(
            listener,
            front,
            run,
            state.clone(),
            admit.clone(),
            decide.clone(),
            dial.clone(),
        ));
    }
    info!(
        session = %run.0, fence = run.1, http = %fronts.http, socks = %fronts.socks,
        "hive: the session's egress proxy is serving"
    );
    Ok(fronts)
}

/// FR-90 P3b-3b — the CA of `run`'s proxy, while it is served.
pub fn ca_of(session: ObjectId, fence: u64) -> Option<Arc<ca::SessionCa>> {
    runs()
        .get(&(session, fence))
        .map(|state| Arc::clone(&state.ca))
}

/// Stop serving `run`: both fronts close and every connection on them ends.
/// Answers whether it was served.
pub fn drop_run(session: ObjectId, fence: u64) -> bool {
    match runs().remove(&(session, fence)) {
        Some(state) => {
            state.stop.cancel();
            info!(session = %session, fence, "hive: the session's egress proxy stopped");
            true
        }
        None => false,
    }
}

/// One front's listener, until the run ends.
async fn accept(
    listener: TcpListener,
    front: Front,
    run: RunKey,
    state: Arc<RunState>,
    admit: Admit,
    decide: Arc<dyn Decide>,
    dial: Arc<dyn Dial>,
) {
    let Ok(local) = listener.local_addr() else {
        return;
    };
    loop {
        let accepted = tokio::select! {
            accepted = listener.accept() => accepted,
            () = state.stop.cancelled() => break,
        };
        let tcp = match accepted {
            Ok((tcp, _)) => tcp,
            Err(e) => {
                warn!(session = %run.0, ?front, %e, "hive: the egress proxy could not accept");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        if let Err(who) = admit(&tcp, local) {
            debug!(
                session = %run.0, ?front, %who,
                "hive: an egress connection that is not the session's account — closed"
            );
            continue;
        }
        tokio::spawn(carry(
            front,
            tcp,
            run,
            state.clone(),
            decide.clone(),
            dial.clone(),
        ));
    }
}

/// One connection: where it goes, whether it may, and the bytes.
async fn carry(
    front: Front,
    mut tcp: TcpStream,
    run: RunKey,
    state: Arc<RunState>,
    decide: Arc<dyn Decide>,
    dial: Arc<dyn Dial>,
) {
    let Ok(_open) = state.open.clone().try_acquire_owned() else {
        refuse(
            front,
            &mut tcp,
            "",
            &Refusal::Busy("too many connections open for this session"),
        )
        .await;
        return;
    };
    let asked =
        tokio::time::timeout(Duration::from_secs(HANDSHAKE_SECS), target(front, &mut tcp)).await;
    let (asked_host, port, early) = match asked {
        Ok(Ok(target)) => target,
        Ok(Err(refusal)) => {
            debug!(session = %run.0, ?front, ?refusal, "hive: an egress request it could not read");
            refuse(front, &mut tcp, "", &refusal).await;
            return;
        }
        Err(_) => {
            debug!(session = %run.0, ?front, "hive: an egress request that never said where it goes");
            return;
        }
    };
    let shown = target_text(&asked_host, port);
    let reached = reach(&state, run, &asked_host, port, &*decide, &*dial).await;
    let upstream = match reached {
        Ok(upstream) => upstream,
        Err(refusal) => {
            match &refusal {
                Refusal::Denied(word) => info!(
                    session = %run.0, fence = run.1, target = %shown, word = %word,
                    "hive: egress refused — the vault's policies do not allow it"
                ),
                Refusal::NotDialable(why) => warn!(
                    session = %run.0, fence = run.1, target = %shown, why = %why,
                    "hive: egress refused — an address this proxy never dials"
                ),
                other => info!(
                    session = %run.0, fence = run.1, target = %shown, refusal = ?other,
                    "hive: egress refused"
                ),
            }
            refuse(front, &mut tcp, &shown, &refusal).await;
            return;
        }
    };
    if !accepted(front, &mut tcp).await {
        return;
    }
    debug!(session = %run.0, fence = run.1, target = %shown, ?front, "hive: egress carried");
    let _ = relay(tcp, upstream, early, state.stop.clone()).await;
}

/// From the host the client named to a connection to it: its one spelling,
/// the run's decision, the dial.
async fn reach(
    state: &RunState,
    run: RunKey,
    asked_host: &str,
    port: u16,
    decide: &dyn Decide,
    dial: &dyn Dial,
) -> Result<TcpStream, Refusal> {
    let Some(host) = canonical_net_host(asked_host) else {
        return Err(Refusal::BadRequest(format!(
            "{asked_host:?} is not a host this proxy can decide"
        )));
    };
    match decided(state, run, &host, port, decide).await {
        Decision::Allowed => {}
        Decision::Denied(word) => return Err(Refusal::Denied(word)),
        Decision::Undecided(why) => return Err(Refusal::Undecided(why)),
    }
    match tokio::time::timeout(Duration::from_secs(DIAL_SECS), dial.dial(&host, port)).await {
        Ok(dialed) => dialed,
        Err(_) => Err(Refusal::Unreachable(format!(
            "no answer within {DIAL_SECS} s"
        ))),
    }
}

/// The run's decision for `host:port`: the one it keeps, or the vault's,
/// asked once however many connections wait for it. An answer that is no
/// decision is not kept.
async fn decided(
    state: &RunState,
    run: RunKey,
    host: &str,
    port: u16,
    decide: &dyn Decide,
) -> Decision {
    let cell = {
        let mut decisions = lock(&state.decisions);
        match decisions.get(&(host.to_string(), port)) {
            Some(cell) => cell.clone(),
            None if decisions.len() >= MAX_HOSTS => {
                return Decision::Undecided(format!(
                    "this session asked about more than {MAX_HOSTS} hosts"
                ));
            }
            None => {
                let cell = Arc::new(OnceCell::new());
                decisions.insert((host.to_string(), port), cell.clone());
                cell
            }
        }
    };
    let kept = cell
        .get_or_try_init(|| async {
            match decide.decide(run, host, port).await {
                Decision::Undecided(why) => Err(why),
                decision => Ok(decision),
            }
        })
        .await;
    match kept {
        Ok(decision) => decision.clone(),
        Err(why) => Decision::Undecided(why),
    }
}

/// Where the client goes, and (HTTP) what it sent past its request.
async fn target(front: Front, tcp: &mut TcpStream) -> Result<(String, u16, Vec<u8>), Refusal> {
    match front {
        // `accept_connect` answers a protocol error itself, a UDP request
        // included.
        Front::Socks => socks5::accept_connect(tcp)
            .await
            .map(|(host, port)| (host, port, Vec::new()))
            .map_err(|e| Refusal::BadRequest(e.to_string())),
        Front::Http => {
            let (head, early) = read_head(tcp).await?;
            let (host, port) = parse_connect(&head)?;
            Ok((host, port, early))
        }
    }
}

/// A request head, to its blank line, and the bytes the same reads brought
/// past it. At most [`MAX_HEAD`] bytes.
async fn read_head(tcp: &mut TcpStream) -> Result<(Vec<u8>, Vec<u8>), Refusal> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        let n = tcp
            .read(&mut chunk)
            .await
            .map_err(|e| Refusal::BadRequest(e.to_string()))?;
        if n == 0 {
            return Err(Refusal::BadRequest(
                "the client closed before its request ended".into(),
            ));
        }
        // The blank line can straddle two reads: look from three bytes back.
        let from = buf.len().saturating_sub(3);
        buf.extend_from_slice(&chunk[..n]);
        if let Some(at) = buf[from..].windows(4).position(|w| w == b"\r\n\r\n") {
            let end = from + at + 4;
            let early = buf.split_off(end);
            return Ok((buf, early));
        }
        if buf.len() > MAX_HEAD {
            return Err(Refusal::BadRequest(format!(
                "a request head past {MAX_HEAD} bytes"
            )));
        }
    }
}

/// `CONNECT host:port HTTP/1.x`: the host as named (an IPv6 address without
/// its brackets) and the port. Headers are not read: nothing in them changes
/// where the connection goes.
pub fn parse_connect(head: &[u8]) -> Result<(String, u16), Refusal> {
    let head = std::str::from_utf8(head)
        .map_err(|_| Refusal::BadRequest("a request line that is not UTF-8".into()))?;
    let line = head.split("\r\n").next().unwrap_or_default();
    let mut words = line.split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (words.next(), words.next(), words.next(), words.next())
    else {
        return Err(Refusal::BadRequest("a request line of three words".into()));
    };
    if !version.starts_with("HTTP/1.") {
        return Err(Refusal::BadRequest(format!("{version:?} is not HTTP/1")));
    }
    if method != "CONNECT" {
        return Err(Refusal::NotConnect(method.chars().take(16).collect()));
    }
    let (host, port) = match target.strip_prefix('[') {
        Some(rest) => {
            let (host, port) = rest
                .split_once("]:")
                .ok_or_else(|| Refusal::BadRequest("a bracketed address with no port".into()))?;
            (host, port)
        }
        None => target
            .rsplit_once(':')
            .ok_or_else(|| Refusal::BadRequest("a target with no port".into()))?,
    };
    let port: u16 = port
        .parse()
        .ok()
        .filter(|p| *p != 0)
        .ok_or_else(|| Refusal::BadRequest(format!("{port:?} is not a port")))?;
    if host.is_empty() || host.contains(['[', ']']) {
        return Err(Refusal::BadRequest(format!("{host:?} is not a host")));
    }
    Ok((host.to_string(), port))
}

/// How a target is named in a log line and an answer: an IPv6 address in
/// brackets, so its port stays its own.
fn target_text(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// Tell the client its connection is through.
async fn accepted(front: Front, tcp: &mut TcpStream) -> bool {
    match front {
        Front::Http => tcp
            .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
            .await
            .is_ok(),
        Front::Socks => {
            socks5::reply(tcp, socks5::REP_SUCCESS).await;
            true
        }
    }
}

/// Tell the client why not, as its front speaks.
async fn refuse(front: Front, tcp: &mut TcpStream, target: &str, refusal: &Refusal) {
    match front {
        Front::Http => {
            let (code, reason) = refusal.status();
            let body = refusal.body(target);
            let answer = format!(
                "HTTP/1.1 {code} {reason}\r\nContent-Type: text/plain; charset=utf-8\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = tcp.write_all(answer.as_bytes()).await;
            let _ = tcp.shutdown().await;
        }
        // A request it could not read was answered by `accept_connect`.
        Front::Socks if matches!(refusal, Refusal::BadRequest(_)) => {}
        Front::Socks => socks5::reply(tcp, refusal.socks()).await,
    }
}

/// The bytes both ways, until either side closes or the run ends.
async fn relay(
    mut client: TcpStream,
    mut upstream: TcpStream,
    early: Vec<u8>,
    stop: CancellationToken,
) -> std::io::Result<()> {
    if !early.is_empty() {
        upstream.write_all(&early).await?;
    }
    tokio::select! {
        relayed = tokio::io::copy_bidirectional(&mut client, &mut upstream) => relayed.map(|_| ()),
        () = stop.cancelled() => Ok(()),
    }
}

/// Whether the proxy dials `ip`. Never this host's loopback, a link-local
/// address, an unspecified, a multicast or a broadcast one, `0.0.0.0/8` (which
/// Linux connects to this host), nor AWS's IPv6 metadata service; an IPv4
/// address written as IPv6 is judged as itself.
pub fn dialable(ip: IpAddr) -> bool {
    match ip.to_canonical() {
        IpAddr::V4(v4) => {
            !(v4.is_loopback()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_multicast()
                || v4.is_broadcast()
                || v4.octets()[0] == 0)
        }
        IpAddr::V6(v6) => {
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (v6.segments()[0] & 0xffc0) == 0xfe80
                || v6 == AWS_METADATA_V6)
        }
    }
}

/// The daemon's dialer: the OS's resolver (which MagicDNS steers), then the
/// first dialable address that answers.
pub struct SystemDialer;

#[async_trait::async_trait]
impl Dial for SystemDialer {
    async fn dial(&self, host: &str, port: u16) -> Result<TcpStream, Refusal> {
        let addrs: Vec<SocketAddr> = match host.parse::<IpAddr>() {
            Ok(ip) => vec![SocketAddr::new(ip, port)],
            Err(_) => tokio::net::lookup_host((host, port))
                .await
                .map_err(|e| Refusal::Unreachable(format!("{host} did not resolve: {e}")))?
                .collect(),
        };
        let (ok, never): (Vec<SocketAddr>, Vec<SocketAddr>) =
            addrs.into_iter().partition(|a| dialable(a.ip()));
        if ok.is_empty() {
            return Err(match never.first() {
                Some(a) => Refusal::NotDialable(format!("{host} resolves to {}", a.ip())),
                None => Refusal::Unreachable(format!("{host} resolved to no address")),
            });
        }
        let mut last = None;
        for addr in ok {
            match TcpStream::connect(addr).await {
                Ok(tcp) => return Ok(tcp),
                Err(e) => last = Some((addr, e)),
            }
        }
        let (addr, e) = last.expect("one address was tried");
        Err(if e.kind() == std::io::ErrorKind::ConnectionRefused {
            Refusal::ConnectionRefused(format!("{addr}: {e}"))
        } else {
            Refusal::Unreachable(format!("{addr}: {e}"))
        })
    }
}

/// The daemon's decider: its vault client (`rc:vault.egress`).
pub struct VaultDecider;

#[async_trait::async_trait]
impl Decide for VaultDecider {
    async fn decide(&self, (session_id, fence): RunKey, host: &str, port: u16) -> Decision {
        decision_of(
            crate::vault::egress(crate::vault::EgressAsk {
                session_id,
                fence,
                host: host.to_string(),
                port,
            })
            .await,
        )
    }
}

/// The vault's answer as a decision. An allow, or a refusal the vault made,
/// is kept for the run; `busy`, `unavailable` and `undecided`, no answer, or
/// an answer for something else, are not.
pub fn decision_of(asked: Result<(), crate::vault::LeaseError>) -> Decision {
    use crate::vault::LeaseError;
    match asked {
        Ok(()) => Decision::Allowed,
        Err(LeaseError::Refused(word)) => {
            let again = [
                LeaseRefusal::Busy,
                LeaseRefusal::Unavailable,
                LeaseRefusal::Undecided,
            ]
            .iter()
            .any(|r| r.wire() == word);
            if again {
                Decision::Undecided(word)
            } else {
                Decision::Denied(word)
            }
        }
        Err(LeaseError::AwaitingApproval(_)) => {
            Decision::Undecided(LeaseRefusal::AwaitingApproval.wire().to_string())
        }
        Err(other) => Decision::Undecided(format!("{other:?}")),
    }
}

#[cfg(test)]
mod tests;
