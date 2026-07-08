//! Phase 2 MagicDNS — a tiny split-DNS resolver.
//!
//! Overlay names (`<node>.<magic_domain>`, or a bare `<node>` label the OS sends
//! with the search domain) are answered locally from the netmap's name→overlay-IP
//! map; every other query is forwarded to an upstream nameserver and relayed
//! back verbatim. The runtime binds it to the node's own overlay IP on :53 and
//! points the roomler interface's DNS there (Tailscale uses a dedicated
//! `100.100.100.100`; the node's own overlay IP avoids an extra NIC-address
//! assignment for v1).
//!
//! Hand-rolled A-record codec — no DNS-library dependency. We only need to parse
//! the first question and build a single `A` answer for a hit; misses relay the
//! raw bytes upstream, so the full RR machinery never has to exist here.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use tokio::net::UdpSocket;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

/// Shared `node-name → overlay-IPv4` map, updated by the runtime as the netmap
/// changes (includes the node itself).
pub type NameMap = Arc<RwLock<HashMap<String, Ipv4Addr>>>;

/// Resolver configuration. Cheap to clone (the map is an `Arc`), so each query
/// gets its own task without blocking the receive loop on a slow upstream.
#[derive(Clone)]
pub struct DnsConfig {
    /// Where to bind + serve (the node's overlay IP on :53).
    pub bind: SocketAddr,
    /// Tenant DNS suffix, lowercased, no trailing dot (e.g. `myorg.roomler.net`).
    pub magic_domain: String,
    /// Upstream resolver for non-overlay queries.
    pub upstream: SocketAddr,
    /// name → overlay IP.
    pub names: NameMap,
}

/// Serve until the socket errors (or the task is dropped). Best-effort: a bind
/// failure (needs :53 privileges + the address on the NIC) logs and returns, so
/// the overlay keeps working without DNS.
pub async fn run(cfg: DnsConfig) {
    let sock = match UdpSocket::bind(cfg.bind).await {
        Ok(s) => Arc::new(s),
        Err(e) => {
            warn!(bind = %cfg.bind, %e, "magicdns: bind failed; resolver off");
            return;
        }
    };
    info!(bind = %cfg.bind, domain = %cfg.magic_domain, "magicdns: resolver up");
    let mut buf = [0u8; 1500];
    loop {
        let (n, from) = match sock.recv_from(&mut buf).await {
            Ok(v) => v,
            Err(e) => {
                warn!(%e, "magicdns: recv failed; resolver exiting");
                return;
            }
        };
        let query = buf[..n].to_vec();
        let sock = sock.clone();
        let cfg = cfg.clone();
        // Per-query task so a slow upstream can't stall other queries.
        tokio::spawn(async move {
            if let Some(resp) = build_response(&query, &cfg).await {
                let _ = sock.send_to(&resp, from).await;
            }
        });
    }
}

struct Question {
    /// Lowercased, dot-joined, no trailing dot.
    qname: String,
    qtype: u16,
    /// Offset just past the question (start of the answer section).
    qend: usize,
}

/// Decide the reply for one raw query.
async fn build_response(query: &[u8], cfg: &DnsConfig) -> Option<Vec<u8>> {
    let q = parse_question(query)?;
    // Only intercept address queries (A=1, AAAA=28); pass everything else on.
    if q.qtype == 1 || q.qtype == 28 {
        let domain = cfg.magic_domain.trim_end_matches('.').to_ascii_lowercase();

        // In our zone: `<label>.<domain>` — we're authoritative, so a miss is
        // NXDOMAIN (never leaks the overlay name upstream).
        let in_zone_label = if domain.is_empty() {
            None
        } else {
            q.qname
                .strip_suffix(&format!(".{domain}"))
                .filter(|p| !p.is_empty())
        };
        if let Some(label) = in_zone_label {
            let names = cfg.names.read().await;
            return Some(match names.get(label) {
                Some(ip) if q.qtype == 1 => build_a(query, q.qend, *ip),
                Some(_) => build_status(query, q.qend, 0), // AAAA → NODATA
                None => build_status(query, q.qend, 3),    // NXDOMAIN
            });
        }

        // Bare single label (search-domain style): answer only if it's a known
        // node; otherwise fall through to upstream (might be a real LAN host).
        if !q.qname.contains('.') && !q.qname.is_empty() {
            let names = cfg.names.read().await;
            if let Some(ip) = names.get(&q.qname) {
                return Some(if q.qtype == 1 {
                    build_a(query, q.qend, *ip)
                } else {
                    build_status(query, q.qend, 0) // AAAA → NODATA
                });
            }
        }
    }
    forward_upstream(query, cfg.upstream).await
}

/// Parse the header + first question. `None` on a malformed / compressed
/// question (questions never use name compression).
fn parse_question(msg: &[u8]) -> Option<Question> {
    if msg.len() < 12 {
        return None;
    }
    let qdcount = u16::from_be_bytes([msg[4], msg[5]]);
    if qdcount < 1 {
        return None;
    }
    let mut pos = 12usize;
    let mut labels: Vec<String> = Vec::new();
    loop {
        let len = *msg.get(pos)? as usize;
        if len == 0 {
            pos += 1;
            break;
        }
        if len & 0xC0 != 0 {
            return None; // pointer/compression not valid in a question
        }
        pos += 1;
        let end = pos.checked_add(len)?;
        let label = msg.get(pos..end)?;
        labels.push(std::str::from_utf8(label).ok()?.to_ascii_lowercase());
        pos = end;
    }
    let qtype = u16::from_be_bytes([*msg.get(pos)?, *msg.get(pos + 1)?]);
    let qend = pos.checked_add(4)?; // qtype(2) + qclass(2)
    if qend > msg.len() {
        return None;
    }
    Some(Question {
        qname: labels.join("."),
        qtype,
        qend,
    })
}

/// Response header + echoed question, `ancount` answers, `rcode` status.
fn resp_header_and_question(query: &[u8], qend: usize, ancount: u16, rcode: u8) -> Vec<u8> {
    let mut out = Vec::with_capacity(qend + 16);
    out.extend_from_slice(&query[0..2]); // ID
    out.push(query[2] | 0x84); // QR=1, AA=1 (opcode + RD preserved)
    out.push(0x80 | (rcode & 0x0F)); // RA=1 + RCODE
    out.extend_from_slice(&[0, 1]); // QDCOUNT = 1
    out.extend_from_slice(&ancount.to_be_bytes());
    out.extend_from_slice(&[0, 0, 0, 0]); // NSCOUNT + ARCOUNT
    out.extend_from_slice(&query[12..qend]); // the question, verbatim
    out
}

/// Positive `A` answer (name-compression pointer back to the question).
fn build_a(query: &[u8], qend: usize, ip: Ipv4Addr) -> Vec<u8> {
    let mut out = resp_header_and_question(query, qend, 1, 0);
    out.extend_from_slice(&[0xC0, 0x0C]); // NAME → question at offset 12
    out.extend_from_slice(&[0, 1]); // TYPE A
    out.extend_from_slice(&[0, 1]); // CLASS IN
    out.extend_from_slice(&[0, 0, 0, 60]); // TTL 60s
    out.extend_from_slice(&[0, 4]); // RDLENGTH
    out.extend_from_slice(&ip.octets());
    out
}

/// Answer with no records — `rcode` 0 = NODATA (name exists, no A of this type),
/// 3 = NXDOMAIN.
fn build_status(query: &[u8], qend: usize, rcode: u8) -> Vec<u8> {
    resp_header_and_question(query, qend, 0, rcode)
}

/// Relay a non-overlay query to the upstream resolver and return its reply.
/// 3 s timeout so a dead upstream can't wedge the per-query task.
async fn forward_upstream(query: &[u8], upstream: SocketAddr) -> Option<Vec<u8>> {
    let sock = UdpSocket::bind(("0.0.0.0", 0)).await.ok()?;
    sock.send_to(query, upstream).await.ok()?;
    let mut buf = vec![0u8; 1500];
    let n = tokio::time::timeout(Duration::from_secs(3), sock.recv(&mut buf))
        .await
        .ok()?
        .ok()?;
    buf.truncate(n);
    debug!(bytes = n, %upstream, "magicdns: relayed upstream reply");
    Some(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal A/IN query for `name` (labels, no compression).
    fn query_for(name: &str, qtype: u16) -> Vec<u8> {
        let mut m = vec![0x12, 0x34, 0x01, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0];
        for label in name.split('.') {
            m.push(label.len() as u8);
            m.extend_from_slice(label.as_bytes());
        }
        m.push(0);
        m.extend_from_slice(&qtype.to_be_bytes());
        m.extend_from_slice(&[0, 1]); // CLASS IN
        m
    }

    #[test]
    fn parses_question_name_and_type() {
        let q = parse_question(&query_for("neo16.myorg.roomler.net", 1)).unwrap();
        assert_eq!(q.qname, "neo16.myorg.roomler.net");
        assert_eq!(q.qtype, 1);
    }

    #[test]
    fn a_answer_is_well_formed() {
        let query = query_for("neo16.myorg.roomler.net", 1);
        let q = parse_question(&query).unwrap();
        let resp = build_a(&query, q.qend, Ipv4Addr::new(100, 64, 0, 7));
        assert_eq!(&resp[0..2], &query[0..2]); // ID echoed
        assert_eq!(resp[2] & 0x80, 0x80); // QR set
        assert_eq!(u16::from_be_bytes([resp[6], resp[7]]), 1); // ANCOUNT
        // Last 4 bytes are the A record's IP.
        assert_eq!(&resp[resp.len() - 4..], &[100, 64, 0, 7]);
        // Name pointer to the question.
        assert_eq!(&resp[q.qend..q.qend + 2], &[0xC0, 0x0C]);
    }

    #[tokio::test]
    async fn in_zone_hit_answers_a_and_miss_is_nxdomain() {
        let mut map = HashMap::new();
        map.insert("neo16".to_string(), Ipv4Addr::new(100, 64, 0, 7));
        let cfg = DnsConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            magic_domain: "myorg.roomler.net".into(),
            upstream: "127.0.0.1:0".parse().unwrap(),
            names: Arc::new(RwLock::new(map)),
        };

        let hit = build_response(&query_for("neo16.myorg.roomler.net", 1), &cfg)
            .await
            .unwrap();
        assert_eq!(u16::from_be_bytes([hit[6], hit[7]]), 1); // one answer
        assert_eq!(&hit[hit.len() - 4..], &[100, 64, 0, 7]);

        let miss = build_response(&query_for("ghost.myorg.roomler.net", 1), &cfg)
            .await
            .unwrap();
        assert_eq!(miss[3] & 0x0F, 3); // NXDOMAIN
        assert_eq!(u16::from_be_bytes([miss[6], miss[7]]), 0); // no answers
    }

    #[tokio::test]
    async fn bare_known_label_resolves() {
        let mut map = HashMap::new();
        map.insert("neo16".to_string(), Ipv4Addr::new(100, 64, 0, 9));
        let cfg = DnsConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            magic_domain: "myorg.roomler.net".into(),
            upstream: "127.0.0.1:0".parse().unwrap(),
            names: Arc::new(RwLock::new(map)),
        };
        let resp = build_response(&query_for("neo16", 1), &cfg).await.unwrap();
        assert_eq!(&resp[resp.len() - 4..], &[100, 64, 0, 9]);
    }
}
