//! Disco — the out-of-tunnel carrier echo (C1: responder only).
//!
//! # Why this exists, and why it is NOT the rc.346 data-probe
//!
//! Measuring whether a *path* delivers data is the one thing the liveness
//! stack could never do honestly. Every passive signal is indirect: WG
//! handshakes are small and retried, so they complete over a path that drops
//! most bulk data (field 2026-08-11: `tx=122 rx=58`, `ping` 100 % loss, and
//! the carrier still looked healthy to every reaper).
//!
//! rc.346 tried to fix that with an **in-tunnel** probe — an inner ICMP /
//! marked IP packet that the PEER'S OS or overlay engine had to answer. That
//! made the measurement depend on the peer's operating system, its firewall,
//! and a capability negotiation, and it shipped default-ON in the same release
//! that introduced the responder, so nothing in the fleet could answer yet: it
//! demoted healthy 0 ms carriers within minutes.
//!
//! Disco is the Tailscale-shaped correction. It rides **outside** the WG
//! tunnel, as its own datagram shape on the carrier socket the two daemons
//! already share, and it is answered **unconditionally by the daemon** — no
//! OS, no firewall rule, no tunnel session, no capability bit. It measures the
//! PATH, not the peer's host stack.
//!
//! # Deployment rule this module encodes
//!
//! **A prober that punishes non-answer must ship at least one release AFTER
//! the responder is fleet-wide.** C1 therefore ships the responder ONLY —
//! this node answers pings, and asks nothing. Nothing reads the answers yet.
//! The prober (C2) and scoring (C3) come later, once every peer can reply.
//!
//! # Frame
//!
//! ```text
//!  0   magic[8]      "RMDISCO1"
//!  8   kind[1]       1 = ping, 2 = pong
//!  9   reserved[1]   0
//! 10   nonce[8]      echoed verbatim in the pong
//! 18   sender_pub[32] the sender's WG static public key
//! 50   observed[19]  the source the RESPONDER saw (family|16-byte addr|port);
//!                    all-zero in a ping. A free srflx observation for C2.
//! 69   mac[16]       HMAC-SHA256(X25519(self_secret, peer_public), bytes[0..69])[..16]
//! ```
//! Total 85 bytes, fixed.
//!
//! **Shape disjointness (load-bearing — a false match steals a live datagram
//! and blacks out the mesh):** WireGuard is `pkt[0] ∈ 1..=4 && pkt[1..4] ==
//! [0,0,0]` ([`super::wg::is_wg_shaped`]) and STUN is `len ≥ 20 && pkt[4..8]
//! == 0x2112A442` ([`crate::transport::stun::has_stun_cookie`]). `"RMDISCO1"`
//! starts with `0x52` (∉ 1..=4) and carries `"SCO1"` at bytes 4..8 (≠ the STUN
//! cookie), so the three shapes are pairwise disjoint by construction. Locked
//! by [`tests::disco_shape_is_disjoint_from_wg_and_stun`].

use std::net::{IpAddr, SocketAddr};

use boringtun::x25519::{PublicKey, StaticSecret};
use hmac::{Hmac, Mac};
use sha2::Sha256;

pub(crate) const MAGIC: &[u8; 8] = b"RMDISCO1";
pub(crate) const KIND_PING: u8 = 1;
pub(crate) const KIND_PONG: u8 = 2;

const OFF_KIND: usize = 8;
const OFF_NONCE: usize = 10;
const OFF_SENDER: usize = 18;
const OFF_OBSERVED: usize = 50;
const OFF_MAC: usize = 69;
/// Fixed frame length. A reply is exactly this long too, so the responder can
/// never amplify (reply bytes == request bytes).
pub(crate) const FRAME_LEN: usize = 85;

/// A parsed, MAC-verified disco frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Disco {
    pub kind: u8,
    pub nonce: [u8; 8],
    pub sender: [u8; 32],
    /// The source the responder observed, `None` in a ping (all-zero).
    pub observed: Option<SocketAddr>,
}

/// Cheap pre-filter: does this datagram even look like disco? Checked before
/// any crypto so a junk flood costs a memcmp, not a DH.
pub(crate) fn is_disco_shaped(pkt: &[u8]) -> bool {
    pkt.len() == FRAME_LEN && &pkt[..8] == MAGIC.as_slice()
}

/// The sender's claimed static public key, readable WITHOUT verifying the MAC.
/// The caller uses it to decide whether this is a peer it knows (a cheap map
/// lookup) before spending an X25519 — an unknown key is dropped for free, so
/// a flood of forged frames can't burn CPU.
pub(crate) fn claimed_sender(pkt: &[u8]) -> Option<[u8; 32]> {
    if !is_disco_shaped(pkt) {
        return None;
    }
    let mut k = [0u8; 32];
    k.copy_from_slice(&pkt[OFF_SENDER..OFF_SENDER + 32]);
    Some(k)
}

/// Verify + parse. `shared` is the X25519 static-static DH with the claimed
/// sender (the caller resolves and caches it). Returns `None` on any shape or
/// MAC failure — this is the authentication boundary.
pub(crate) fn parse(pkt: &[u8], shared: &[u8; 32]) -> Option<Disco> {
    if !is_disco_shaped(pkt) {
        return None;
    }
    let kind = pkt[OFF_KIND];
    if kind != KIND_PING && kind != KIND_PONG {
        return None;
    }
    if !mac_ok(pkt, shared) {
        return None;
    }
    let mut nonce = [0u8; 8];
    nonce.copy_from_slice(&pkt[OFF_NONCE..OFF_NONCE + 8]);
    let mut sender = [0u8; 32];
    sender.copy_from_slice(&pkt[OFF_SENDER..OFF_SENDER + 32]);
    Some(Disco {
        kind,
        nonce,
        sender,
        observed: decode_observed(&pkt[OFF_OBSERVED..OFF_OBSERVED + 19]),
    })
}

/// Build a frame. `observed` is `Some` only in a pong.
pub(crate) fn build(
    kind: u8,
    nonce: [u8; 8],
    sender_pub: &[u8; 32],
    observed: Option<SocketAddr>,
    shared: &[u8; 32],
) -> Vec<u8> {
    let mut p = vec![0u8; FRAME_LEN];
    p[..8].copy_from_slice(MAGIC.as_slice());
    p[OFF_KIND] = kind;
    p[OFF_NONCE..OFF_NONCE + 8].copy_from_slice(&nonce);
    p[OFF_SENDER..OFF_SENDER + 32].copy_from_slice(sender_pub);
    if let Some(sa) = observed {
        encode_observed(&mut p[OFF_OBSERVED..OFF_OBSERVED + 19], sa);
    }
    let tag = mac(&p[..OFF_MAC], shared);
    p[OFF_MAC..].copy_from_slice(&tag);
    p
}

/// The static-static X25519 shared secret with `peer_public`. Deterministic,
/// so the caller may cache it per peer.
pub(crate) fn shared_secret(secret: &StaticSecret, peer_public: &PublicKey) -> [u8; 32] {
    secret.diffie_hellman(peer_public).to_bytes()
}

/// Answer a disco PING: verify it and build the pong to send back to `src`.
/// `None` = not for us / not a ping / unknown sender / bad MAC ⇒ drop.
///
/// `known_peer` is consulted BEFORE any crypto so a flood of forged frames
/// costs a map lookup, never an X25519. A pong is exactly as long as the ping
/// ([`FRAME_LEN`]), so this can never be used as a reflection amplifier.
///
/// C1 answers peers this device has installed. That is also sufficient for
/// C2's "measure a path we are not currently routing over": the PEER is
/// installed, it is the ENDPOINT that differs.
pub(crate) fn respond(
    pkt: &[u8],
    src: SocketAddr,
    secret: &StaticSecret,
    public: &PublicKey,
    known_peer: impl Fn(&[u8; 32]) -> bool,
) -> Option<Vec<u8>> {
    match classify(pkt, src, secret, public, known_peer) {
        Verdict::Answer(pong) => Some(pong),
        _ => None,
    }
}

/// What a recv loop should do with a disco datagram.
pub(crate) enum Verdict {
    /// A verified PING — send these bytes back to the source.
    Answer(Vec<u8>),
    /// A verified PONG — hand it to the prober's sink (C2).
    Pong(DiscoInbound),
    /// Not ours / unknown sender / bad MAC ⇒ drop silently.
    Ignore,
}

/// Verify a disco datagram once and say what to do with it. Both recv seams
/// (device demux + carrier plane) call exactly this, so ping-answering and
/// pong-routing can never diverge between them.
pub(crate) fn classify(
    pkt: &[u8],
    src: SocketAddr,
    secret: &StaticSecret,
    public: &PublicKey,
    known_peer: impl Fn(&[u8; 32]) -> bool,
) -> Verdict {
    let Some(sender) = claimed_sender(pkt) else {
        return Verdict::Ignore;
    };
    // Known-peer check BEFORE any crypto: a forged flood costs a map lookup.
    if !known_peer(&sender) {
        return Verdict::Ignore;
    }
    let shared = shared_secret(secret, &PublicKey::from(sender));
    let Some(d) = parse(pkt, &shared) else {
        return Verdict::Ignore;
    };
    match d.kind {
        KIND_PING => Verdict::Answer(build(
            KIND_PONG,
            d.nonce,
            public.as_bytes(),
            Some(src),
            &shared,
        )),
        KIND_PONG => Verdict::Pong(DiscoInbound {
            sender: d.sender,
            nonce: d.nonce,
            src,
        }),
        _ => Verdict::Ignore,
    }
}

fn mac(body: &[u8], shared: &[u8; 32]) -> [u8; 16] {
    let mut m = <Hmac<Sha256> as Mac>::new_from_slice(shared).expect("hmac accepts any key length");
    m.update(body);
    let out = m.finalize().into_bytes();
    let mut tag = [0u8; 16];
    tag.copy_from_slice(&out[..16]);
    tag
}

/// Constant-time-ish compare via the MAC crate's own verifier.
fn mac_ok(pkt: &[u8], shared: &[u8; 32]) -> bool {
    let mut m = <Hmac<Sha256> as Mac>::new_from_slice(shared).expect("hmac accepts any key length");
    m.update(&pkt[..OFF_MAC]);
    let out = m.finalize().into_bytes();
    out[..16] == pkt[OFF_MAC..]
}

fn encode_observed(dst: &mut [u8], sa: SocketAddr) {
    match sa.ip() {
        IpAddr::V4(v4) => {
            dst[0] = 4;
            dst[1..5].copy_from_slice(&v4.octets());
        }
        IpAddr::V6(v6) => {
            dst[0] = 6;
            dst[1..17].copy_from_slice(&v6.octets());
        }
    }
    dst[17..19].copy_from_slice(&sa.port().to_be_bytes());
}

fn decode_observed(src: &[u8]) -> Option<SocketAddr> {
    let port = u16::from_be_bytes([src[17], src[18]]);
    match src[0] {
        4 => {
            let mut o = [0u8; 4];
            o.copy_from_slice(&src[1..5]);
            Some(SocketAddr::from((o, port)))
        }
        6 => {
            let mut o = [0u8; 16];
            o.copy_from_slice(&src[1..17]);
            Some(SocketAddr::from((o, port)))
        }
        _ => None,
    }
}

/// C2 — a verified PONG handed from a recv loop to the prober. Carries the
/// path it arrived on so the prober can attribute the sample to the exact
/// (local socket, remote endpoint) pair it probed.
#[derive(Debug, Clone)]
pub(crate) struct DiscoInbound {
    /// The peer that answered (its WG static public key).
    pub sender: [u8; 32],
    /// The nonce being answered — matched against the outstanding round.
    pub nonce: [u8; 8],
    /// Where the pong came FROM (the remote end of the measured path).
    pub src: SocketAddr,
}

// ───────────────────────────── C2: the path table ─────────────────────────
//
// The measurement C1 made possible. Keyed by (peer, local socket, remote
// endpoint) — the granularity the tier model structurally lacks, where every
// advertised endpoint of a peer collapses into ONE `TierState` and is
// disambiguated only by a strike-count rotation.
//
// Two rules are load-bearing, both paid for in the field:
//
// 1. **Silence is never negative evidence.** A missing pong lowers nothing.
//    A path loses only by DECAYING while rivals accumulate fresh pongs. This
//    is what makes a mixed fleet safe with no capability bit: a peer that
//    cannot answer simply never wins that path, instead of being punished.
// 2. **Loss is a WINDOWED RATE, never a consecutive-miss count.** rc.346's
//    reaper counted consecutive misses and could therefore only catch a
//    FULLY dead path: on the live grox carrier (~20 % delivery, `tx=17..22
//    rx=3..4`) roughly one probe in five got through and reset the counter,
//    so it demoted nothing. A rate sees 80 % loss for what it is.

/// How many recent rounds the loss rate is computed over. At the steady 30 s
/// cadence that is ~8 minutes of history; at the 5 s discovery cadence, ~80 s.
pub(crate) const LOSS_WINDOW: u32 = 16;

/// One measured path to one peer.
///
/// `allow(dead_code)`: the prober that drives this lands in the NEXT commit
/// (C2b). Same convention as the repo's defensive-enum rule — the allow is
/// temporary and must be removed when the consumer lands. Shipping the table
/// first keeps the measurement primitives reviewable on their own, and they
/// are already locked by `loss_is_a_rate_so_a_lossy_path_cannot_hide`.
#[derive(Debug, Clone, Default)]
pub(crate) struct PathStats {
    /// Rounds issued into the window (saturating at [`LOSS_WINDOW`]).
    sent: u32,
    /// Rounds answered within the window.
    answered: u32,
    /// Smoothed round-trip, `None` until the first pong.
    pub rtt_ms: Option<f64>,
    /// The nonce of the round currently outstanding, if any.
    pending: Option<[u8; 8]>,
}

impl PathStats {
    /// Record that a round was issued with `nonce`. A still-outstanding
    /// previous round counts as unanswered — that is the ONLY way `sent`
    /// outpaces `answered`, and it is a rate input, never a death.
    pub(crate) fn on_sent(&mut self, nonce: [u8; 8]) {
        self.sent = (self.sent + 1).min(LOSS_WINDOW);
        if self.sent >= LOSS_WINDOW {
            // Slide: keep the ratio, halve both so recent rounds dominate.
            self.sent = self.sent.div_ceil(2);
            self.answered = self.answered.div_ceil(2);
        }
        self.pending = Some(nonce);
    }

    /// Record a pong. Only a MATCHING nonce counts — a late pong from an
    /// earlier round is ignored rather than credited to the current one.
    pub(crate) fn on_pong(&mut self, nonce: [u8; 8], rtt_ms: f64) {
        if self.pending != Some(nonce) {
            return;
        }
        self.pending = None;
        self.answered = (self.answered + 1).min(self.sent);
        // EWMA, α = 0.3 — fast enough to track a real path change, slow
        // enough that one scheduling hiccup doesn't dominate.
        self.rtt_ms = Some(match self.rtt_ms {
            Some(prev) => prev * 0.7 + rtt_ms * 0.3,
            None => rtt_ms,
        });
    }

    /// Fraction of windowed rounds that went unanswered, 0.0..=1.0. `None`
    /// until the window has enough rounds to mean anything — an unmeasured
    /// path must never look like a bad one.
    pub(crate) fn loss(&self) -> Option<f64> {
        if self.sent < 4 {
            return None;
        }
        Some(1.0 - (self.answered as f64 / self.sent as f64))
    }
}

/// One measured path, as reported out of the [`Prober`].
#[derive(Debug, Clone, Copy)]
pub(crate) struct PathSample {
    pub peer: [u8; 32],
    pub dst: SocketAddr,
    /// Windowed loss 0.0..=1.0; `None` until enough rounds to judge — an
    /// unmeasured path must never read as a bad one.
    pub loss: Option<f64>,
    pub rtt_ms: Option<f64>,
}

/// C2 — the prober: one round per peer per tick over that peer's CURRENT
/// direct-carrier path, plus the windowed table of what came back.
///
/// Deliberately measurement-only. Nothing here demotes, penalises, or ranks —
/// `paths()` is read by the LocalAPI and the summary log, and by nothing that
/// makes a routing decision. Scoring (C3) and authority (C6) are separate
/// stages behind their own flags, precisely so a bug here cannot move traffic.
#[derive(Default)]
pub(crate) struct Prober {
    /// (peer pubkey, remote endpoint) → windowed stats.
    table: std::collections::HashMap<([u8; 32], SocketAddr), PathStats>,
    /// Outstanding rounds: nonce → (peer, path, sent-at).
    pending: std::collections::HashMap<[u8; 8], ([u8; 32], SocketAddr, std::time::Instant)>,
    /// Monotonic nonce source — unique per round so a late pong from an
    /// earlier round can never be credited to the current one.
    seq: u64,
}

impl Prober {
    /// Build the next ping for `peer` and remember the round. The caller
    /// sends the bytes over that peer's carrier and reports the dst back via
    /// [`Self::sent`].
    pub(crate) fn next_ping(
        &mut self,
        peer: &[u8; 32],
        secret: &StaticSecret,
        public: &PublicKey,
    ) -> ([u8; 8], Vec<u8>) {
        self.seq = self.seq.wrapping_add(1);
        let nonce = self.seq.to_be_bytes();
        let shared = shared_secret(secret, &PublicKey::from(*peer));
        (
            nonce,
            build(KIND_PING, nonce, public.as_bytes(), None, &shared),
        )
    }

    /// Record that the round actually went out to `dst`.
    pub(crate) fn sent(&mut self, peer: [u8; 32], dst: SocketAddr, nonce: [u8; 8]) {
        self.table.entry((peer, dst)).or_default().on_sent(nonce);
        self.pending
            .insert(nonce, (peer, dst, std::time::Instant::now()));
        // Bound the pending map: a path that never answers would otherwise
        // accumulate one entry per round forever.
        if self.pending.len() > 512 {
            let cutoff = std::time::Instant::now() - std::time::Duration::from_secs(120);
            self.pending.retain(|_, (_, _, at)| *at > cutoff);
        }
    }

    /// Record a verified pong. Unmatched nonces (a late reply, or a path we
    /// never probed) are ignored rather than credited.
    pub(crate) fn on_pong(&mut self, p: &DiscoInbound) {
        let Some((peer, dst, at)) = self.pending.remove(&p.nonce) else {
            return;
        };
        // The nonce alone is not enough: a pong must come from the PEER and
        // the PATH we probed, or the sample would be attributed to the wrong
        // path (and a peer could credit a path it does not actually serve).
        if p.sender != peer || p.src != dst {
            return;
        }
        let rtt = at.elapsed().as_secs_f64() * 1000.0;
        if let Some(s) = self.table.get_mut(&(peer, dst)) {
            s.on_pong(p.nonce, rtt);
        }
    }

    /// Every measured path.
    pub(crate) fn paths(&self) -> Vec<PathSample> {
        self.table
            .iter()
            .map(|((peer, dst), s)| PathSample {
                peer: *peer,
                dst: *dst,
                loss: s.loss(),
                rtt_ms: s.rtt_ms,
            })
            .collect()
    }

    /// A compact, operator-readable digest of what the paths measure right
    /// now: how many are measured, and the WORST few by loss. This is C2's
    /// entire observable — the stage deliberately produces a log line and a
    /// LocalAPI field, and no behaviour.
    ///
    /// `None` until at least one path has enough rounds to judge, so a
    /// freshly-started prober says nothing rather than something misleading.
    pub(crate) fn summary(&self) -> Option<String> {
        let mut worst: Vec<(f64, SocketAddr, Option<f64>)> = self
            .paths()
            .into_iter()
            .filter_map(|p| p.loss.map(|l| (l, p.dst, p.rtt_ms)))
            .collect();
        if worst.is_empty() {
            return None;
        }
        let measured = worst.len();
        worst.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        let lossy = worst.iter().filter(|(l, _, _)| *l > 0.1).count();
        let top: Vec<String> = worst
            .iter()
            .take(3)
            .map(|(l, dst, rtt)| match rtt {
                Some(r) => format!("{dst} loss={:.0}% rtt={r:.0}ms", l * 100.0),
                None => format!("{dst} loss={:.0}% rtt=—", l * 100.0),
            })
            .collect();
        Some(format!(
            "paths={} lossy={lossy} worst=[{}]",
            measured,
            top.join(", ")
        ))
    }

    /// Drop state for a peer that left the mesh.
    pub(crate) fn forget(&mut self, peer: &[u8; 32]) {
        self.table.retain(|(p, _), _| p != peer);
        self.pending.retain(|_, (p, _, _)| p != peer);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::stun::has_stun_cookie;

    fn kp() -> (StaticSecret, PublicKey) {
        let kp = super::super::WgKeypair::generate();
        (kp.secret, kp.public)
    }

    /// THE load-bearing invariant: a disco frame must never be mistaken for
    /// WireGuard or STUN (a false match steals a live datagram and blacks out
    /// the mesh), and neither may be mistaken for disco.
    #[test]
    fn disco_shape_is_disjoint_from_wg_and_stun() {
        let (a_s, a_p) = kp();
        let (_b_s, b_p) = kp();
        let sh = shared_secret(&a_s, &b_p);
        let f = build(KIND_PING, [7u8; 8], a_p.as_bytes(), None, &sh);

        assert_eq!(f.len(), FRAME_LEN);
        assert!(is_disco_shaped(&f));
        assert!(
            !super::super::wg::is_wg_shaped(&f),
            "disco must never look like WireGuard"
        );
        assert!(
            !has_stun_cookie(&f),
            "disco must never look like STUN (bytes 4..8 != magic cookie)"
        );

        // …and every WG message type must not look like disco. WG is
        // type byte 1..=4 followed by three zero bytes.
        for t in 1u8..=4 {
            let mut wg = vec![0u8; FRAME_LEN];
            wg[0] = t;
            assert!(super::super::wg::is_wg_shaped(&wg));
            assert!(!is_disco_shaped(&wg), "WG type {t} must not parse as disco");
        }
        // A STUN Binding response shape must not look like disco either.
        let mut stun = vec![0u8; FRAME_LEN];
        stun[0] = 0x01;
        stun[4..8].copy_from_slice(&0x2112_A442u32.to_be_bytes());
        assert!(has_stun_cookie(&stun));
        assert!(!is_disco_shaped(&stun));
    }

    #[test]
    fn ping_pong_roundtrip_and_mac_rejects_forgery() {
        let (a_s, a_p) = kp();
        let (b_s, b_p) = kp();
        // A→B share == B→A share (static-static DH is symmetric).
        let ab = shared_secret(&a_s, &b_p);
        let ba = shared_secret(&b_s, &a_p);
        assert_eq!(ab, ba);

        let ping = build(KIND_PING, [9u8; 8], a_p.as_bytes(), None, &ab);
        let got = parse(&ping, &ba).expect("B verifies A's ping");
        assert_eq!(got.kind, KIND_PING);
        assert_eq!(got.nonce, [9u8; 8]);
        assert_eq!(&got.sender, a_p.as_bytes());
        assert_eq!(got.observed, None, "a ping carries no observation");
        assert_eq!(claimed_sender(&ping).unwrap(), *a_p.as_bytes());

        // The pong echoes the nonce and reports the observed source.
        let obs: SocketAddr = "203.0.113.9:41000".parse().unwrap();
        let pong = build(KIND_PONG, got.nonce, b_p.as_bytes(), Some(obs), &ba);
        assert_eq!(pong.len(), ping.len(), "a pong must never amplify");
        let back = parse(&pong, &ab).expect("A verifies B's pong");
        assert_eq!(back.kind, KIND_PONG);
        assert_eq!(back.nonce, [9u8; 8]);
        assert_eq!(back.observed, Some(obs));

        // Forgery: a third party's DH does not verify.
        let (c_s, _c_p) = kp();
        let ca = shared_secret(&c_s, &a_p);
        assert!(parse(&ping, &ca).is_none(), "wrong key must not verify");

        // Any bit flip in the body invalidates the MAC.
        let mut tampered = ping.clone();
        tampered[OFF_NONCE] ^= 0x01;
        assert!(parse(&tampered, &ba).is_none(), "tamper must not verify");

        // Wrong length / wrong magic / unknown kind are rejected pre-crypto.
        assert!(!is_disco_shaped(&ping[..FRAME_LEN - 1]));
        let mut bad_kind = ping.clone();
        bad_kind[OFF_KIND] = 9;
        assert!(parse(&bad_kind, &ba).is_none());
    }

    /// THE regression lock. rc.346 counted CONSECUTIVE misses, so a path that
    /// delivered ~20 % (the real grox carrier: `tx=17..22 rx=3..4`) reset the
    /// counter roughly every fifth round and was never detected. A windowed
    /// RATE must see that same path as ~80 % lossy — while a healthy path
    /// reads ~0 and an unmeasured one reads `None`, never "bad".
    #[test]
    fn loss_is_a_rate_so_a_lossy_path_cannot_hide() {
        // ~20 % delivery: 1 pong per 5 rounds — the shape that defeated the
        // consecutive-miss reaper.
        let mut lossy = PathStats::default();
        for i in 0..10u32 {
            let n = [i as u8; 8];
            lossy.on_sent(n);
            if i % 5 == 0 {
                lossy.on_pong(n, 40.0);
            }
        }
        let l = lossy.loss().expect("enough rounds to judge");
        assert!(
            l > 0.6,
            "a ~20% delivery path must read as heavily lossy, got {l}"
        );

        // A healthy path reads ~0 loss.
        let mut good = PathStats::default();
        for i in 0..10u32 {
            let n = [i as u8; 8];
            good.on_sent(n);
            good.on_pong(n, 10.0);
        }
        assert_eq!(good.loss(), Some(0.0));
        assert!(good.rtt_ms.unwrap() > 0.0);

        // An UNMEASURED path is `None` — never mistaken for a bad one. This
        // is the "silence is not negative evidence" rule in code.
        let mut fresh = PathStats::default();
        fresh.on_sent([1u8; 8]);
        assert_eq!(fresh.loss(), None, "too few rounds must not read as loss");

        // A stale pong (wrong nonce) credits nothing.
        let mut stale = PathStats::default();
        stale.on_sent([1u8; 8]);
        stale.on_pong([9u8; 8], 5.0);
        assert_eq!(stale.rtt_ms, None);
    }

    /// The prober's round accounting: a path that answers reads healthy, a
    /// path that goes silent accrues LOSS (never a death), and an unmatched
    /// or late pong credits nothing.
    #[test]
    fn prober_rounds_measure_loss_without_punishing_silence() {
        let (a_s, a_p) = kp();
        let (_b_s, b_p) = kp();
        let peer = *b_p.as_bytes();
        let dst: SocketAddr = "203.0.113.9:41000".parse().unwrap();
        let mut pr = Prober::default();

        // Six answered rounds ⇒ zero loss, an RTT is known.
        for _ in 0..6 {
            let (nonce, frame) = pr.next_ping(&peer, &a_s, &a_p);
            assert!(is_disco_shaped(&frame));
            pr.sent(peer, dst, nonce);
            pr.on_pong(&DiscoInbound {
                sender: peer,
                nonce,
                src: dst,
            });
        }
        let p = pr.paths();
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].loss, Some(0.0), "answered rounds ⇒ no loss");
        assert!(p[0].rtt_ms.is_some(), "an RTT was measured");

        // Six silent rounds ⇒ the loss RATE climbs. Nothing else happens:
        // there is no death, no penalty, no ranking — measurement only.
        for _ in 0..6 {
            let (nonce, _f) = pr.next_ping(&peer, &a_s, &a_p);
            pr.sent(peer, dst, nonce);
        }
        let loss = pr.paths()[0].loss.expect("measured");
        assert!(loss > 0.0, "silence must show as loss, got {loss}");

        // An unknown nonce credits nothing.
        let before = pr.paths()[0].loss;
        pr.on_pong(&DiscoInbound {
            sender: peer,
            nonce: [0xEE; 8],
            src: dst,
        });
        assert_eq!(
            pr.paths()[0].loss,
            before,
            "an unmatched pong must not credit"
        );

        // Forgetting a departed peer clears its paths.
        pr.forget(&peer);
        assert!(pr.paths().is_empty());
    }

    #[test]
    fn observed_encodes_v4_and_v6() {
        let (a_s, a_p) = kp();
        let (_b_s, b_p) = kp();
        let sh = shared_secret(&a_s, &b_p);
        for s in ["198.51.100.5:65535", "[2001:db8::1]:443"] {
            let sa: SocketAddr = s.parse().unwrap();
            let f = build(KIND_PONG, [0u8; 8], a_p.as_bytes(), Some(sa), &sh);
            assert_eq!(parse(&f, &sh).unwrap().observed, Some(sa));
        }
    }
}
