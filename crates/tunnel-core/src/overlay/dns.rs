// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
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
//! Hand-rolled A/AAAA-record codec — no DNS-library dependency. We only need to
//! parse the first question and build a single answer for a hit; misses relay
//! the raw bytes upstream, so the full RR machinery never has to exist here.
//!
//! Dual-stack: an `AAAA` query for a known node answers its **derived** overlay
//! IPv6 ([`derive_overlay_v6`]) — same map, no v6 state. Default-on;
//! `ROOMLERD_DNS_AAAA=0` (read by the runtime into
//! [`DnsConfig::answer_aaaa`]) reverts to A-only, the mixed-fleet escape hatch:
//! an old peer's OS doesn't own its derived v6, so v6 toward it blackholes —
//! happy-eyeballs apps fall back to A, strictly-sequential ones may hang.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};

use super::router::derive_overlay_v6;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::UdpSocket;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

/// Shared `node-name → overlay-IPv4` map, updated by the runtime as the netmap
/// changes (includes the node itself).
pub type NameMap = Arc<RwLock<HashMap<String, Ipv4Addr>>>;

/// FR-72 P6 — the label this resolver answers for its own liveness probe:
/// `<PROBE_LABEL>.<magic_domain>`. Underscore-prefixed so it can never collide
/// with a node name (`dns_label` never produces one).
///
/// ⚠️ The hosts fallback must never write this name. Resolving it proves the OS
/// reached THIS resolver; if the fallback could satisfy it, the probe would be
/// answering itself and the ladder could never climb back to DNS.
pub const PROBE_LABEL: &str = "_roomler-probe";

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
    /// Answer `AAAA` for known nodes with their derived overlay IPv6. `false`
    /// (the `ROOMLERD_DNS_AAAA=0` escape hatch) answers NODATA instead.
    pub answer_aaaa: bool,
}

/// Parse an upstream nameserver — accepts a bare IP (`"1.1.1.1"`, defaults to
/// port 53) or `host:port` (`"1.1.1.1:53"`). `None` if neither parses.
pub fn parse_upstream(s: &str) -> Option<SocketAddr> {
    if let Ok(sa) = s.parse::<SocketAddr>() {
        return Some(sa);
    }
    let ip: std::net::IpAddr = s.parse().ok()?;
    Some(SocketAddr::new(ip, 53))
}

/// Serve until the socket errors (or the task is dropped). Best-effort about the
/// bind (it needs `:53` privileges + the address on the NIC) — but it RETRIES
/// rather than giving up, so a transient holder cannot cost the whole process.
///
/// FR-72 P2. The bind used to log once and return, which made one momentary
/// conflict permanent: the resolver stayed off for the daemon's entire lifetime,
/// with nothing scheduled to try again. Field-measured on the dev box, where a
/// daemon restart raced its own dying predecessor for the port and lost:
///
/// ```text
/// 20:57:13  INFO magicdns: resolver up               bind=<a>:53   (org A)
/// 20:57:13  INFO magicdns: resolver up               bind=<b>:53   (org B)
/// 21:03:17  WARN magicdns: bind failed; resolver off bind=<b>:53   AddressAlreadyInUse
/// 21:03:18  WARN magicdns: bind failed; resolver off bind=<a>:53   AddressAlreadyInUse
/// ```
///
/// Both orgs, five hours dead — while a probe minutes later bound the same
/// address on the first try. Nothing was holding it any more; nothing asked.
pub async fn run(cfg: DnsConfig, bound: tokio::sync::watch::Sender<bool>) {
    let sock = {
        let mut attempt: u32 = 0;
        loop {
            match UdpSocket::bind(cfg.bind).await {
                Ok(s) => break Arc::new(s),
                Err(e) => {
                    // WARN once, then DEBUG: a permanently-held port (a real DNS
                    // server on the host) must not write a WARN every 30 s
                    // forever, but the first failure is worth seeing.
                    if attempt == 0 {
                        // Report not-bound so the runtime withholds the OS steer.
                        // Steering at a resolver that never bound blackholes the
                        // magic domain host-wide (NRPT is registry-global).
                        let _ = bound.send(false);
                        warn!(bind = %cfg.bind, %e, "magicdns: bind failed; retrying");
                    } else {
                        debug!(bind = %cfg.bind, %e, attempt, "magicdns: bind still failing");
                    }
                    // Retry FOREVER, capped at 60 s. A squatter that goes away —
                    // the case actually measured — then heals itself, and the
                    // cost of being wrong is one timer per minute. The task is
                    // aborted by the runtime teardown, so this cannot outlive it.
                    let backoff = Duration::from_secs(2u64.saturating_pow(attempt.min(5)).min(60));
                    attempt = attempt.saturating_add(1);
                    tokio::time::sleep(backoff).await;
                }
            }
        }
    };
    // P5 S4b — tell the runtime the resolver is actually listening, so it only
    // steers the "." catch-all at this address when there IS a live resolver.
    // Steering "." at a dead :53 would blackhole ALL DNS (worse than the leak
    // we're closing), so the exit-DNS steer is gated on this.
    //
    // ⚠️ This can now fire LATE — after the runtime's initial wait gave up. The
    // runtime therefore keeps watching it and installs the steer when it flips,
    // instead of reading it once. A `watch` (not a `oneshot`) is what makes the
    // late arrival observable at all.
    let _ = bound.send(true);
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
            // FR-72 P6 — the ladder's probe. `<PROBE_LABEL>.<domain>` is
            // answered by THIS resolver and by nothing else, so a runtime that
            // resolves it through the OS knows its steer actually reaches us.
            //
            // ⚠️ It must be a name the hosts fallback never writes, or the
            // probe would answer from its own fallback and the ladder could
            // never climb back to DNS — a check that cannot fail.
            if label == PROBE_LABEL {
                let self_v4 = match cfg.bind.ip() {
                    std::net::IpAddr::V4(v4) => v4,
                    std::net::IpAddr::V6(_) => Ipv4Addr::UNSPECIFIED,
                };
                return Some(match q.qtype {
                    1 => build_a(query, q.qend, self_v4),
                    _ => build_status(query, q.qend, 0), // NODATA for anything else
                });
            }
            let names = cfg.names.read().await;
            return Some(match names.get(label) {
                Some(ip) if q.qtype == 1 => build_a(query, q.qend, *ip),
                Some(ip) if cfg.answer_aaaa => build_aaaa(query, q.qend, derive_overlay_v6(*ip)),
                Some(_) => build_status(query, q.qend, 0), // AAAA off → NODATA
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
                } else if cfg.answer_aaaa {
                    build_aaaa(query, q.qend, derive_overlay_v6(*ip))
                } else {
                    build_status(query, q.qend, 0) // AAAA off → NODATA
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

/// Positive `AAAA` answer — the node's derived overlay IPv6 (same shape as
/// [`build_a`], TYPE 28 / RDLENGTH 16).
fn build_aaaa(query: &[u8], qend: usize, ip: std::net::Ipv6Addr) -> Vec<u8> {
    let mut out = resp_header_and_question(query, qend, 1, 0);
    out.extend_from_slice(&[0xC0, 0x0C]); // NAME → question at offset 12
    out.extend_from_slice(&[0, 28]); // TYPE AAAA
    out.extend_from_slice(&[0, 1]); // CLASS IN
    out.extend_from_slice(&[0, 0, 0, 60]); // TTL 60s
    out.extend_from_slice(&[0, 16]); // RDLENGTH
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

// ─────────────────────────────────────────────────────────────────────────────
// Per-OS split-DNS config
// ─────────────────────────────────────────────────────────────────────────────

/// The DEFAULT overlay NIC name (matches `tun.rs`) — the Linux fallback when
/// the caller can't name its link. W2: multi-org hosts run one resolver PER
/// ORG on that org's own TUN link (`roomler0`, `roomler0-<hex>`, …), and
/// `resolvectl dns/domain <link> …` REPLACES the link's settings — so every
/// org steering the hard-coded `roomler0` meant the second org overwrote the
/// first, and either guard's Drop reverted BOTH zones. Callers now pass
/// their own link name; the Windows path is unaffected (NRPT rules are
/// keyed per namespace, not per interface).
#[cfg(target_os = "linux")]
const DNS_IF_NAME: &str = "roomler0";

/// RAII guard for the per-OS DNS config — points `<magic_domain>` queries at our
/// resolver (Windows NRPT / Linux systemd-resolved routing domain). `Drop`
/// reverts it. Best-effort: a failure just means MagicDNS isn't wired into the
/// OS on that host (the resolver still runs).
pub struct DnsOsGuard {
    magic_domain: String,
    /// The Linux link this guard configured (owned so Drop reverts exactly
    /// what setup touched — never a sibling org's link). Unused on Windows.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    link: String,
    active: bool,
}

impl DnsOsGuard {
    /// S2 — did the OS split-DNS steer actually take? (Rendered in the
    /// desktop's DNS section + `roomler status`.)
    pub fn active(&self) -> bool {
        self.active
    }
}

/// Route `magic_domain` queries to our `resolver_ip` at the OS level. Returns a
/// guard that reverts on `Drop`. `link` is the caller's OWN overlay link name
/// (`TunIo::os_name()`), load-bearing on multi-org Linux hosts (see
/// [`DNS_IF_NAME`]); `None` falls back to the single-org default.
pub async fn configure_os(
    resolver_ip: Ipv4Addr,
    magic_domain: &str,
    link: Option<&str>,
) -> DnsOsGuard {
    #[cfg(target_os = "linux")]
    let link_owned = link.unwrap_or(DNS_IF_NAME).to_string();
    #[cfg(not(target_os = "linux"))]
    let link_owned = link.unwrap_or_default().to_string();
    let active = setup_os(resolver_ip, magic_domain, &link_owned).await;
    if active {
        info!(%resolver_ip, domain = %magic_domain, link = %link_owned, "magicdns: OS split-DNS configured");
    }
    DnsOsGuard {
        magic_domain: magic_domain.to_string(),
        link: link_owned,
        active,
    }
}

#[cfg(target_os = "windows")]
async fn setup_os(resolver_ip: Ipv4Addr, magic_domain: &str, _link: &str) -> bool {
    // NRPT split-DNS: only `*.<domain>` queries go to our resolver; everything
    // else stays on the system resolvers. Idempotent-ish — clear a stale rule
    // for this namespace first.
    let ns = format!(".{magic_domain}");
    let _ = run_cmd(vec![
        "powershell".into(),
        "-NoProfile".into(),
        "-Command".into(),
        format!(
            "Get-DnsClientNrptRule | Where-Object {{ $_.Namespace -eq '{ns}' }} | \
             Remove-DnsClientNrptRule -Force -ErrorAction SilentlyContinue"
        ),
    ])
    .await;
    run_cmd(vec![
        "powershell".into(),
        "-NoProfile".into(),
        "-Command".into(),
        format!("Add-DnsClientNrptRule -Namespace '{ns}' -NameServers '{resolver_ip}'"),
    ])
    .await
}

#[cfg(target_os = "linux")]
async fn setup_os(resolver_ip: Ipv4Addr, magic_domain: &str, link: &str) -> bool {
    // systemd-resolved: point THIS ORG'S overlay link at our resolver and
    // mark `<domain>` a routing-only domain (`~`) so only it resolves here.
    // Per-link on purpose: sibling orgs steer their own links, so N zones
    // coexist (resolvectl REPLACES a link's settings — the shared-link
    // variant made the second org clobber the first).
    let a = run_cmd(vec![
        "resolvectl".into(),
        "dns".into(),
        link.into(),
        resolver_ip.to_string(),
    ])
    .await;
    let b = run_cmd(vec![
        "resolvectl".into(),
        "domain".into(),
        link.into(),
        format!("~{magic_domain}"),
    ])
    .await;
    a && b
}

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
async fn setup_os(_resolver_ip: Ipv4Addr, _magic_domain: &str, _link: &str) -> bool {
    false
}

impl Drop for DnsOsGuard {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        #[cfg(target_os = "windows")]
        {
            let ns = format!(".{}", self.magic_domain);
            let _ = std::process::Command::new("powershell")
                .args([
                    "-NoProfile",
                    "-Command",
                    &format!(
                        "Get-DnsClientNrptRule | Where-Object {{ $_.Namespace -eq '{ns}' }} | \
                         Remove-DnsClientNrptRule -Force -ErrorAction SilentlyContinue"
                    ),
                ])
                .output();
        }
        #[cfg(target_os = "linux")]
        {
            // Revert exactly the link WE configured — a sibling org's guard
            // must never be able to strip this org's steering (the shared
            // hard-coded link made either teardown revert both zones).
            let _ = std::process::Command::new("resolvectl")
                .args(["revert", &self.link])
                .output();
        }
        info!(domain = %self.magic_domain, "magicdns: OS split-DNS reverted");
    }
}

/// Run an OS command off the reactor; `true` on exit 0, else logs stderr.
#[cfg(any(target_os = "windows", target_os = "linux"))]
async fn run_cmd(args: Vec<String>) -> bool {
    tokio::task::spawn_blocking(move || {
        let prog = args[0].clone();
        match std::process::Command::new(&prog).args(&args[1..]).output() {
            Ok(o) if o.status.success() => true,
            Ok(o) => {
                warn!(%prog, stderr = %String::from_utf8_lossy(&o.stderr).trim(),
                    "magicdns: OS-config command failed");
                false
            }
            Err(e) => {
                warn!(%prog, %e, "magicdns: OS-config command spawn failed");
                false
            }
        }
    })
    .await
    .unwrap_or(false)
}

// ─────────────────────────────────────────────────────────────────────────────
// P5 S4b — exit-node DNS steering (catch-all "." → the exit's egress)
// ─────────────────────────────────────────────────────────────────────────────
//
// While an exit node carries this host's default egress, steer the DEFAULT ("."
// catch-all) DNS namespace so every non-overlay query resolves through the exit
// (no DNS leak to the local ISP resolver). Two shapes, one goal:
//   • MagicDNS ON  → point "." at the LOCAL resolver (`self_v4`). It already
//     forwards non-overlay queries to `nameservers`; that forward is captured by
//     the split-default → egresses via the exit → resolves from the exit's
//     vantage. The P2 suffix rule stays intact (more specific → overlay names
//     still resolve locally). Linux just ADDS `~.`; Windows adds a `.`-root rule.
//   • MagicDNS OFF → no local resolver, so point "." DIRECTLY at `nameservers[0]`
//     (or 1.1.1.1). The query to that public resolver is itself captured by the
//     split-default and egresses via the exit. No overlay names to preserve.
// systemd-resolved routes a public name to the BEST-MATCHING routing domain
// (most labels); an explicit `~.` on roomler0 is a match, so it wins over the
// physical link's `DefaultRoute=yes` (which is only the no-match fallback) — no
// physical-link demotion needed.

/// Windows NRPT rule comment tag marking OUR catch-all rule, so the crash/boot
/// purge removes exactly it (never a foreign `.`-namespace rule, e.g. a corp
/// VPN's). Also the idempotent-clear selector on (re)steer.
#[cfg(target_os = "windows")]
const EXIT_DNS_NRPT_TAG: &str = "roomler-exit-dns";

/// Steer the default ("." catch-all) namespace at `target`. `magic_domain`:
/// `Some` = MagicDNS on (keep the P2 suffix split, just add `~.`); `None` =
/// MagicDNS off (point roomler0's own DNS at `target`). Best-effort — `false`
/// when the OS tool is unavailable (surfaced via `dns_steered=false`). Reverted
/// by [`unsteer_default_dns`] / [`purge_exit_dns`].
#[cfg(target_os = "windows")]
pub async fn steer_default_dns(target: Ipv4Addr, _magic_domain: Option<&str>) -> bool {
    // Idempotent: drop any stale roomler catch-all (tag-scoped) then add ours.
    // The `.`-root namespace is NRPT's catch-all; the P2 `.<domain>` rule is more
    // specific and still wins for overlay names.
    let _ = run_cmd(vec![
        "powershell".into(),
        "-NoProfile".into(),
        "-Command".into(),
        format!(
            "Get-DnsClientNrptRule | Where-Object {{ $_.Comment -eq '{EXIT_DNS_NRPT_TAG}' }} | \
             Remove-DnsClientNrptRule -Force -ErrorAction SilentlyContinue"
        ),
    ])
    .await;
    run_cmd(vec![
        "powershell".into(),
        "-NoProfile".into(),
        "-Command".into(),
        format!(
            "Add-DnsClientNrptRule -Namespace '.' -NameServers '{target}' -Comment '{EXIT_DNS_NRPT_TAG}'"
        ),
    ])
    .await
}

#[cfg(target_os = "linux")]
pub async fn steer_default_dns(target: Ipv4Addr, magic_domain: Option<&str>) -> bool {
    match magic_domain {
        // MagicDNS on: roomler0's DNS is already `self_v4` (== target) from the P2
        // suffix config. ADD `~.` so ALL names route to the local resolver (an
        // explicit `~.` is the best-match routing domain, beating the physical
        // link's DefaultRoute), while `~<domain>` keeps winning for overlay names.
        Some(domain) => {
            run_cmd(vec![
                "resolvectl".into(),
                "domain".into(),
                DNS_IF_NAME.into(),
                format!("~{domain}"),
                "~.".into(),
            ])
            .await
        }
        // MagicDNS off: no local resolver. Point roomler0's own DNS at the public
        // upstream and make it the default route; the query to `target` is
        // captured by the split-default and egresses via the exit.
        None => {
            let a = run_cmd(vec![
                "resolvectl".into(),
                "dns".into(),
                DNS_IF_NAME.into(),
                target.to_string(),
            ])
            .await;
            let b = run_cmd(vec![
                "resolvectl".into(),
                "domain".into(),
                DNS_IF_NAME.into(),
                "~.".into(),
            ])
            .await;
            a && b
        }
    }
}

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
pub async fn steer_default_dns(_target: Ipv4Addr, _magic_domain: Option<&str>) -> bool {
    false
}

/// Revert [`steer_default_dns`]. `magic_domain` `Some` restores the P2 suffix-only
/// routing domain (drops `~.`); `None` reverts the link entirely (no P2 config to
/// preserve). Windows removes the tagged catch-all rule. Best-effort.
#[cfg(target_os = "windows")]
pub async fn unsteer_default_dns(_magic_domain: Option<&str>) -> bool {
    run_cmd(vec![
        "powershell".into(),
        "-NoProfile".into(),
        "-Command".into(),
        format!(
            "Get-DnsClientNrptRule | Where-Object {{ $_.Comment -eq '{EXIT_DNS_NRPT_TAG}' }} | \
             Remove-DnsClientNrptRule -Force -ErrorAction SilentlyContinue"
        ),
    ])
    .await
}

#[cfg(target_os = "linux")]
pub async fn unsteer_default_dns(magic_domain: Option<&str>) -> bool {
    match magic_domain {
        Some(domain) => {
            run_cmd(vec![
                "resolvectl".into(),
                "domain".into(),
                DNS_IF_NAME.into(),
                format!("~{domain}"),
            ])
            .await
        }
        None => {
            run_cmd(vec![
                "resolvectl".into(),
                "revert".into(),
                DNS_IF_NAME.into(),
            ])
            .await
        }
    }
}

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
pub async fn unsteer_default_dns(_magic_domain: Option<&str>) -> bool {
    false
}

/// P5 S4b crash-safety (A2) — synchronously drop the exit-node catch-all steer.
/// Windows: the `.`-root NRPT rule is machine-global and PERSISTS across a crash
/// / reboot, so a stale rule pointing at a dead resolver would blackhole ALL DNS
/// until removed — this is the load-bearing path. Linux: the roomler0 link config
/// is link-scoped and dies with the interface, so `resolvectl revert roomler0` is
/// mostly belt (a harmless no-op when roomler0 is already gone; the P2 suffix
/// config re-applies when the resolver next comes up). Called from the crate-root
/// `purge_exit_routes` at every pre-`process::exit` site AND at boot. Context-free
/// (no `self_v4`/domain in hand) — hence the blanket revert on Linux. No-op off
/// Windows/Linux.
pub fn purge_exit_dns() {
    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new("powershell")
            .args([
                "-NoProfile",
                "-Command",
                &format!(
                    "Get-DnsClientNrptRule | Where-Object {{ $_.Comment -eq '{EXIT_DNS_NRPT_TAG}' }} | \
                     Remove-DnsClientNrptRule -Force -ErrorAction SilentlyContinue"
                ),
            ])
            .output();
    }
    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("resolvectl")
            .args(["revert", DNS_IF_NAME])
            .output();
    }
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
        let q = parse_question(&query_for("devbox.myorg.roomler.net", 1)).unwrap();
        assert_eq!(q.qname, "devbox.myorg.roomler.net");
        assert_eq!(q.qtype, 1);
    }

    #[test]
    fn a_answer_is_well_formed() {
        let query = query_for("devbox.myorg.roomler.net", 1);
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
        map.insert("devbox".to_string(), Ipv4Addr::new(100, 64, 0, 7));
        let cfg = DnsConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            magic_domain: "myorg.roomler.net".into(),
            upstream: "127.0.0.1:0".parse().unwrap(),
            names: Arc::new(RwLock::new(map)),
            answer_aaaa: true,
        };

        let hit = build_response(&query_for("devbox.myorg.roomler.net", 1), &cfg)
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
    async fn aaaa_answers_derived_v6_and_kill_switch_reverts_to_nodata() {
        let mut map = HashMap::new();
        map.insert("devbox".to_string(), Ipv4Addr::new(100, 64, 0, 7));
        let mut cfg = DnsConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            magic_domain: "myorg.roomler.net".into(),
            upstream: "127.0.0.1:0".parse().unwrap(),
            names: Arc::new(RwLock::new(map)),
            answer_aaaa: true,
        };

        // In-zone AAAA → one answer whose RDATA is the DERIVED overlay v6.
        let hit = build_response(&query_for("devbox.myorg.roomler.net", 28), &cfg)
            .await
            .unwrap();
        assert_eq!(u16::from_be_bytes([hit[6], hit[7]]), 1); // one answer
        let want = derive_overlay_v6(Ipv4Addr::new(100, 64, 0, 7)).octets();
        assert_eq!(&hit[hit.len() - 16..], &want);
        // The answer RR's TYPE field is AAAA (28).
        let rr = hit.len() - 16 - 10; // RDATA(16) + RDLEN(2)+TTL(4)+CLASS(2)+TYPE(2)
        assert_eq!(&hit[rr..rr + 2], &[0, 28]);

        // Bare label AAAA resolves the same way.
        let bare = build_response(&query_for("devbox", 28), &cfg)
            .await
            .unwrap();
        assert_eq!(&bare[bare.len() - 16..], &want);

        // Unknown in-zone name stays NXDOMAIN on AAAA too.
        let miss = build_response(&query_for("ghost.myorg.roomler.net", 28), &cfg)
            .await
            .unwrap();
        assert_eq!(miss[3] & 0x0F, 3);

        // Kill switch: AAAA → NODATA (name exists, zero answers, rcode 0).
        cfg.answer_aaaa = false;
        let off = build_response(&query_for("devbox.myorg.roomler.net", 28), &cfg)
            .await
            .unwrap();
        assert_eq!(off[3] & 0x0F, 0);
        assert_eq!(u16::from_be_bytes([off[6], off[7]]), 0);
        // A records are unaffected by the switch.
        let a = build_response(&query_for("devbox.myorg.roomler.net", 1), &cfg)
            .await
            .unwrap();
        assert_eq!(&a[a.len() - 4..], &[100, 64, 0, 7]);
    }

    #[tokio::test]
    async fn bare_known_label_resolves() {
        let mut map = HashMap::new();
        map.insert("devbox".to_string(), Ipv4Addr::new(100, 64, 0, 9));
        let cfg = DnsConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            magic_domain: "myorg.roomler.net".into(),
            upstream: "127.0.0.1:0".parse().unwrap(),
            names: Arc::new(RwLock::new(map)),
            answer_aaaa: true,
        };
        let resp = build_response(&query_for("devbox", 1), &cfg).await.unwrap();
        assert_eq!(&resp[resp.len() - 4..], &[100, 64, 0, 9]);
    }

    /// FR-72 — the resolver OWNS its port for as long as its task lives, and
    /// gives it back the moment the task is aborted.
    ///
    /// The runtime is scoped to one WS session and used to spawn this task
    /// without keeping the handle, so a reconnect's resolver lost the bind to
    /// its own dead predecessor and MagicDNS stayed off until the process
    /// restarted. Field-measured on a corp laptop as two starts 14 s apart:
    /// `resolver up` then `bind failed; resolver off … AddressAlreadyInUse`.
    /// It reads exactly like a third-party `:53` squatter and is not one.
    ///
    /// ⚠️ The second bind MUST fail while the first task lives — otherwise this
    /// test would pass on the broken code too, proving only that binding works.
    /// That assertion is the negative control and is the point of the test.
    #[tokio::test]
    async fn an_aborted_resolver_releases_its_port_for_the_next_one() {
        // A port nothing else holds: bind ephemeral, note it, drop it.
        let port = {
            let probe = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
            probe.local_addr().unwrap().port()
        };
        let cfg = |p: u16| DnsConfig {
            bind: format!("127.0.0.1:{p}").parse().unwrap(),
            magic_domain: "myorg.roomler.net".into(),
            upstream: "127.0.0.1:0".parse().unwrap(),
            names: Arc::new(RwLock::new(HashMap::new())),
            answer_aaaa: true,
        };

        // The first value the resolver publishes about itself.
        async fn first_report(rx: &mut tokio::sync::watch::Receiver<bool>) -> bool {
            rx.changed().await.expect("resolver dropped its reporter");
            *rx.borrow_and_update()
        }

        let (tx1, mut rx1) = tokio::sync::watch::channel(false);
        let first = tokio::spawn(run(cfg(port), tx1));
        assert!(
            first_report(&mut rx1).await,
            "the first resolver should bind"
        );

        // Negative control: while it lives, nobody else gets the port.
        let (tx2, mut rx2) = tokio::sync::watch::channel(false);
        let loser = tokio::spawn(run(cfg(port), tx2));
        assert!(
            !first_report(&mut rx2).await,
            "a second resolver must NOT be able to bind while the first holds the \
             port — if this passes, the test cannot detect the leak it exists for"
        );

        // The fix: the runtime aborts the task on teardown, and the port frees.
        first.abort();
        let _ = first.await;

        // FR-72 P2 — the loser is still RETRYING, so it becomes the owner on its
        // own. This is the whole point: before it, a resolver that lost the port
        // once stayed off for the daemon's lifetime even after the holder left,
        // and MagicDNS sat dead for hours with the port free the entire time.
        let recovered = tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                if *rx2.borrow_and_update() {
                    return true;
                }
                if rx2.changed().await.is_err() {
                    return false;
                }
            }
        })
        .await
        .unwrap_or(false);
        assert!(
            recovered,
            "a resolver that lost the bind must retry and take the port once it \
             frees — otherwise one momentary conflict is permanent"
        );
        loser.abort();
    }
}
