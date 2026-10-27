// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P4-5-1b — a knowhow probe, the device's half (the FR-90 spec, §3e
//! "P4-5, verification, as designed"; `docs/knowhow.md` §13).
//!
//! The server asks this device (`rc:knowhow.probe`) to reach one host and
//! port the map says, as `tcp` or `tls`. The device decides whether it may
//! (decision 53, its owner's `knowhow_probe_reach`), connects and never
//! converses (decision 55), and answers `rc:knowhow.probed` once.
//!
//! ⚠️ **Resolve, check, then connect to what was checked.** A name is
//! resolved here, every address it resolves to is held to the never-probed
//! rule and the list, and a connect only ever goes to an address that
//! passed, so a name that resolves elsewhere by the time of the connect
//! reaches nothing new. An answer that holds a never-probed address at all
//! refuses the probe whole: a name pointing at a metadata service is not one
//! to reach through its other addresses.
//!
//! ⚠️ **Connect, never converse.** `tcp`: a connect and a close, not a byte
//! sent. `tls`: a ClientHello, the server's certificate read and its
//! handshake signature checked (so the server is shown to hold that
//! certificate's key), the chain neither trusted nor refused; then a close.
//! No request, no client certificate, no credential, ten seconds at most.

use std::collections::VecDeque;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bson::oid::ObjectId;
use roomler_ai_remote_control::knowhow::{
    MAX_DETAIL, ProbeKind, ProbeOutcome, ProbeTls, never_probed, probe_host, probe_limits,
};
use roomler_ai_remote_control::models::DestinationRule;
use roomler_ai_remote_control::signaling::ClientMsg;
use roomler_node_core::probe_reach;
use sha2::{Digest, Sha256};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_rustls::rustls;
use tracing::{info, warn};

/// Answers kept, so a probe delivered again is answered as it was, never run
/// twice.
const ANSWERS_KEPT: usize = 16;

/// What the server asked.
#[derive(Debug, Clone)]
pub struct Asked {
    pub id: ObjectId,
    pub kind: String,
    pub host: String,
    pub port: u16,
    pub sni: Option<String>,
}

/// The primary's prober: `knowhow_probe_reach` read from `config` under
/// `lock` at each probe, each probe run once.
pub struct Prober {
    config: PathBuf,
    lock: crate::config::WriteLock,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// The probes being run, each with the connection to answer it on: the
    /// newest that delivered it.
    running: Vec<(ObjectId, mpsc::Sender<ClientMsg>)>,
    answered: VecDeque<(ObjectId, ClientMsg)>,
}

impl Prober {
    /// The daemon's: the list read from `config` under `lock` (the
    /// LocalAPI's own, so a probe never reads a half-written file).
    pub fn new(config: PathBuf, lock: crate::config::WriteLock) -> Arc<Self> {
        Arc::new(Self {
            config,
            lock,
            state: Mutex::new(State::default()),
        })
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A probe asked: answered again from what it answered, joined while it
    /// runs (the newest connection hears it), or run.
    pub fn request(self: Arc<Self>, asked: Asked, tx: mpsc::Sender<ClientMsg>) {
        let mut st = self.state();
        if let Some((_, answer)) = st.answered.iter().find(|(id, _)| *id == asked.id) {
            let answer = answer.clone();
            drop(st);
            info!(probe = %asked.id, "knowhow: a probe delivered again, answered as it was");
            send(tx, answer);
            return;
        }
        if let Some(slot) = st.running.iter_mut().find(|(id, _)| *id == asked.id) {
            slot.1 = tx;
            return;
        }
        st.running.push((asked.id, tx));
        drop(st);
        tokio::spawn(async move {
            let answer = self.run(&asked).await;
            let tx = {
                let mut st = self.state();
                let at = st.running.iter().position(|(id, _)| *id == asked.id);
                let tx = at.map(|i| st.running.swap_remove(i).1);
                st.answered.push_back((asked.id, answer.clone()));
                while st.answered.len() > ANSWERS_KEPT {
                    st.answered.pop_front();
                }
                tx
            };
            // A connection that died meanwhile drops the answer; the server
            // delivers the probe again at the next hello, and it is answered
            // from `answered`.
            if let Some(tx) = tx {
                let _ = tx.send(answer).await;
            }
        });
    }

    /// One probe: the list read from the config file now, then the probe.
    async fn run(&self, asked: &Asked) -> ClientMsg {
        let cfg = match self.read_config().await {
            Ok(cfg) => cfg,
            Err(e) => {
                warn!(probe = %asked.id, error = %e, "knowhow: the config could not be read");
                return reply(
                    asked.id,
                    ProbeOutcome::Failed,
                    None,
                    None,
                    None,
                    Some(format!("reading its config: {e}")),
                );
            }
        };
        let rules = probe_reach::rules(&cfg.knowhow_probe_reach);
        let answer = measure(asked, &rules).await;
        if let ClientMsg::KnowhowProbed {
            outcome,
            rtt_ms,
            address,
            ..
        } = &answer
        {
            info!(
                probe = %asked.id, kind = %asked.kind, host = %asked.host, port = asked.port,
                outcome = outcome.as_str(), ?rtt_ms, ?address, "knowhow: a probe ran"
            );
        }
        answer
    }

    async fn read_config(&self) -> Result<crate::config::AgentConfig, String> {
        let _guard = self.lock.lock().await;
        let path = self.config.clone();
        tokio::task::spawn_blocking(move || {
            crate::config::load(&path).map_err(|e| format!("{e:#}"))
        })
        .await
        .map_err(|e| format!("the read's task: {e}"))?
    }
}

/// `rc:knowhow.probe`, taken on a connection: on the primary's alone, where
/// the list is its owner's (decision 43's rule), else refused with a word,
/// never left unanswered.
pub fn handle(
    prober: Option<&Arc<Prober>>,
    asked: Asked,
    is_primary: bool,
    tx: mpsc::Sender<ClientMsg>,
) {
    if !is_primary {
        warn!(probe = %asked.id, "rc:knowhow.probe refused — not the primary enrollment");
        let why = "this device probes for its primary organization only";
        send(
            tx,
            reply(
                asked.id,
                ProbeOutcome::NotAllowed,
                None,
                None,
                None,
                Some(why.into()),
            ),
        );
        return;
    }
    let Some(prober) = prober else {
        warn!(probe = %asked.id, "rc:knowhow.probe refused — this daemon reads no config file");
        let why = "this daemon has no config file to read where a probe may go";
        send(
            tx,
            reply(
                asked.id,
                ProbeOutcome::Failed,
                None,
                None,
                None,
                Some(why.into()),
            ),
        );
        return;
    };
    Arc::clone(prober).request(asked, tx);
}

fn send(tx: mpsc::Sender<ClientMsg>, msg: ClientMsg) {
    tokio::spawn(async move {
        let _ = tx.send(msg).await;
    });
}

fn clamp(detail: String) -> String {
    detail
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_DETAIL)
        .collect()
}

fn reply(
    id: ObjectId,
    outcome: ProbeOutcome,
    rtt_ms: Option<u32>,
    address: Option<IpAddr>,
    tls: Option<ProbeTls>,
    detail: Option<String>,
) -> ClientMsg {
    ClientMsg::KnowhowProbed {
        probe_id: id,
        outcome,
        rtt_ms,
        address: address.map(|a| a.to_string()),
        tls: tls.and_then(|t| serde_json::to_value(t).ok()),
        detail: detail.map(clamp),
    }
}

fn left(deadline: Instant) -> Duration {
    deadline.saturating_duration_since(Instant::now())
}

fn ms(d: Duration) -> u32 {
    u32::try_from(d.as_millis()).unwrap_or(u32::MAX)
}

/// One probe within `rules`, pure of the config: resolve, check, connect to
/// what was checked, and for `tls` the handshake. Its answer.
pub(crate) async fn measure(asked: &Asked, rules: &[DestinationRule]) -> ClientMsg {
    let id = asked.id;
    let no = |outcome, why: String| reply(id, outcome, None, None, None, Some(why));
    let deadline = Instant::now() + Duration::from_secs(probe_limits::TIMEOUT_SECS);
    let Some(kind) = ProbeKind::from_word(&asked.kind) else {
        return no(
            ProbeOutcome::Failed,
            format!("this build does not probe {:?}", asked.kind),
        );
    };
    let Some(host) = probe_host(&asked.host) else {
        return no(
            ProbeOutcome::Failed,
            "the server named no host a probe reaches".into(),
        );
    };
    if asked.port == 0 {
        return no(ProbeOutcome::Failed, "the server named no port".into());
    }
    if rules.is_empty() {
        return no(
            ProbeOutcome::NotAllowed,
            "its owner lists nowhere a probe may reach (knowhow_probe_reach)".into(),
        );
    }
    // Resolve.
    let literal = host.parse::<IpAddr>().ok();
    let name = literal.is_none().then(|| host.clone());
    let addrs: Vec<IpAddr> = match literal {
        Some(ip) => vec![ip],
        None => {
            let lookup = tokio::net::lookup_host((host.as_str(), asked.port));
            match tokio::time::timeout(left(deadline), lookup).await {
                Ok(Ok(found)) => {
                    let mut ips: Vec<IpAddr> = Vec::new();
                    for sa in found {
                        if !ips.contains(&sa.ip()) {
                            ips.push(sa.ip());
                        }
                    }
                    ips
                }
                Ok(Err(e)) => {
                    return no(
                        ProbeOutcome::Unreachable,
                        format!("the name did not resolve: {e}"),
                    );
                }
                Err(_) => {
                    return no(
                        ProbeOutcome::Unreachable,
                        "the name did not resolve in time".into(),
                    );
                }
            }
        }
    };
    if addrs.is_empty() {
        return no(
            ProbeOutcome::Unreachable,
            "the name resolved to no address".into(),
        );
    }
    // Check every address before any connect.
    if let Some(bad) = addrs.iter().find(|a| never_probed(**a)) {
        return no(
            ProbeOutcome::NotAllowed,
            format!(
                "it resolves to {bad}, never probed: link-local, a cloud metadata address or no destination"
            ),
        );
    }
    let allowed: Vec<IpAddr> = addrs
        .iter()
        .copied()
        .filter(|a| probe_reach::allows(rules, name.as_deref(), *a, asked.port))
        .collect();
    if allowed.is_empty() {
        return no(
            ProbeOutcome::NotAllowed,
            "outside what its owner lists in knowhow_probe_reach".into(),
        );
    }
    // Connect, only to what was checked, the first that answers.
    let mut last: Option<(IpAddr, String)> = None;
    for ip in allowed {
        let started = Instant::now();
        let connect = TcpStream::connect(SocketAddr::new(ip, asked.port));
        match tokio::time::timeout(left(deadline), connect).await {
            Ok(Ok(tcp)) => {
                let rtt = ms(started.elapsed());
                return match kind {
                    ProbeKind::Tcp => {
                        drop(tcp);
                        reply(id, ProbeOutcome::Ok, Some(rtt), Some(ip), None, None)
                    }
                    ProbeKind::Tls => {
                        let shake = handshake(tcp, asked.sni.as_deref(), ip);
                        match tokio::time::timeout(left(deadline), shake).await {
                            Ok(Ok(seen)) => {
                                reply(id, ProbeOutcome::Ok, Some(rtt), Some(ip), Some(seen), None)
                            }
                            Ok(Err(e)) => reply(
                                id,
                                ProbeOutcome::Failed,
                                Some(rtt),
                                Some(ip),
                                None,
                                Some(format!("the TLS handshake failed: {e}")),
                            ),
                            Err(_) => reply(
                                id,
                                ProbeOutcome::Failed,
                                Some(rtt),
                                Some(ip),
                                None,
                                Some("the TLS handshake did not finish in time".into()),
                            ),
                        }
                    }
                };
            }
            Ok(Err(e)) => last = Some((ip, e.to_string())),
            Err(_) => {
                last = Some((ip, "no answer in time".into()));
                break;
            }
        }
    }
    let (ip, why) = last.unwrap_or((addrs[0], "no address answered".into()));
    reply(
        id,
        ProbeOutcome::Unreachable,
        None,
        Some(ip),
        None,
        Some(why),
    )
}

/// The handshake, reading the leaf certificate: its names and its SHA-256.
async fn handshake(tcp: TcpStream, sni: Option<&str>, ip: IpAddr) -> Result<ProbeTls, String> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(Arc::clone(&provider))
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(SeeOnly { provider }))
        .with_no_client_auth();
    let name = sni
        .and_then(|s| rustls::pki_types::ServerName::try_from(s.to_string()).ok())
        .unwrap_or_else(|| rustls::pki_types::ServerName::IpAddress(ip.into()));
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    let stream = connector
        .connect(name, tcp)
        .await
        .map_err(|e| e.to_string())?;
    let (_, conn) = stream.get_ref();
    let leaf = conn
        .peer_certificates()
        .and_then(|chain| chain.first())
        .ok_or_else(|| "the server presented no certificate".to_string())?;
    Ok(ProbeTls {
        names: cert_names(leaf.as_ref()),
        fingerprint: hex::encode(Sha256::digest(leaf.as_ref())),
    })
}

/// A certificate's names: its subject alternative names (DNS names and IP
/// addresses), else its subject's common name, each as the server's record
/// reads them, at most [`probe_limits::MAX_TLS_NAMES`].
pub(crate) fn cert_names(der: &[u8]) -> Vec<String> {
    use x509_parser::prelude::{FromDer, GeneralName, X509Certificate};
    let Ok((_, cert)) = X509Certificate::from_der(der) else {
        return Vec::new();
    };
    let mut names: Vec<String> = Vec::new();
    if let Ok(Some(san)) = cert.subject_alternative_name() {
        for general in &san.value.general_names {
            match general {
                GeneralName::DNSName(d) => names.push(d.to_ascii_lowercase()),
                GeneralName::IPAddress(bytes) => {
                    let ip = match bytes.len() {
                        4 => <[u8; 4]>::try_from(*bytes).ok().map(IpAddr::from),
                        16 => <[u8; 16]>::try_from(*bytes).ok().map(IpAddr::from),
                        _ => None,
                    };
                    if let Some(ip) = ip {
                        names.push(ip.to_string());
                    }
                }
                _ => {}
            }
        }
    }
    if names.is_empty() {
        for cn in cert.subject().iter_common_name() {
            if let Ok(s) = cn.as_str() {
                names.push(s.to_ascii_lowercase());
            }
        }
    }
    // What the server reads as no name refuses its whole `tls`: kept out.
    names.retain(|n| {
        !n.is_empty()
            && n.len() <= probe_limits::MAX_HOST
            && !n.chars().any(|c| c.is_control() || c.is_whitespace())
    });
    names.dedup();
    names.truncate(probe_limits::MAX_TLS_NAMES);
    names
}

/// A verifier that reads, never trusts: any chain is accepted, since a
/// private CA is what an organization's own services have and a probe says
/// only what the address presents, but the handshake's signature is
/// checked, so the server holds the key of the certificate recorded.
#[derive(Debug)]
struct SeeOnly {
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl rustls::client::danger::ServerCertVerifier for SeeOnly {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::net::TcpListener;

    fn asked(kind: &str, host: &str, port: u16) -> Asked {
        Asked {
            id: ObjectId::new(),
            kind: kind.into(),
            host: host.into(),
            port,
            sni: None,
        }
    }

    fn list(entries: &[String]) -> Vec<DestinationRule> {
        probe_reach::rules(entries)
    }

    /// The answer's outcome, address, round trip, certificate and detail.
    fn read(
        msg: &ClientMsg,
    ) -> (
        ProbeOutcome,
        Option<String>,
        Option<u32>,
        Option<ProbeTls>,
        Option<String>,
    ) {
        let ClientMsg::KnowhowProbed {
            outcome,
            address,
            rtt_ms,
            tls,
            detail,
            ..
        } = msg
        else {
            panic!("not an answer: {msg:?}");
        };
        let tls = tls
            .as_ref()
            .map(|v| ProbeTls::read(v).expect("a tls the server reads"));
        (*outcome, address.clone(), *rtt_ms, tls, detail.clone())
    }

    /// A listener on 127.0.0.1 that counts what it accepts, and closes each.
    async fn counting() -> (u16, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(AtomicUsize::new(0));
        let n = Arc::clone(&seen);
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                n.fetch_add(1, Ordering::SeqCst);
                drop(tcp);
            }
        });
        (port, seen)
    }

    /// A port nothing listens on, held while what it returns lives: bound and
    /// never listening, so no test beside this one is given it meanwhile. One
    /// bound and let go was given to a listener once, and answered the probe.
    fn closed_port() -> (u16, tokio::net::TcpSocket) {
        let socket = tokio::net::TcpSocket::new_v4().unwrap();
        socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
        (socket.local_addr().unwrap().port(), socket)
    }

    /// A `tcp` probe within the list connects, says where and how long, and
    /// sends nothing; a port nothing answers is `unreachable`, the address it
    /// tried named.
    #[tokio::test]
    async fn a_tcp_probe_connects_within_the_list() {
        let (port, seen) = counting().await;
        let rules = list(&[format!("127.0.0.1:{port}")]);
        let (outcome, address, rtt, tls, detail) =
            read(&measure(&asked("tcp", "127.0.0.1", port), &rules).await);
        assert_eq!(outcome, ProbeOutcome::Ok, "{detail:?}");
        assert_eq!(address.as_deref(), Some("127.0.0.1"));
        assert!(rtt.is_some() && tls.is_none() && detail.is_none());
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(seen.load(Ordering::SeqCst), 1);

        let (shut, _held) = closed_port();
        let rules = list(&[format!("127.0.0.1:{shut}")]);
        let (outcome, address, rtt, _, detail) =
            read(&measure(&asked("tcp", "127.0.0.1", shut), &rules).await);
        assert_eq!(outcome, ProbeOutcome::Unreachable);
        assert_eq!(address.as_deref(), Some("127.0.0.1"));
        assert!(rtt.is_none() && detail.is_some());
    }

    /// Decision 53: outside the list, or with nothing listed, `not_allowed`
    /// and no connect; a never-probed address is never reached, whatever the
    /// list says; and a name is held to its name rule after it resolves.
    #[tokio::test]
    async fn what_the_list_does_not_allow_is_never_reached() {
        let (port, seen) = counting().await;
        let other = list(&[format!("127.0.0.1:{}", port.wrapping_add(1).max(1))]);
        for rules in [Vec::new(), other] {
            let (outcome, address, _, _, detail) =
                read(&measure(&asked("tcp", "127.0.0.1", port), &rules).await);
            assert_eq!(outcome, ProbeOutcome::NotAllowed, "{detail:?}");
            assert_eq!(address, None, "nowhere was tried");
        }
        for host in ["169.254.169.254", "fd00:ec2::254", "100.100.100.200"] {
            let rules = list(&["0.0.0.0/0".into(), "[::/0]".into()]);
            let (outcome, _, _, _, detail) = read(&measure(&asked("tcp", host, 80), &rules).await);
            assert_eq!(outcome, ProbeOutcome::NotAllowed, "{host}: {detail:?}");
        }
        // `localhost` by its name: the name rule lets it reach wherever the
        // name resolves, and an address rule alone would not.
        let named = list(&[format!("localhost:{port}")]);
        let (outcome, _, _, _, detail) =
            read(&measure(&asked("tcp", "localhost", port), &named).await);
        assert_eq!(outcome, ProbeOutcome::Ok, "{detail:?}");
        let by_address = list(&[format!("10.0.0.0/8:{port}")]);
        let (outcome, ..) = read(&measure(&asked("tcp", "localhost", port), &by_address).await);
        assert_eq!(outcome, ProbeOutcome::NotAllowed);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            seen.load(Ordering::SeqCst),
            1,
            "only the allowed one connected"
        );
    }

    /// What the server names wrong is `failed`, never a connect: a kind this
    /// build does not know, a host that is no host, no port.
    #[tokio::test]
    async fn what_cannot_be_probed_fails_with_its_reason() {
        let rules = list(&["127.0.0.1".into()]);
        for (a, says) in [
            (asked("http", "127.0.0.1", 80), "does not probe"),
            (asked("tcp", "https://127.0.0.1", 80), "no host"),
            (asked("tcp", "127.0.0.1", 0), "no port"),
        ] {
            let (outcome, _, _, _, detail) = read(&measure(&a, &rules).await);
            assert_eq!(outcome, ProbeOutcome::Failed);
            assert!(detail.unwrap().contains(says), "{a:?}");
        }
    }

    /// A TLS server for `localhost` on 127.0.0.1, with a certificate made for
    /// this test; its port and the certificate's DER.
    async fn tls_server() -> (u16, Vec<u8>) {
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let der = cert.cert.der().to_vec();
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.cert.der().clone()],
            rustls::pki_types::PrivateKeyDer::Pkcs8(cert.key_pair.serialize_der().into()),
        )
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
        (port, der)
    }

    /// A `tls` probe reads the leaf the server presents, a self-signed one
    /// as well: its names and its SHA-256, in the spelling the server reads.
    #[tokio::test]
    async fn a_tls_probe_reads_the_certificate_it_is_shown() {
        let (port, der) = tls_server().await;
        let rules = list(&[format!("127.0.0.1:{port}")]);
        let mut a = asked("tls", "127.0.0.1", port);
        a.sni = Some("localhost".into());
        let (outcome, _, rtt, tls, detail) = read(&measure(&a, &rules).await);
        assert_eq!(outcome, ProbeOutcome::Ok, "{detail:?}");
        assert!(rtt.is_some());
        let tls = tls.expect("a certificate");
        assert_eq!(tls.names, ["localhost"]);
        assert_eq!(tls.fingerprint, hex::encode(Sha256::digest(&der)));

        // A port that answers but speaks no TLS: `failed`, past the connect.
        let (plain, _) = counting().await;
        let rules = list(&[format!("127.0.0.1:{plain}")]);
        let (outcome, address, rtt, tls, detail) =
            read(&measure(&asked("tls", "127.0.0.1", plain), &rules).await);
        assert_eq!(outcome, ProbeOutcome::Failed);
        assert!(address.is_some() && rtt.is_some() && tls.is_none());
        assert!(detail.unwrap().contains("TLS handshake"));
    }

    /// A secondary enrollment's connection probes nothing, and says so.
    #[tokio::test]
    async fn a_secondary_enrollment_probes_nothing() {
        let (tx, mut rx) = mpsc::channel(4);
        handle(None, asked("tcp", "127.0.0.1", 80), false, tx);
        let (outcome, _, _, _, detail) = read(&rx.recv().await.unwrap());
        assert_eq!(outcome, ProbeOutcome::NotAllowed);
        assert!(detail.unwrap().contains("primary organization"));
    }

    /// The daemon's prober reads its list from the config file at each
    /// probe, runs a probe id once, and answers it again as it was.
    #[tokio::test]
    async fn a_probe_is_run_once_and_answered_again_as_it_was() {
        let (port, seen) = counting().await;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut cfg = crate::config::test_fixture();
        cfg.knowhow_probe_reach = vec![format!("127.0.0.1:{port}")];
        crate::config::save(&path, &cfg).unwrap();
        let prober = Prober::new(path.clone(), Default::default());

        let probe = asked("tcp", "127.0.0.1", port);
        let (tx, mut rx) = mpsc::channel(4);
        Arc::clone(&prober).request(probe.clone(), tx.clone());
        let first = rx.recv().await.unwrap();
        assert_eq!(read(&first).0, ProbeOutcome::Ok);
        Arc::clone(&prober).request(probe.clone(), tx.clone());
        let again = rx.recv().await.unwrap();
        assert_eq!(
            format!("{again:?}"),
            format!("{first:?}"),
            "answered as it was"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(seen.load(Ordering::SeqCst), 1, "run once");

        // The list is read again at each probe: cleared, nowhere.
        cfg.knowhow_probe_reach.clear();
        crate::config::save(&path, &cfg).unwrap();
        Arc::clone(&prober).request(asked("tcp", "127.0.0.1", port), tx);
        assert_eq!(read(&rx.recv().await.unwrap()).0, ProbeOutcome::NotAllowed);
    }
}
