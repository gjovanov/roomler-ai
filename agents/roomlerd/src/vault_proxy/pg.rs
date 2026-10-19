// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P3-5b — the Postgres proxy's session: one client connection on the
//! endpoint, and one upstream connection the proxy authenticates itself.
//!
//! | Step | What happens |
//! |---|---|
//! | the client's `SSLRequest` or `GSSENCRequest` | answered `N`: the endpoint is a local socket |
//! | its `CancelRequest` | sent to the upstream as it came, over TLS: the key it names is the upstream's own, relayed unchanged |
//! | its startup | read; `replication` refused, each one it names; `user` replaced by the secret's; everything else kept |
//! | the upstream | TCP, `SSLRequest`, TLS verified ([`super::tls`]); an upstream that declines TLS is refused |
//! | its authentication | SCRAM-SHA-256 only, the server's signature checked; cleartext, MD5 and every other kind refused |
//! | then | `AuthenticationOk` to the client, and every byte both ways |
//!
//! ⚠️ The client never authenticates: the endpoint is its credential (a
//! socket only the session's account can reach). So it is never asked for a
//! password, and the secret's never crosses the endpoint, either way.
//!
//! ⚠️ An upstream's own error before the session starts (a wrong password,
//! an unknown database) is relayed as the upstream wrote it. The proxy's
//! refusals are `FATAL`, SQLSTATE `08004`, and say why.

use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream;
use zeroize::Zeroizing;

use super::scram::ScramClient;
use super::tls;

/// Where, and as whom, the proxy authenticates.
pub(crate) struct Upstream {
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) user: String,
    pub(crate) password: Zeroizing<String>,
    pub(crate) tls: tokio_rustls::TlsConnector,
}

pub(super) const SSL_REQUEST: u32 = 80_877_103;
const GSSENC_REQUEST: u32 = 80_877_104;
pub(super) const CANCEL_REQUEST: u32 = 80_877_102;
/// The longest startup packet a client may send (Postgres's own limit).
const MAX_STARTUP: usize = 10_000;
/// The longest upstream message before the session starts.
const MAX_AUTH_MESSAGE: usize = 64 * 1024;
/// How long a session may take to start, both sides together.
pub(crate) const START_DEADLINE: Duration = Duration::from_secs(30);
/// How long the upstream may take to accept a connection.
const CONNECT_DEADLINE: Duration = Duration::from_secs(10);

/// Why a session did not start.
enum Refusal {
    /// The proxy's own reason, to tell the client.
    Tell(String),
    /// The upstream's error, relayed to the client already.
    Told(String),
}

/// Serve one client: start its session upstream, then relay both ways until
/// either side closes. An `Err` says why no session started (the client was
/// told too, where it could be).
pub(crate) async fn serve<C>(mut client: C, up: &Upstream) -> Result<(), String>
where
    C: AsyncRead + AsyncWrite + Unpin,
{
    let started = tokio::time::timeout(START_DEADLINE, start(&mut client, up)).await;
    let mut upstream = match started {
        Ok(Ok(Some(upstream))) => upstream,
        // A cancel: nothing more to serve.
        Ok(Ok(None)) => return Ok(()),
        Ok(Err(Refusal::Told(why))) => return Err(why),
        Ok(Err(Refusal::Tell(why))) => {
            let _ = client.write_all(&fatal(&why)).await;
            return Err(why);
        }
        Err(_) => {
            let why = format!("the session did not start within {START_DEADLINE:?}");
            let _ = client.write_all(&fatal(&why)).await;
            return Err(why);
        }
    };
    // A reset at the end is how many clients close: the session ran.
    let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
    Ok(())
}

async fn start<C>(client: &mut C, up: &Upstream) -> Result<Option<TlsStream<TcpStream>>, Refusal>
where
    C: AsyncRead + AsyncWrite + Unpin,
{
    // The client's startup, past at most one SSL and one GSS ask.
    let mut asked = 0;
    let (version, mut params) = loop {
        let packet = read_startup(client).await.map_err(Refusal::Tell)?;
        let code = u32::from_be_bytes([packet[0], packet[1], packet[2], packet[3]]);
        match code {
            SSL_REQUEST | GSSENC_REQUEST if asked < 2 => {
                asked += 1;
                client
                    .write_all(b"N")
                    .await
                    .map_err(|e| Refusal::Told(e.to_string()))?;
            }
            CANCEL_REQUEST => {
                cancel(&packet, up).await;
                return Ok(None);
            }
            v if v >> 16 == 3 => break (v, parse_params(&packet[4..]).map_err(Refusal::Tell)?),
            _ => {
                return Err(Refusal::Tell(
                    "the client asked for a protocol the proxy does not speak".into(),
                ));
            }
        }
    };
    // Postgres takes the LAST `replication` a startup names, so each one
    // must say no: a "no" first is no cover for a "database" after it.
    if params
        .iter()
        .any(|(k, v)| k == "replication" && !matches!(v.as_str(), "false" | "off" | "no" | "0"))
    {
        return Err(Refusal::Tell(
            "a replication connection is not something the proxy relays".into(),
        ));
    }
    // The secret's user ends a NUL-separated startup: one holding a NUL (or
    // nothing) would name parameters of its own.
    if up.user.is_empty() || up.user.contains('\0') {
        return Err(Refusal::Tell(
            "the secret's user is not a name Postgres takes".into(),
        ));
    }
    params.retain(|(k, _)| k != "user");
    params.insert(0, ("user".into(), up.user.clone()));

    let mut upstream = connect(up).await.map_err(Refusal::Tell)?;
    let mut body = version.to_be_bytes().to_vec();
    for (k, v) in &params {
        body.extend(k.as_bytes());
        body.push(0);
        body.extend(v.as_bytes());
        body.push(0);
    }
    body.push(0);
    upstream
        .write_all(&frame(None, &body))
        .await
        .map_err(|e| Refusal::Tell(format!("the upstream: {e}")))?;

    // Authenticate. What the upstream says meanwhile reaches the client
    // where a client reads it.
    let mut scram: Option<ScramClient> = None;
    let mut verified = false;
    let mut held = Vec::new();
    loop {
        let (tag, body) = read_message(&mut upstream, MAX_AUTH_MESSAGE)
            .await
            .map_err(Refusal::Tell)?;
        match tag {
            b'R' if body.len() >= 4 => {
                let code = u32::from_be_bytes([body[0], body[1], body[2], body[3]]);
                match code {
                    // An upstream that trusts this connection asks for
                    // nothing, and is told nothing. One that began SCRAM
                    // must have proved itself first.
                    0 if scram.is_none() || verified => break,
                    0 => {
                        return Err(Refusal::Tell(
                            "the upstream let the session in without proving it holds the password"
                                .into(),
                        ));
                    }
                    10 => {
                        let offered: Vec<&[u8]> = body[4..]
                            .split(|b| *b == 0)
                            .filter(|m| !m.is_empty())
                            .collect();
                        if !offered.contains(&b"SCRAM-SHA-256".as_slice()) {
                            return Err(Refusal::Tell(
                                "the upstream offers no SCRAM-SHA-256, the only way the proxy authenticates"
                                    .into(),
                            ));
                        }
                        let c = ScramClient::new("", &up.password).map_err(Refusal::Tell)?;
                        let first = c.client_first();
                        let mut msg = b"SCRAM-SHA-256\0".to_vec();
                        msg.extend((first.len() as u32).to_be_bytes());
                        msg.extend(first.as_bytes());
                        send(&mut upstream, b'p', &msg).await?;
                        scram = Some(c);
                    }
                    11 => {
                        let c = scram.as_mut().ok_or_else(|| {
                            Refusal::Tell("the upstream continued SCRAM it never began".into())
                        })?;
                        let server_first = std::str::from_utf8(&body[4..]).map_err(|_| {
                            Refusal::Tell("the upstream's SCRAM is not text".into())
                        })?;
                        let fin = c.client_final(server_first).map_err(Refusal::Tell)?;
                        send(&mut upstream, b'p', fin.as_bytes()).await?;
                    }
                    12 => {
                        let c = scram.as_ref().ok_or_else(|| {
                            Refusal::Tell("the upstream ended SCRAM it never began".into())
                        })?;
                        let server_final = std::str::from_utf8(&body[4..]).map_err(|_| {
                            Refusal::Tell("the upstream's SCRAM is not text".into())
                        })?;
                        c.verify_server_final(server_final).map_err(Refusal::Tell)?;
                        verified = true;
                    }
                    other => {
                        return Err(Refusal::Tell(format!(
                            "the upstream asked for {}; the proxy authenticates with SCRAM-SHA-256 only",
                            auth_name(other)
                        )));
                    }
                }
            }
            b'E' => {
                let _ = client.write_all(&frame(Some(b'E'), &body)).await;
                return Err(Refusal::Told(format!(
                    "the upstream refused: {}",
                    error_message(&body)
                )));
            }
            // A protocol negotiation answers the client's own startup, which
            // went upstream as the client wrote it: passed on now, since a
            // client takes one only before its `AuthenticationOk`.
            b'v' => client
                .write_all(&frame(Some(b'v'), &body))
                .await
                .map_err(|e| Refusal::Told(e.to_string()))?,
            // A notice waits for the `AuthenticationOk`: before it, a client
            // takes only an authentication request, a negotiation or an
            // error.
            b'N' => held.extend(frame(Some(b'N'), &body)),
            other => {
                return Err(Refusal::Tell(format!(
                    "the upstream sent {:?} before the session was authenticated",
                    other as char
                )));
            }
        }
    }
    let mut ok = frame(Some(b'R'), &0u32.to_be_bytes());
    ok.extend(held);
    client
        .write_all(&ok)
        .await
        .map_err(|e| Refusal::Told(e.to_string()))?;
    Ok(Some(upstream))
}

/// TCP to the upstream, its `SSLRequest` accepted, and TLS verified.
async fn connect(up: &Upstream) -> Result<TlsStream<TcpStream>, String> {
    let name = tls::server_name(&up.host)?;
    let mut tcp = tokio::time::timeout(
        CONNECT_DEADLINE,
        TcpStream::connect((up.host.as_str(), up.port)),
    )
    .await
    .map_err(|_| {
        format!(
            "{}:{} did not answer within {CONNECT_DEADLINE:?}",
            up.host, up.port
        )
    })?
    .map_err(|e| format!("{}:{}: {e}", up.host, up.port))?;
    let mut ssl = 8u32.to_be_bytes().to_vec();
    ssl.extend(SSL_REQUEST.to_be_bytes());
    tcp.write_all(&ssl)
        .await
        .map_err(|e| format!("the upstream: {e}"))?;
    let mut answer = [0u8; 1];
    tcp.read_exact(&mut answer)
        .await
        .map_err(|e| format!("the upstream: {e}"))?;
    if answer[0] != b'S' {
        return Err(
            "the upstream declined TLS, and the proxy speaks to an upstream only over TLS".into(),
        );
    }
    up.tls
        .connect(name, tcp)
        .await
        .map_err(|e| format!("TLS to {}: {e}", up.host))
}

/// A `CancelRequest`, sent to the upstream as it came. Best-effort, as
/// Postgres's own is: the upstream answers nothing.
async fn cancel(packet: &[u8], up: &Upstream) {
    if let Ok(mut upstream) = connect(up).await {
        let _ = upstream.write_all(&frame(None, packet)).await;
        let _ = upstream.shutdown().await;
    }
}

async fn send(upstream: &mut TlsStream<TcpStream>, tag: u8, body: &[u8]) -> Result<(), Refusal> {
    upstream
        .write_all(&frame(Some(tag), body))
        .await
        .map_err(|e| Refusal::Tell(format!("the upstream: {e}")))
}

/// A startup-phase packet, without its length: its code, then the rest.
pub(super) async fn read_startup<R: AsyncRead + Unpin>(r: &mut R) -> Result<Vec<u8>, String> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)
        .await
        .map_err(|e| format!("the client: {e}"))?;
    let len = u32::from_be_bytes(len) as usize;
    if !(8..=MAX_STARTUP).contains(&len) {
        return Err(format!("the client's startup packet is {len} bytes"));
    }
    let mut packet = vec![0u8; len - 4];
    r.read_exact(&mut packet)
        .await
        .map_err(|e| format!("the client: {e}"))?;
    Ok(packet)
}

/// One message: its tag, and its body without the length.
pub(super) async fn read_message<R: AsyncRead + Unpin>(
    r: &mut R,
    max: usize,
) -> Result<(u8, Vec<u8>), String> {
    let mut head = [0u8; 5];
    r.read_exact(&mut head)
        .await
        .map_err(|e| format!("the upstream: {e}"))?;
    let len = u32::from_be_bytes([head[1], head[2], head[3], head[4]]) as usize;
    if !(4..=max).contains(&len) {
        return Err(format!("the upstream sent a message of {len} bytes"));
    }
    let mut body = vec![0u8; len - 4];
    r.read_exact(&mut body)
        .await
        .map_err(|e| format!("the upstream: {e}"))?;
    Ok((head[0], body))
}

/// The startup's parameters: pairs of text, each ended by a NUL, and an
/// empty name to end them.
pub(super) fn parse_params(raw: &[u8]) -> Result<Vec<(String, String)>, String> {
    let mut rest = raw;
    let mut params = Vec::new();
    loop {
        // The end is an empty name with its own NUL: the slice after a last
        // NUL is not one.
        let name = cstr(&mut rest)?;
        if name.is_empty() {
            return Ok(params);
        }
        let value = cstr(&mut rest).map_err(|_| "a startup parameter has no value".to_string())?;
        params.push((name, value));
    }
}

/// One NUL-ended string off the front of `rest`.
fn cstr(rest: &mut &[u8]) -> Result<String, String> {
    let end = rest
        .iter()
        .position(|b| *b == 0)
        .ok_or_else(|| "the startup's parameters do not end".to_string())?;
    let text = String::from_utf8(rest[..end].to_vec())
        .map_err(|_| "a startup parameter is not text".to_string())?;
    *rest = &rest[end + 1..];
    Ok(text)
}

/// A message: its tag (none for a startup packet), its length, its body.
pub(super) fn frame(tag: Option<u8>, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 5);
    out.extend(tag);
    out.extend(((body.len() + 4) as u32).to_be_bytes());
    out.extend(body);
    out
}

/// The proxy's refusal, as Postgres writes a fatal error: what the endpoint
/// answers a connection it does not serve, too.
pub(crate) fn fatal(why: &str) -> Vec<u8> {
    let mut body = Vec::new();
    for (code, text) in [
        (b'S', "FATAL"),
        (b'V', "FATAL"),
        (b'C', "08004"),
        (b'M', why),
    ] {
        body.push(code);
        body.extend(text.as_bytes());
        body.push(0);
    }
    body.push(0);
    frame(Some(b'E'), &body)
}

/// An `ErrorResponse`'s message field.
fn error_message(body: &[u8]) -> String {
    body.split(|b| *b == 0)
        .find_map(|f| f.strip_prefix(b"M"))
        .map(|m| String::from_utf8_lossy(m).into_owned())
        .unwrap_or_else(|| "no reason given".into())
}

fn auth_name(code: u32) -> &'static str {
    match code {
        2 => "Kerberos",
        3 => "a cleartext password",
        5 => "an MD5 password",
        7 | 8 => "GSSAPI",
        9 => "SSPI",
        _ => "a kind of authentication it does not know",
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::DuplexStream;

    use super::super::testing::*;
    use super::*;

    fn upstream(ca: Option<&str>, port: u16, password: &str) -> Upstream {
        Upstream {
            host: "localhost".into(),
            port,
            user: "app".into(),
            password: Zeroizing::new(password.into()),
            tls: tls::connector(ca).unwrap(),
        }
    }

    /// Serve one client of `up` over a pipe; the client's end, and the
    /// session's result.
    fn client_of(up: Upstream) -> (DuplexStream, tokio::task::JoinHandle<Result<(), String>>) {
        let (client, proxy) = tokio::io::duplex(64 * 1024);
        (client, tokio::spawn(async move { serve(proxy, &up).await }))
    }

    /// The whole road: a client that sends no password is let in, the
    /// upstream sees the secret's user (never the client's) and a proof it
    /// checks, and a query goes through and back.
    #[tokio::test]
    async fn a_client_with_no_password_gets_a_session_as_the_secrets_user() {
        let pw = made();
        let (ca, port, mut saw) = fake(Upstreams::Scram(pw.clone())).await;
        let (mut c, session) = client_of(upstream(Some(&ca), port, &pw));
        // An SSL ask first, as libpq makes by default: declined.
        let mut ssl = 8u32.to_be_bytes().to_vec();
        ssl.extend(SSL_REQUEST.to_be_bytes());
        c.write_all(&ssl).await.unwrap();
        let mut n = [0u8; 1];
        c.read_exact(&mut n).await.unwrap();
        assert_eq!(&n, b"N");
        c.write_all(&startup(&[
            ("user", "whoever"),
            ("database", "orders"),
            ("application_name", "psql"),
        ]))
        .await
        .unwrap();
        let (tag, body) = next(&mut c).await;
        assert_eq!(
            (tag, body),
            (b'R', vec![0, 0, 0, 0]),
            "AuthenticationOk, unasked"
        );
        assert_eq!(next(&mut c).await.0, b'S');
        assert_eq!(
            next(&mut c).await,
            (b'K', vec![0, 0, 0, 7, 1, 2, 3, 4]),
            "the upstream's key, unchanged"
        );
        assert_eq!(next(&mut c).await.0, b'Z');
        assert_eq!(
            saw.recv().await.unwrap(),
            Saw::Startup(vec![
                ("user".into(), "app".into()),
                ("database".into(), "orders".into()),
                ("application_name".into(), "psql".into()),
            ])
        );
        c.write_all(&frame(Some(b'Q'), b"select 1\0"))
            .await
            .unwrap();
        assert_eq!(next(&mut c).await.0, b'C');
        assert_eq!(next(&mut c).await.0, b'Z');
        assert_eq!(saw.recv().await.unwrap(), Saw::Query("select 1".into()));
        c.write_all(&frame(Some(b'X'), b"")).await.unwrap();
        drop(c);
        assert!(session.await.unwrap().is_ok());
    }

    /// A wrong password: the upstream's own refusal reaches the client as
    /// the upstream wrote it.
    #[tokio::test]
    async fn the_upstreams_refusal_reaches_the_client_in_its_words() {
        let (ca, port, _saw) = fake(Upstreams::Scram(made())).await;
        let (mut c, session) = client_of(upstream(Some(&ca), port, &made()));
        c.write_all(&startup(&[("database", "orders")]))
            .await
            .unwrap();
        let (tag, body) = next(&mut c).await;
        assert_eq!(tag, b'E');
        assert_eq!(
            error_message(&body),
            "password authentication failed for user \"app\""
        );
        assert!(
            session
                .await
                .unwrap()
                .unwrap_err()
                .contains("password authentication failed")
        );
    }

    /// What the proxy refuses, each before the secret's password could be
    /// handed to anyone: an upstream without TLS, one asking for a
    /// cleartext password, one whose certificate nobody vouches for, one
    /// that cannot sign — and a replication connection, before any upstream
    /// at all.
    #[tokio::test]
    async fn what_the_proxy_refuses_it_refuses_before_the_password_leaves() {
        // No TLS: refused, and nothing follows the SSL ask.
        let pw = made();
        let (ca, port, mut saw) = fake(Upstreams::NoTls).await;
        let (mut c, session) = client_of(upstream(Some(&ca), port, &pw));
        c.write_all(&startup(&[("database", "d")])).await.unwrap();
        let (tag, body) = next(&mut c).await;
        assert_eq!(tag, b'E');
        assert!(
            error_message(&body).contains("declined TLS"),
            "{}",
            error_message(&body)
        );
        assert!(session.await.unwrap().is_err());
        assert_eq!(saw.recv().await.unwrap(), Saw::After(Vec::new()));

        // A cleartext password asked for: refused, and nothing sent after.
        let (ca, port, mut saw) = fake(Upstreams::Cleartext).await;
        let (mut c, session) = client_of(upstream(Some(&ca), port, &pw));
        c.write_all(&startup(&[("database", "d")])).await.unwrap();
        let (_, body) = next(&mut c).await;
        assert!(session.await.unwrap().is_err());
        // What the upstream saw first: a proxy that answered the ask fails
        // HERE, whatever it then told its client.
        assert!(matches!(saw.recv().await.unwrap(), Saw::Startup(_)));
        let after = saw.recv().await.unwrap();
        assert!(
            !matches!(&after, Saw::After(b) if b.windows(pw.len()).any(|w| w == pw.as_bytes())),
            "the password never left: {after:?}"
        );
        assert!(
            error_message(&body).contains("a cleartext password"),
            "{}",
            error_message(&body)
        );

        // A certificate no root vouches for: refused at TLS.
        let (_ca, port, _saw) = fake(Upstreams::Scram(pw.clone())).await;
        let (mut c, session) = client_of(upstream(None, port, &pw));
        c.write_all(&startup(&[("database", "d")])).await.unwrap();
        let (_, body) = next(&mut c).await;
        assert!(
            error_message(&body).starts_with("TLS to localhost"),
            "{}",
            error_message(&body)
        );
        assert!(session.await.unwrap().is_err());

        // A server that cannot sign: refused before the session.
        let (ca, port, _saw) = fake(Upstreams::Forged(pw.clone())).await;
        let (mut c, session) = client_of(upstream(Some(&ca), port, &pw));
        c.write_all(&startup(&[("database", "d")])).await.unwrap();
        let (_, body) = next(&mut c).await;
        assert!(
            error_message(&body).contains("does not hold the password"),
            "{}",
            error_message(&body)
        );
        assert!(session.await.unwrap().is_err());

        // A server that began SCRAM and lets the session in unsigned: it
        // never proved it holds the password either.
        let (ca, port, _saw) = fake(Upstreams::SkipFinal(pw.clone())).await;
        let (mut c, session) = client_of(upstream(Some(&ca), port, &pw));
        c.write_all(&startup(&[("database", "d")])).await.unwrap();
        let (_, body) = next(&mut c).await;
        assert!(
            error_message(&body).contains("without proving it holds the password"),
            "{}",
            error_message(&body)
        );
        assert!(session.await.unwrap().is_err());

        // Replication: refused with no upstream at all, a "no" before it
        // included (Postgres takes the last one a startup names).
        for asks in [
            &[("replication", "database")][..],
            &[("replication", "false"), ("replication", "database")][..],
        ] {
            let (ca, port, mut saw) = fake(Upstreams::Scram(pw.clone())).await;
            let (mut c, session) = client_of(upstream(Some(&ca), port, &pw));
            let mut params = vec![("database", "d")];
            params.extend_from_slice(asks);
            c.write_all(&startup(&params)).await.unwrap();
            let (_, body) = next(&mut c).await;
            assert!(
                error_message(&body).contains("replication"),
                "{asks:?}: {}",
                error_message(&body)
            );
            assert!(session.await.unwrap().is_err());
            assert!(saw.try_recv().is_err(), "no upstream was dialled");
        }
    }

    /// The secret's user ends the startup the upstream reads: one holding a
    /// NUL would name parameters of its own (`replication` among them), and
    /// one that is empty names nobody. Each is refused before any upstream is
    /// dialled.
    #[tokio::test]
    async fn a_user_that_would_name_parameters_of_its_own_is_refused() {
        let pw = made();
        let (ca, port, mut saw) = fake(Upstreams::Scram(pw.clone())).await;
        for user in ["app\0replication\0database", ""] {
            let mut up = upstream(Some(&ca), port, &pw);
            up.user = user.into();
            let (mut c, session) = client_of(up);
            c.write_all(&startup(&[("database", "d")])).await.unwrap();
            let (_, body) = next(&mut c).await;
            assert!(
                error_message(&body).contains("not a name Postgres takes"),
                "{user:?}: {}",
                error_message(&body)
            );
            assert!(session.await.unwrap().is_err());
        }
        assert!(saw.try_recv().is_err(), "no upstream was dialled");
    }

    /// An upstream that trusts the connection asks for nothing, and the
    /// proxy offers nothing: the session starts as it would for psql.
    #[tokio::test]
    async fn a_trusting_upstream_is_told_nothing() {
        let (ca, port, _saw) = fake(Upstreams::Trusting).await;
        let (mut c, _session) = client_of(upstream(Some(&ca), port, &made()));
        c.write_all(&startup(&[("database", "d")])).await.unwrap();
        assert_eq!(next(&mut c).await, (b'R', vec![0, 0, 0, 0]));
        assert_eq!(next(&mut c).await.0, b'S');
    }

    /// What the upstream says while the proxy authenticates reaches the
    /// client where a client reads it: a protocol negotiation before the
    /// `AuthenticationOk` (libpq takes one only there), a notice after it
    /// (libpq takes none before).
    #[tokio::test]
    async fn a_negotiation_reaches_the_client_before_its_session_and_a_notice_after() {
        let pw = made();
        let (ca, port, _saw) = fake(Upstreams::Negotiates(pw.clone())).await;
        let (mut c, session) = client_of(upstream(Some(&ca), port, &pw));
        c.write_all(&startup(&[("database", "d"), ("_pq_.x", "1")]))
            .await
            .unwrap();
        let mut tags = Vec::new();
        while tags.last() != Some(&b'Z') {
            tags.push(next(&mut c).await.0);
        }
        assert_eq!(String::from_utf8(tags).unwrap(), "vRNSKZ");
        c.write_all(&frame(Some(b'X'), b"")).await.unwrap();
        drop(c);
        assert!(session.await.unwrap().is_ok());
    }

    /// A cancel goes to the upstream as it came, over TLS.
    #[tokio::test]
    async fn a_cancel_goes_upstream_as_it_came() {
        let pw = made();
        let (ca, port, mut saw) = fake(Upstreams::Scram(pw.clone())).await;
        let (mut c, session) = client_of(upstream(Some(&ca), port, &pw));
        let mut body = CANCEL_REQUEST.to_be_bytes().to_vec();
        body.extend([0, 0, 0, 7, 1, 2, 3, 4]);
        c.write_all(&frame(None, &body)).await.unwrap();
        assert!(session.await.unwrap().is_ok());
        assert_eq!(saw.recv().await.unwrap(), Saw::Cancel(body));
    }

    #[test]
    fn startup_parameters_read_as_postgres_writes_them() {
        assert_eq!(
            parse_params(b"user\0a\0database\0b\0\0").unwrap(),
            vec![("user".into(), "a".into()), ("database".into(), "b".into())]
        );
        assert!(parse_params(b"user\0a\0").is_err(), "no end");
        assert!(parse_params(b"user\0").is_err(), "no value");
        assert!(parse_params(b"us\xffer\0a\0\0").is_err(), "not text");
    }
}
