// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! Client-IP extraction that survives our reverse-proxy chain — the server's
//! ONE notion of "the client's address".
//!
//! `X-Forwarded-For` is append-only and anyone may seed it. A client that
//! sends `X-Forwarded-For: 1.2.3.4` makes the header arrive as
//! `1.2.3.4, <real client>, <our hop>`, so trusting the **left-most** entry —
//! which is what `tower_governor`'s `SmartIpKeyExtractor` does — hands every
//! caller a free rate-limit bypass with one header. Verified against prod on
//! 2026-07-28: a throttled `POST /api/auth/login` went straight back to 401
//! simply by adding `-H 'X-Forwarded-For: 203.0.113.77'`.
//!
//! Only the right-most entries are evidence, because only our own proxies
//! appended them. The prod chain is
//! `client → docker-nginx (records the client) → pod-nginx (appends its peer)`,
//! i.e. one hop is ours, so the client sits at `len - 2`.
//!
//! Every reader of a client address goes through here: the per-IP governor
//! and the credential gate (the api crate's `middleware::client_ip`, which
//! re-exports these), the `/ws` analytics session, and since FR-102 P3c the
//! install trace's internal-network check in `fleet` — which is why this
//! lives in core (FR-69: a module never names the api crate). Two rules for
//! one question would let a client be "internal" to one reader and someone
//! else to another.

use std::{
    convert::Infallible,
    net::{IpAddr, SocketAddr},
};

use axum::{
    extract::{ConnectInfo, FromRequestParts},
    http::{HeaderMap, Request, request::Parts},
};

const X_FORWARDED_FOR: &str = "x-forwarded-for";

/// Pick the client address out of `X-Forwarded-For`.
///
/// `trusted_hops` is how many entries our own proxies appended. Those are
/// dropped from the right and the next one is taken: the address the
/// outermost proxy *we control* actually observed.
///
/// Returns `None` — meaning "fall back to the peer address" — when the header
/// is missing, unparseable, or shorter than the trusted suffix. A short header
/// means the request did not traverse the expected chain, so nothing in it has
/// been vouched for. `trusted_hops == 0` (direct exposure, e.g. integration
/// tests) ignores the header outright.
pub fn client_ip_from_headers(headers: &HeaderMap, trusted_hops: usize) -> Option<IpAddr> {
    if trusted_hops == 0 {
        return None;
    }
    let raw = headers.get(X_FORWARDED_FOR)?.to_str().ok()?;
    let hops: Vec<&str> = raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    // Drop our own hops, then take the last remaining entry.
    let idx = hops.len().checked_sub(trusted_hops + 1)?;
    hops.get(idx)?.parse().ok()
}

/// The vouched-for forwarded entry, else the socket peer the listener
/// recorded (`None` when neither exists — a request built in-process).
pub fn client_ip_or_peer(
    headers: &HeaderMap,
    peer: Option<SocketAddr>,
    trusted_hops: usize,
) -> Option<IpAddr> {
    client_ip_from_headers(headers, trusted_hops).or_else(|| peer.map(|p| p.ip()))
}

/// Resolve the client address for a request, preferring the vouched-for
/// forwarded entry and falling back to the socket peer.
pub fn client_ip<T>(req: &Request<T>, trusted_hops: usize) -> Option<IpAddr> {
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(addr)| *addr);
    client_ip_or_peer(req.headers(), peer, trusted_hops)
}

/// The socket peer the listener recorded (`into_make_service_with_connect_info`),
/// or `None` — an extractor that never refuses, so a handler that only
/// *counts* something cannot fail a request over a missing extension.
/// Pair it with the request's headers through [`client_ip_or_peer`].
#[derive(Debug, Clone, Copy)]
pub struct PeerAddr(pub Option<SocketAddr>);

impl<S> FromRequestParts<S> for PeerAddr
where
    S: Send + Sync,
{
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(PeerAddr(
            parts
                .extensions
                .get::<ConnectInfo<SocketAddr>>()
                .map(|ConnectInfo(addr)| *addr),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(xff: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(X_FORWARDED_FOR, xff.parse().unwrap());
        h
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    /// The prod shape: docker-nginx recorded the client, pod-nginx appended
    /// itself.
    #[test]
    fn picks_client_behind_one_trusted_hop() {
        assert_eq!(
            client_ip_from_headers(&headers("203.0.113.9, 10.10.0.1"), 1),
            Some(ip("203.0.113.9"))
        );
    }

    /// The bug this module exists for: a spoofed left-most entry must be
    /// ignored, and the caller must land on their real address.
    #[test]
    fn spoofed_prefix_is_ignored() {
        let h = headers("1.2.3.4, 203.0.113.9, 10.10.0.1");
        assert_eq!(client_ip_from_headers(&h, 1), Some(ip("203.0.113.9")));

        // However many entries the attacker stuffs in, the answer is stable.
        let h = headers("9.9.9.9, 8.8.8.8, 1.1.1.1, 203.0.113.9, 10.10.0.1");
        assert_eq!(client_ip_from_headers(&h, 1), Some(ip("203.0.113.9")));
    }

    /// A header with only our own hop carries no client evidence, so the
    /// caller must fall through to the peer address rather than key everyone
    /// onto the proxy's IP (which would be one shared bucket for the world).
    #[test]
    fn header_without_client_entry_falls_back() {
        assert_eq!(client_ip_from_headers(&headers("10.10.0.1"), 1), None);
        assert_eq!(client_ip_from_headers(&HeaderMap::new(), 1), None);
    }

    /// Direct exposure: nothing in the header was added by us.
    #[test]
    fn zero_trusted_hops_ignores_header() {
        assert_eq!(client_ip_from_headers(&headers("1.2.3.4"), 0), None);
    }

    #[test]
    fn deeper_chains_and_junk() {
        // Two of our own hops.
        let h = headers("203.0.113.9, 172.17.0.2, 10.10.0.1");
        assert_eq!(client_ip_from_headers(&h, 2), Some(ip("203.0.113.9")));
        // Padding and IPv6 survive the round trip.
        let h = headers("  2001:db8::1 ,  10.10.0.1 ");
        assert_eq!(client_ip_from_headers(&h, 1), Some(ip("2001:db8::1")));
        // A non-address in the client slot is not usable.
        assert_eq!(
            client_ip_from_headers(&headers("unknown, 10.10.0.1"), 1),
            None
        );
    }

    /// The fallback both the limiter and the install trace take: the
    /// vouched-for entry when there is one, the peer otherwise.
    #[test]
    fn the_peer_is_the_fallback_and_never_overrides_the_forwarded_entry() {
        let peer: SocketAddr = "127.0.0.1:5555".parse().unwrap();
        assert_eq!(
            client_ip_or_peer(&headers("203.0.113.9, 10.10.0.1"), Some(peer), 1),
            Some(ip("203.0.113.9"))
        );
        assert_eq!(
            client_ip_or_peer(&HeaderMap::new(), Some(peer), 1),
            Some(ip("127.0.0.1"))
        );
        assert_eq!(
            client_ip_or_peer(&headers("203.0.113.9"), Some(peer), 1),
            Some(ip("127.0.0.1")),
            "a lone entry is the client's own claim, not our hop's record"
        );
        assert_eq!(client_ip_or_peer(&HeaderMap::new(), None, 1), None);
        // `client_ip` over a request reads the same two inputs.
        let mut req = Request::builder()
            .header(X_FORWARDED_FOR, "1.2.3.4, 198.51.100.7, 10.10.0.1")
            .body(())
            .unwrap();
        req.extensions_mut().insert(ConnectInfo(peer));
        assert_eq!(client_ip(&req, 1), Some(ip("198.51.100.7")));
    }
}
