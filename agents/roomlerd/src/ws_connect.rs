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

use std::net::IpAddr;
use std::time::Duration;

use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::{self, handshake::client::Response, http::Uri};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, client_async_tls_with_config, connect_async,
};

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

/// `connect_async`, with the host's address from `ctl_dns`. An address literal,
/// or a name nothing knows, takes `connect_async`'s own path unchanged.
pub async fn connect(url: &str) -> Result<(Ws, Response), tungstenite::Error> {
    let Some((host, port)) = host_port(url) else {
        return connect_async(url).await;
    };
    if host.trim_matches(['[', ']']).parse::<IpAddr>().is_ok() {
        return connect_async(url).await;
    }
    let hostport = tunnel_core::ctl_dns::hostport(&host, port);
    let addrs = tunnel_core::ctl_dns::resolve(&hostport).await;
    if addrs.is_empty() {
        return connect_async(url).await;
    }
    let mut last = None;
    for addr in &addrs {
        match tokio::time::timeout(PER_ADDR_CONNECT, TcpStream::connect(addr)).await {
            Ok(Ok(stream)) => {
                let done = client_async_tls_with_config(url, stream, None, None).await;
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
fn host_port(url: &str) -> Option<(String, u16)> {
    let uri: Uri = url.parse().ok()?;
    let host = uri.host()?.to_string();
    let port = uri.port_u16().unwrap_or(match uri.scheme_str() {
        Some("wss") | Some("https") => 443,
        _ => 80,
    });
    Some((host, port))
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
        assert_eq!(
            host_port("wss://roomler.ai/ws?token=x&role=agent&tid=y"),
            Some(("roomler.ai".into(), 443))
        );
        assert_eq!(
            host_port("ws://relay.example:8080/derp"),
            Some(("relay.example".into(), 8080))
        );
        assert_eq!(host_port("not a url"), None);
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
        let (mut ws, _) = tokio::time::timeout(Duration::from_secs(10), connect(&url))
            .await
            .expect("must not wait on the resolver")
            .expect("connects to the remembered address");
        let first = ws.next().await.unwrap().unwrap();
        assert_eq!(first, Message::Text("hello".into()));
        assert_eq!(*host_seen.lock().unwrap(), hostport, "Host keeps the name");
    }
}
