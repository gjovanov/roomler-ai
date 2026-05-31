//! Minimal STUN client (RFC 5389) — just enough to discover a
//! **server-reflexive** candidate (our public `ip:port` mapping behind
//! a NAT) for QUIC hole-punching. We send a Binding Request and parse
//! the XOR-MAPPED-ADDRESS out of the Binding Success Response.
//!
//! Deliberately tiny + dependency-light (no `stun`/`webrtc-stun` crate):
//! one request type, one attribute, IPv4 only. The query MUST run on the
//! SAME UDP socket the QUIC endpoint will later use, or the discovered
//! NAT mapping won't be the one QUIC traffic traverses — see the
//! socket-sharing constructors in [`crate::transport::quic`].

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::net::UdpSocket;

/// STUN magic cookie (RFC 5389 §6).
const MAGIC_COOKIE: u32 = 0x2112_A442;
const BINDING_REQUEST: u16 = 0x0001;
const BINDING_SUCCESS: u16 = 0x0101;
const ATTR_XOR_MAPPED_ADDRESS: u16 = 0x0020;

/// Encode a 20-byte STUN Binding Request with the given transaction id
/// and no attributes.
pub fn encode_binding_request(txn_id: [u8; 12]) -> [u8; 20] {
    let mut buf = [0u8; 20];
    buf[0..2].copy_from_slice(&BINDING_REQUEST.to_be_bytes());
    buf[2..4].copy_from_slice(&0u16.to_be_bytes()); // message length = 0
    buf[4..8].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
    buf[8..20].copy_from_slice(&txn_id);
    buf
}

/// Parse the XOR-MAPPED-ADDRESS (IPv4) out of a STUN Binding Success
/// Response, returning the de-XORed public `ip:port`. Returns `None`
/// for non-success messages, a missing/short attribute, or IPv6 (out of
/// scope for v1 — its XOR uses the cookie || transaction-id).
pub fn parse_xor_mapped_address(resp: &[u8]) -> Option<SocketAddr> {
    if resp.len() < 20 {
        return None;
    }
    if u16::from_be_bytes([resp[0], resp[1]]) != BINDING_SUCCESS {
        return None;
    }
    let msg_len = u16::from_be_bytes([resp[2], resp[3]]) as usize;
    let body_end = 20usize.checked_add(msg_len)?.min(resp.len());
    let cookie = MAGIC_COOKIE.to_be_bytes();

    let mut i = 20;
    while i + 4 <= body_end {
        let attr_type = u16::from_be_bytes([resp[i], resp[i + 1]]);
        let attr_len = u16::from_be_bytes([resp[i + 2], resp[i + 3]]) as usize;
        let val = i + 4;
        if val + attr_len > resp.len() {
            break;
        }
        if attr_type == ATTR_XOR_MAPPED_ADDRESS && attr_len >= 8 {
            // value: reserved(1) family(1) x-port(2) x-addr(4, IPv4)
            let family = resp[val + 1];
            let xport = u16::from_be_bytes([resp[val + 2], resp[val + 3]]);
            let port = xport ^ ((MAGIC_COOKIE >> 16) as u16);
            if family == 0x01 {
                let a = Ipv4Addr::new(
                    resp[val + 4] ^ cookie[0],
                    resp[val + 5] ^ cookie[1],
                    resp[val + 6] ^ cookie[2],
                    resp[val + 7] ^ cookie[3],
                );
                return Some(SocketAddr::new(IpAddr::V4(a), port));
            }
        }
        // Attributes are padded to a 4-byte boundary.
        i = val + attr_len.div_ceil(4) * 4;
    }
    None
}

/// Query `stun_server` over `socket` for our server-reflexive
/// candidate. Sends a Binding Request, awaits the matching Success
/// Response (source + transaction-id checked), and returns the de-XORed
/// public `ip:port`. Retries a few times since STUN rides UDP. Run this
/// on the SAME socket the QUIC endpoint will use so the mapping matches.
pub async fn srflx_query(
    socket: &UdpSocket,
    stun_server: SocketAddr,
    attempt_timeout: Duration,
) -> Result<SocketAddr> {
    let mut last_err = String::from("no attempts made");
    for _ in 0..3 {
        let txn_id: [u8; 12] = rand::random();
        let req = encode_binding_request(txn_id);
        socket
            .send_to(&req, stun_server)
            .await
            .context("stun: send binding request")?;

        let mut buf = [0u8; 512];
        match tokio::time::timeout(attempt_timeout, socket.recv_from(&mut buf)).await {
            Ok(Ok((n, from))) => {
                if from != stun_server {
                    last_err = format!("stun: reply from unexpected source {from}");
                    continue;
                }
                let resp = &buf[..n];
                // Confirm the transaction id echoes ours (bytes 8..20).
                if resp.len() < 20 || resp[8..20] != txn_id {
                    last_err = "stun: transaction-id mismatch".into();
                    continue;
                }
                if let Some(srflx) = parse_xor_mapped_address(resp) {
                    return Ok(srflx);
                }
                last_err = "stun: no XOR-MAPPED-ADDRESS in success response".into();
            }
            Ok(Err(e)) => last_err = format!("stun: recv error: {e}"),
            Err(_) => last_err = "stun: response timed out".into(),
        }
    }
    bail!("stun srflx query failed after 3 attempts: {last_err}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a Binding Success Response carrying an XOR-MAPPED-ADDRESS
    /// for `ip:port`, XOR-encoded per RFC 5389 §15.2.
    fn fake_success_response(txn: [u8; 12], ip: [u8; 4], port: u16) -> Vec<u8> {
        let cookie = MAGIC_COOKIE.to_be_bytes();
        let xport = port ^ ((MAGIC_COOKIE >> 16) as u16);
        let mut resp = Vec::new();
        resp.extend_from_slice(&BINDING_SUCCESS.to_be_bytes());
        resp.extend_from_slice(&12u16.to_be_bytes()); // one 12-byte attr
        resp.extend_from_slice(&cookie);
        resp.extend_from_slice(&txn);
        resp.extend_from_slice(&ATTR_XOR_MAPPED_ADDRESS.to_be_bytes());
        resp.extend_from_slice(&8u16.to_be_bytes());
        resp.push(0); // reserved
        resp.push(0x01); // family IPv4
        resp.extend_from_slice(&xport.to_be_bytes());
        resp.extend_from_slice(&[
            ip[0] ^ cookie[0],
            ip[1] ^ cookie[1],
            ip[2] ^ cookie[2],
            ip[3] ^ cookie[3],
        ]);
        resp
    }

    #[test]
    fn binding_request_has_magic_cookie_and_txn() {
        let txn = [9u8; 12];
        let req = encode_binding_request(txn);
        assert_eq!(u16::from_be_bytes([req[0], req[1]]), BINDING_REQUEST);
        assert_eq!(u16::from_be_bytes([req[2], req[3]]), 0, "no attributes");
        assert_eq!(
            u32::from_be_bytes([req[4], req[5], req[6], req[7]]),
            MAGIC_COOKIE
        );
        assert_eq!(&req[8..20], &txn);
    }

    #[test]
    fn parses_xor_mapped_ipv4() {
        let resp = fake_success_response([0u8; 12], [192, 0, 2, 10], 4096);
        let got = parse_xor_mapped_address(&resp).expect("must parse srflx");
        assert_eq!(got, SocketAddr::from(([192, 0, 2, 10], 4096)));
    }

    #[test]
    fn rejects_non_success_message() {
        // A Binding Request is not a Success Response.
        let req = encode_binding_request([7u8; 12]);
        assert!(parse_xor_mapped_address(&req).is_none());
    }

    /// Full query path against a controlled fake STUN server (no real
    /// network): exercises send → recv → source-check → txn-id-check →
    /// parse, end to end.
    #[tokio::test]
    async fn srflx_query_against_fake_stun_server() {
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let server_addr = server.local_addr().unwrap();
        tokio::spawn(async move {
            let mut buf = [0u8; 512];
            let (_n, from) = server.recv_from(&mut buf).await.unwrap();
            let txn: [u8; 12] = buf[8..20].try_into().unwrap();
            let resp = fake_success_response(txn, [198, 51, 100, 7], 5000);
            server.send_to(&resp, from).await.unwrap();
        });

        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let srflx = srflx_query(&client, server_addr, Duration::from_secs(2))
            .await
            .expect("query must succeed against fake server");
        assert_eq!(srflx, SocketAddr::from(([198, 51, 100, 7], 5000)));
    }
}
