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

// FR-90 P3-5c-1 — a MongoDB upstream, over TLS from its first byte.

/// A TLS listener on a random port of 127.0.0.1, with a certificate for
/// `localhost` made for this test: its CA's PEM, its acceptor, the listener,
/// and its port.
async fn tls_server() -> (String, TlsAcceptor, TcpListener, u16) {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let pem = cert.cert.pem();
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
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    (pem, TlsAcceptor::from(Arc::new(config)), listener, port)
}

/// How the fake MongoDB upstream behaves.
#[derive(Clone, PartialEq)]
pub(crate) enum MongoUpstreams {
    /// SCRAM-SHA-256 with this password, the empty exchange skipped, then
    /// answering what it is asked.
    Scram(String),
    /// The same, without skipping the empty exchange: a server before 4.4.
    NoSkip(String),
    /// Checks the proof, then signs wrong: a server that never held the
    /// password.
    Forged(String),
}

/// What the fake MongoDB upstream saw, in order.
#[derive(Debug, PartialEq)]
pub(crate) enum MongoSaw {
    /// An `OP_MSG` command: its name and its `$db`.
    Command(String, String),
    /// The user a SCRAM exchange named.
    User(String),
    /// A legacy `OP_QUERY`: its collection, and its command's name.
    Query(String, String),
    /// Whether a handshake carried client metadata.
    Metadata(bool),
}

/// A MongoDB upstream on a random port of 127.0.0.1, TLS from its first
/// byte: its CA's PEM, its port, and what it saw.
pub(crate) async fn fake_mongo(
    how: MongoUpstreams,
) -> (String, u16, mpsc::UnboundedReceiver<MongoSaw>) {
    use base64::Engine;
    use bson::spec::BinarySubtype;
    use bson::{Binary, Bson, doc};

    use super::mongo::{OP_MSG, OP_QUERY, op_msg, op_msg_document, op_reply, read_message};

    let (pem, acceptor, listener, port) = tls_server().await;
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else {
                return;
            };
            let (acceptor, tx, how) = (acceptor.clone(), tx.clone(), how.clone());
            tokio::spawn(async move {
                // A proxy that refused the certificate hangs up mid-handshake.
                let Ok(mut s) = acceptor.accept(tcp).await else {
                    return;
                };
                let (password, forged, skips) = match &how {
                    MongoUpstreams::Scram(p) => (p.clone(), false, true),
                    MongoUpstreams::NoSkip(p) => (p.clone(), false, false),
                    MongoUpstreams::Forged(p) => (p.clone(), true, true),
                };
                let mut srv = ScramServer::new(&password, b"a-salt", 4096, "upnonce");
                let mut continued = 0;
                let binary = |bytes: Vec<u8>| {
                    Bson::Binary(Binary {
                        subtype: BinarySubtype::Generic,
                        bytes,
                    })
                };
                while let Ok(m) = read_message(&mut s, 1 << 20).await {
                    let answer = match m.op_code {
                        OP_MSG => {
                            let Ok(doc) = op_msg_document(m.body()) else {
                                return;
                            };
                            let name = doc.keys().next().cloned().unwrap_or_default();
                            let db = doc.get_str("$db").unwrap_or("").to_string();
                            let _ = tx.send(MongoSaw::Command(name.clone(), db));
                            if name == "hello" {
                                let _ = tx.send(MongoSaw::Metadata(doc.contains_key("client")));
                            }
                            let payload = || match doc.get("payload") {
                                Some(Bson::Binary(b)) => {
                                    String::from_utf8(b.bytes.clone()).unwrap()
                                }
                                _ => String::new(),
                            };
                            let reply = match name.as_str() {
                                "saslStart" => {
                                    let first = payload();
                                    let user = first
                                        .split(',')
                                        .find_map(|a| a.strip_prefix("n="))
                                        .unwrap_or("")
                                        .to_string();
                                    let _ = tx.send(MongoSaw::User(user));
                                    let server_first = srv.server_first(&first);
                                    doc! {
                                        "conversationId": 1,
                                        "done": false,
                                        "payload": binary(server_first.into_bytes()),
                                        "ok": 1.0,
                                    }
                                }
                                "saslContinue" if continued == 0 => {
                                    continued += 1;
                                    match srv.server_final(&payload()) {
                                        Ok(v) => {
                                            let v = if forged {
                                                // A signature no server holding the
                                                // password would make.
                                                let wrong =
                                                    base64::engine::general_purpose::STANDARD
                                                        .encode([7u8; 32]);
                                                format!("v={wrong}")
                                            } else {
                                                v
                                            };
                                            doc! {
                                                "conversationId": 1,
                                                "done": skips,
                                                "payload": binary(v.into_bytes()),
                                                "ok": 1.0,
                                            }
                                        }
                                        Err(_) => doc! {
                                            "ok": 0.0,
                                            "errmsg": "Authentication failed.",
                                            "code": 18,
                                            "codeName": "AuthenticationFailed",
                                        },
                                    }
                                }
                                "saslContinue" => doc! {
                                    "conversationId": 1,
                                    "done": true,
                                    "payload": binary(Vec::new()),
                                    "ok": 1.0,
                                },
                                "hello" => doc! {
                                    "isWritablePrimary": true,
                                    "maxWireVersion": 21,
                                    "ok": 1.0,
                                },
                                _ => doc! { "ok": 1.0 },
                            };
                            op_msg(0, m.request_id, &reply)
                        }
                        OP_QUERY => {
                            // Flags, the collection's name, two counts, then
                            // the query.
                            let body = m.body();
                            let Some(nul) = body[4..].iter().position(|b| *b == 0) else {
                                return;
                            };
                            let collection =
                                String::from_utf8_lossy(&body[4..4 + nul]).into_owned();
                            let Ok(query) =
                                bson::Document::from_reader(&mut &body[4 + nul + 1 + 8..])
                            else {
                                return;
                            };
                            let name = query.keys().next().cloned().unwrap_or_default();
                            let _ = tx.send(MongoSaw::Query(collection, name));
                            let _ = tx.send(MongoSaw::Metadata(query.contains_key("client")));
                            op_reply(
                                m.request_id,
                                &doc! { "ismaster": true, "maxWireVersion": 21, "ok": 1.0 },
                            )
                        }
                        _ => return,
                    };
                    if s.write_all(&answer).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    (pem, port, rx)
}

/// A client's legacy `OP_QUERY`: `query` against `collection`, as a driver
/// sends its first handshake.
pub(crate) fn op_query(request_id: i32, collection: &str, query: &bson::Document) -> Vec<u8> {
    let mut body = 0i32.to_le_bytes().to_vec();
    body.extend(collection.as_bytes());
    body.push(0);
    body.extend(0i32.to_le_bytes());
    body.extend((-1i32).to_le_bytes());
    query
        .to_writer(&mut body)
        .expect("a document writes into memory");
    let len = i32::try_from(16 + body.len()).unwrap();
    let mut out = len.to_le_bytes().to_vec();
    out.extend(request_id.to_le_bytes());
    out.extend(0i32.to_le_bytes());
    out.extend(super::mongo::OP_QUERY.to_le_bytes());
    out.extend(body);
    out
}

/// P3-5c-2 — a client's MongoDB command, as an `OP_MSG`.
pub(crate) fn mongo_command(request_id: i32, body: &bson::Document) -> Vec<u8> {
    super::mongo::op_msg(request_id, 0, body)
}

/// The next MongoDB reply on `r`, within 10 s: its op code, the request it
/// answers, and its document.
pub(crate) async fn mongo_reply<R: AsyncRead + Unpin>(r: &mut R) -> (i32, i32, bson::Document) {
    use super::mongo::{OP_MSG, OP_REPLY, op_msg_document, read_message};
    let m = tokio::time::timeout(Duration::from_secs(10), read_message(r, 1 << 20))
        .await
        .expect("an answer within 10 s")
        .expect("a message");
    let doc = match m.op_code {
        OP_MSG => op_msg_document(m.body()).unwrap(),
        // Flags, a cursor, where it starts, how many: then the document.
        OP_REPLY => bson::Document::from_reader(&mut &m.body()[20..]).unwrap(),
        other => panic!("a reply of op code {other}"),
    };
    (m.op_code, m.response_to, doc)
}
