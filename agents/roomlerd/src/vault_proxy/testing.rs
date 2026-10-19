// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P3-5b — what the proxies' tests and the endpoint's share: a Postgres
//! upstream over real TLS, with a certificate made for each test, that
//! authenticates as it is told to and answers queries; and a client's half of
//! the wire.

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::{self, ServerConfig};

use super::pg::{CANCEL_REQUEST, SSL_REQUEST, frame, parse_params, read_message, read_startup};
use super::scram::server::ScramServer;

/// A message from the client's side: `tag`, then `body`.
pub(crate) fn message(tag: u8, body: &[u8]) -> Vec<u8> {
    frame(Some(tag), body)
}

/// How the fake upstream behaves.
#[derive(Clone, PartialEq)]
pub(crate) enum Upstreams {
    /// SCRAM with this password, then a session that answers queries.
    Scram(String),
    /// Declines TLS.
    NoTls,
    /// Asks for a cleartext password.
    Cleartext,
    /// Checks the proof for this password, then signs wrong: a server
    /// that never held it.
    Forged(String),
    /// Checks the proof, then lets the session in without signing at
    /// all.
    SkipFinal(String),
    /// Trusts the connection: `AuthenticationOk` at once.
    Trusting,
    /// SCRAM with this password, after a protocol negotiation and with a
    /// notice before its `AuthenticationOk`.
    Negotiates(String),
    /// P3-5b-3 — SCRAM with this password, bound to its own certificate,
    /// and the GS2 header it saw reported.
    Binds(String),
    /// Offers no `-PLUS`, and cannot bind: an older server, or a pooler.
    PlainOnly(String),
    /// Offers no `-PLUS`, though it binds: what a relay that strips `-PLUS`
    /// from the offer leaves.
    Stripped(String),
    /// Offers `-PLUS`, and binds to another certificate than the one the
    /// proxy saw: what the real upstream sees behind a TLS-terminating relay.
    BindsElsewhere(String),
}

/// What the fake upstream saw.
#[derive(Debug, PartialEq)]
pub(crate) enum Saw {
    Startup(Vec<(String, String)>),
    Query(String),
    Cancel(Vec<u8>),
    /// Bytes from the proxy after a refusal: what a password would be.
    After(Vec<u8>),
    /// P3-5b-3 — the SASL mechanism and the GS2 header the proxy chose.
    Sasl(String, String),
}

/// A TLS upstream on a random port of 127.0.0.1, with a certificate for
/// `localhost` made for this test; its CA's PEM, its port, and what it
/// saw.
pub(crate) async fn fake(how: Upstreams) -> (String, u16, mpsc::UnboundedReceiver<Saw>) {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let pem = cert.cert.pem();
    // P3-5b-3 — this channel's binding data, as Postgres computes its own.
    let own_binding = super::tls::server_end_point(cert.cert.der()).unwrap();
    let config =
        ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![cert.cert.der().clone()],
                rustls::pki_types::PrivateKeyDer::Pkcs8(cert.key_pair.serialize_der().into()),
            )
            .unwrap();
    let acceptor = TlsAcceptor::from(Arc::new(config));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            let Ok((mut tcp, _)) = listener.accept().await else {
                return;
            };
            let (acceptor, tx, how) = (acceptor.clone(), tx.clone(), how.clone());
            let own_binding = own_binding.clone();
            tokio::spawn(async move {
                let Ok(packet) = read_startup(&mut tcp).await else {
                    return;
                };
                assert_eq!(&packet[..4], &SSL_REQUEST.to_be_bytes());
                if how == Upstreams::NoTls {
                    tcp.write_all(b"N").await.unwrap();
                    let rest = what_follows(&mut tcp).await;
                    let _ = tx.send(Saw::After(rest));
                    return;
                }
                tcp.write_all(b"S").await.unwrap();
                // A proxy that refused the certificate hangs up mid-handshake.
                let Ok(mut s) = acceptor.accept(tcp).await else {
                    return;
                };
                let Ok(packet) = read_startup(&mut s).await else {
                    return;
                };
                if packet[..4] == CANCEL_REQUEST.to_be_bytes() {
                    let _ = tx.send(Saw::Cancel(packet));
                    return;
                }
                let _ = tx.send(Saw::Startup(parse_params(&packet[4..]).unwrap()));
                let r = |code: u32, more: &[u8]| {
                    let mut b = code.to_be_bytes().to_vec();
                    b.extend(more);
                    frame(Some(b'R'), &b)
                };
                match how {
                    Upstreams::Cleartext => {
                        s.write_all(&r(3, &[])).await.unwrap();
                        let rest = what_follows(&mut s).await;
                        let _ = tx.send(Saw::After(rest));
                        return;
                    }
                    Upstreams::Trusting => s.write_all(&r(0, &[])).await.unwrap(),
                    Upstreams::Scram(ref password)
                    | Upstreams::Forged(ref password)
                    | Upstreams::SkipFinal(ref password)
                    | Upstreams::Negotiates(ref password)
                    | Upstreams::Binds(ref password)
                    | Upstreams::PlainOnly(ref password)
                    | Upstreams::Stripped(ref password)
                    | Upstreams::BindsElsewhere(ref password) => {
                        let negotiates = matches!(how, Upstreams::Negotiates(_));
                        if negotiates {
                            // Minor version 0, and one option it does not know.
                            let v = b"\0\0\0\0\0\0\0\x01_pq_.x\0";
                            s.write_all(&frame(Some(b'v'), v)).await.unwrap();
                        }
                        let offer: &[u8] = match how {
                            Upstreams::PlainOnly(_) | Upstreams::Stripped(_) => {
                                b"SCRAM-SHA-256\0\0"
                            }
                            _ => b"SCRAM-SHA-256-PLUS\0SCRAM-SHA-256\0\0",
                        };
                        s.write_all(&r(10, offer)).await.unwrap();
                        let (tag, body) = read_message(&mut s, 64 * 1024).await.unwrap();
                        assert_eq!(tag, b'p');
                        // SASLInitialResponse: the mechanism, its length, its data.
                        let nul = body.iter().position(|b| *b == 0).unwrap();
                        let mechanism = String::from_utf8(body[..nul].to_vec()).unwrap();
                        let first = &body[nul + 1 + 4..];
                        let mut srv = ScramServer::new(password, b"a-salt", 4096, "upnonce");
                        srv = match how {
                            Upstreams::PlainOnly(_) => srv,
                            Upstreams::BindsElsewhere(_) => {
                                srv.binding(b"the hash of a relay's own certificate".to_vec())
                            }
                            _ => srv.binding(own_binding.clone()),
                        };
                        let forged = matches!(how, Upstreams::Forged(_));
                        let server_first = srv.server_first(std::str::from_utf8(first).unwrap());
                        if matches!(
                            how,
                            Upstreams::Binds(_)
                                | Upstreams::PlainOnly(_)
                                | Upstreams::Stripped(_)
                                | Upstreams::BindsElsewhere(_)
                        ) {
                            let _ = tx.send(Saw::Sasl(mechanism, srv.gs2().to_string()));
                        }
                        s.write_all(&r(11, server_first.as_bytes())).await.unwrap();
                        let (_, fin) = read_message(&mut s, 64 * 1024).await.unwrap();
                        match srv.server_final(std::str::from_utf8(&fin).unwrap()) {
                            Ok(_) if forged => {
                                let forged = "v=6rriTRBi23WpRR/wtup+mMhUZUn/dB5nLTJRsjl95G4=";
                                s.write_all(&r(12, forged.as_bytes())).await.unwrap();
                                s.write_all(&r(0, &[])).await.unwrap();
                            }
                            Ok(_) if matches!(how, Upstreams::SkipFinal(_)) => {
                                s.write_all(&r(0, &[])).await.unwrap();
                            }
                            Ok(v) => {
                                s.write_all(&r(12, v.as_bytes())).await.unwrap();
                                if negotiates {
                                    let notice = b"SNOTICE\0Mheld\0\0";
                                    s.write_all(&frame(Some(b'N'), notice)).await.unwrap();
                                }
                                s.write_all(&r(0, &[])).await.unwrap();
                            }
                            Err(why) => {
                                // Postgres's own words for each refusal.
                                let (code, message) = match why.as_str() {
                                    "e=invalid-proof" => {
                                        ("28P01", "password authentication failed for user \"app\"")
                                    }
                                    "e=channel-bindings-dont-match" => {
                                        ("08P01", "SCRAM channel binding check failed")
                                    }
                                    "e=server-does-support-channel-binding" => {
                                        ("08P01", "SCRAM channel binding negotiation error")
                                    }
                                    _ => ("08P01", "malformed SCRAM message"),
                                };
                                let mut e = Vec::new();
                                for (c, t) in [(b'S', "FATAL"), (b'C', code), (b'M', message)] {
                                    e.push(c);
                                    e.extend(t.as_bytes());
                                    e.push(0);
                                }
                                e.push(0);
                                s.write_all(&frame(Some(b'E'), &e)).await.unwrap();
                                return;
                            }
                        }
                    }
                    Upstreams::NoTls => unreachable!(),
                }
                // The session: a parameter, the cancel key, ready.
                s.write_all(&frame(Some(b'S'), b"server_version\x0017.2\x00"))
                    .await
                    .unwrap();
                s.write_all(&frame(Some(b'K'), &[0, 0, 0, 7, 1, 2, 3, 4]))
                    .await
                    .unwrap();
                s.write_all(&frame(Some(b'Z'), b"I")).await.unwrap();
                while let Ok((tag, body)) = read_message(&mut s, 1 << 20).await {
                    if tag == b'X' {
                        return;
                    }
                    let q = String::from_utf8_lossy(&body[..body.len() - 1]).into_owned();
                    let _ = tx.send(Saw::Query(q));
                    s.write_all(&frame(Some(b'C'), b"SELECT 1\x00"))
                        .await
                        .unwrap();
                    s.write_all(&frame(Some(b'Z'), b"I")).await.unwrap();
                }
            });
        }
    });
    (pem, port, rx)
}

/// A password made for one test run, never written down.
pub(crate) fn made() -> String {
    format!("made-{:016x}", rand::random::<u64>())
}

/// What the proxy sends in the two seconds after a refusal: nothing,
/// when it hangs up as it should.
async fn what_follows<S: AsyncRead + Unpin>(s: &mut S) -> Vec<u8> {
    let mut got = Vec::new();
    let mut buf = [0u8; 4096];
    while let Ok(Ok(n)) = tokio::time::timeout(Duration::from_secs(2), s.read(&mut buf)).await {
        if n == 0 {
            break;
        }
        got.extend(&buf[..n]);
    }
    got
}

/// A client's startup message with `params`, protocol 3.0.
pub(crate) fn startup(params: &[(&str, &str)]) -> Vec<u8> {
    let mut body = (3u32 << 16).to_be_bytes().to_vec();
    for (k, v) in params {
        body.extend(k.as_bytes());
        body.push(0);
        body.extend(v.as_bytes());
        body.push(0);
    }
    body.push(0);
    frame(None, &body)
}

/// The next message from `c`, within 10 s.
pub(crate) async fn next<R: AsyncRead + Unpin>(c: &mut R) -> (u8, Vec<u8>) {
    tokio::time::timeout(Duration::from_secs(10), read_message(c, 1 << 20))
        .await
        .expect("an answer within 10 s")
        .expect("a message")
}
