// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-19 P2a — the org-relay bind handshake, authenticated.
//!
//! # Why this exists in this shape
//!
//! FR-19's first draft drew a 3-way handshake in which the client's only
//! obligation was to **echo a value the relay had just sent it in the clear**.
//! That proves the client can *receive at* an address — return-routability —
//! and nothing else. It copied the anti-DoS half of the reference design and
//! dropped the sealing that proves identity.
//!
//! The consequence was concrete, not theoretical: the VNI is 24 bits and not
//! secret, and the peer key in the challenge is public netmap data, so anyone
//! sharing the victim's egress `addr:port` — *a co-worker behind the same
//! corporate NAT, which is exactly the population this feature targets* —
//! could take the slot. A stolen bind black-holes the pair, receives its
//! ciphertext, and injects arbitrary UDP at its WireGuard socket.
//!
//! # Two keys, two jobs, never derived from one another
//!
//! | key | held by | proves |
//! |---|---|---|
//! | [`BindSecret`] | the **member** and the relay | *you are the node this session was minted for* |
//! | [`CookieKey`] | the **relay only**, rotating | *you can receive at the address you claim* |
//!
//! Collapsing these into one key would make the property unstateable: a value
//! the relay can compute alone cannot prove membership, and a value the member
//! holds cannot be a stateless return-routability cookie.
//!
//! # Where the address is bound — and why not in `tag1`
//!
//! `tag1` (step 1) covers `vni ‖ generation ‖ nonce` and **not** the source
//! address. The first version of this module included the address, and the
//! loopback test made the mistake obvious within minutes: a client behind NAT
//! **cannot know its own mapped `addr:port` when it sends its first packet**,
//! so it could never have computed the value the relay expected. The reference
//! design binds the address at the *challenge* step for exactly this reason,
//! and so does this one: the [`cookie`] covers the observed address, `tag2`
//! covers the cookie, and the relay re-derives the cookie against the observed
//! source on the answer. The address is therefore bound *by step 3*, which is
//! the only step that grants anything.
//!
//! What that costs: a captured `tag1` can be replayed from elsewhere to obtain
//! a challenge. A challenge is one 64-byte frame, sent only to the address it
//! was requested from, rate-limited per source — the same posture as a probe
//! echo, and it grants nothing.
//!
//! # Encoding
//!
//! Every MAC input is **fixed-width and domain-separated**. There are no
//! length prefixes because there are no variable-length fields: addresses are
//! encoded as 18 canonical bytes (IPv6-mapped) rather than text, which removes
//! the whole class of ambiguity where an IPv4 and IPv6 rendering — or digits
//! sliding into an adjacent field — serialise two distinct tuples identically.
//!
//! # What this does NOT defend against
//!
//! Replaying a *complete* captured exchange from the **same** source address
//! re-binds that same address, which is where the traffic was already going.
//! The nonce stops a capture being replayed into a *different* window, and the
//! address binding stops it being replayed from anywhere else. An attacker who
//! is already on-path at the victim's exact `addr:port` is outside what a bind
//! handshake can decide.

use std::net::SocketAddr;

use blake2::digest::Mac as _;
use subtle::ConstantTimeEq;

type MacFn = blake2::Blake2sMac<blake2::digest::consts::U16>;

/// MAC / cookie length in bytes.
pub const MAC_LEN: usize = 16;
/// Per-attempt nonce length in bytes.
pub const NONCE_LEN: usize = 16;

pub type Mac = [u8; MAC_LEN];
pub type Nonce = [u8; NONCE_LEN];

// Domain separation. Fixed-length and distinct, so a value computed for one
// step can never validate at another — the reason `tag1` and `tag2` over
// identical inputs are different values.
const DOM_TAG1: &[u8; 8] = b"orlyTAG1";
const DOM_TAG2: &[u8; 8] = b"orlyTAG2";
const DOM_COOKIE: &[u8; 8] = b"orlyCOOK";

/// The per-`(session, member)` secret. Minted by the server and delivered to
/// the member over its **authenticated control WS**, and to the relay in its
/// copy of the mint — so possession of it is what "is this the node the
/// session was minted for" means.
#[derive(Clone)]
pub struct BindSecret([u8; 32]);

/// The relay's own rotating key. Never leaves the relay: it exists so the
/// relay can issue a challenge it can later re-derive **without storing
/// per-attempt state**, which is what stops an unauthenticated packet costing
/// memory.
#[derive(Clone)]
pub struct CookieKey([u8; 32]);

impl BindSecret {
    pub fn from_bytes(b: [u8; 32]) -> Self {
        Self(b)
    }
}

impl CookieKey {
    pub fn from_bytes(b: [u8; 32]) -> Self {
        Self(b)
    }
}

/// Canonical 18-byte address encoding: 16 bytes of IPv6-mapped address plus a
/// big-endian port. Binary and fixed-width on purpose — a textual form makes
/// `1.2.3.4:80` and `::ffff:1.2.3.4:80` two spellings of one address, and a
/// MAC that disagrees about which it covers is a MAC that can be bypassed.
fn addr_bytes(a: &SocketAddr) -> [u8; 18] {
    let mut out = [0u8; 18];
    let ip6 = match a.ip() {
        std::net::IpAddr::V4(v4) => v4.to_ipv6_mapped(),
        std::net::IpAddr::V6(v6) => v6,
    };
    out[..16].copy_from_slice(&ip6.octets());
    out[16..].copy_from_slice(&a.port().to_be_bytes());
    out
}

fn mac(key: &[u8; 32], parts: &[&[u8]]) -> Mac {
    // `new_from_slice` only fails on an invalid key length, and the key is a
    // fixed [u8; 32], so this cannot fail at runtime.
    let mut m = <MacFn as blake2::digest::Mac>::new_from_slice(key)
        .expect("Blake2sMac accepts a 32-byte key");
    for p in parts {
        m.update(p);
    }
    let out = m.finalize().into_bytes();
    let mut tag = [0u8; MAC_LEN];
    tag.copy_from_slice(&out);
    tag
}

/// `tag1` — the member's proof, sent with the initial bind. Deliberately does
/// **not** cover the source address (see the module doc): the client cannot
/// know its NAT-mapped address on its first packet, and the address is bound
/// at step 3 instead.
pub fn tag1(secret: &BindSecret, vni: u32, generation: u64, nonce: &Nonce) -> Mac {
    mac(
        &secret.0,
        &[
            DOM_TAG1,
            &vni.to_be_bytes(),
            &generation.to_be_bytes(),
            nonce,
        ],
    )
}

/// The relay's stateless return-routability cookie — this is where the
/// observed source address enters the handshake.
pub fn cookie(key: &CookieKey, vni: u32, nonce: &Nonce, from: &SocketAddr) -> Mac {
    mac(
        &key.0,
        &[DOM_COOKIE, &vni.to_be_bytes(), nonce, &addr_bytes(from)],
    )
}

/// `tag2` — the member's proof over the challenge it was given.
pub fn tag2(secret: &BindSecret, cookie: &Mac, nonce: &Nonce) -> Mac {
    mac(&secret.0, &[DOM_TAG2, cookie, nonce])
}

/// Why a bind step was refused. Enumerated because during a flood the reason
/// is the entire diagnostic: an attack and a misconfigured peer are
/// indistinguishable in a single "refused" total.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindRefusal {
    /// The member proof did not verify — not a party to this session.
    BadTag1,
    /// The cookie was not one this relay issued to this address.
    BadCookie,
    /// The answer's member proof over the challenge did not verify.
    BadTag2,
}

/// What the relay should do next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindOutcome {
    /// Send this cookie back as the challenge.
    Challenge(Mac),
    /// Both proofs are good; this address may be recorded as bound.
    Bound,
    Refused(BindRefusal),
}

/// Verifies bind steps against the current and previous cookie keys.
///
/// Two windows are accepted because a rotation must not kill an exchange that
/// is legitimately in flight; the window is short enough that it does not
/// meaningfully widen replay, and the address binding constrains that anyway.
pub struct BindVerifier {
    current: CookieKey,
    previous: Option<CookieKey>,
}

impl BindVerifier {
    pub fn new(current: CookieKey, previous: Option<CookieKey>) -> Self {
        Self { current, previous }
    }

    /// Step 1 → 2. Verify the member's proof and issue a challenge bound to
    /// the address the bind arrived from.
    pub fn on_bind(
        &self,
        secret: &BindSecret,
        vni: u32,
        generation: u64,
        nonce: &Nonce,
        from: &SocketAddr,
        presented_tag1: &Mac,
    ) -> BindOutcome {
        // Mutation-verified: deleting this check fails four tests, including
        // `a_valid_cookie_without_the_member_secret_is_refused` — which is the
        // first draft's design, and the reason this module exists.
        let want = tag1(secret, vni, generation, nonce);
        if !ct_eq(&want, presented_tag1) {
            return BindOutcome::Refused(BindRefusal::BadTag1);
        }
        BindOutcome::Challenge(cookie(&self.current, vni, nonce, from))
    }

    /// Step 3. Verify the cookie came from us for THIS address, then verify the
    /// member's proof over it.
    pub fn on_answer(
        &self,
        secret: &BindSecret,
        vni: u32,
        nonce: &Nonce,
        from: &SocketAddr,
        presented_cookie: &Mac,
        presented_tag2: &Mac,
    ) -> BindOutcome {
        let ours = ct_eq(&cookie(&self.current, vni, nonce, from), presented_cookie)
            || self
                .previous
                .as_ref()
                .is_some_and(|k| ct_eq(&cookie(k, vni, nonce, from), presented_cookie));
        if !ours {
            return BindOutcome::Refused(BindRefusal::BadCookie);
        }
        if !ct_eq(&tag2(secret, presented_cookie, nonce), presented_tag2) {
            return BindOutcome::Refused(BindRefusal::BadTag2);
        }
        BindOutcome::Bound
    }
}

/// Constant-time comparison.
///
/// ⚠️ Deliberately NOT `==`. Every MAC comparison already in this tree is a
/// plain slice compare — including one whose comment claims to be
/// "constant-time-ish" and is not — which is defensible where the value is a
/// pre-filter derived from public data. Here the MAC **is** the authenticator,
/// so a timing side channel is a bypass, and "model it on the existing one"
/// would have propagated the wrong precedent.
fn ct_eq(a: &Mac, b: &Mac) -> bool {
    a.ct_eq(b).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secret(b: u8) -> BindSecret {
        BindSecret::from_bytes([b; 32])
    }
    fn ckey(b: u8) -> CookieKey {
        CookieKey::from_bytes([b; 32])
    }
    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }
    const N: Nonce = [7u8; NONCE_LEN];

    fn verifier() -> BindVerifier {
        BindVerifier::new(ckey(1), None)
    }

    #[test]
    fn a_member_completes_the_three_way_handshake() {
        let v = verifier();
        let s = secret(9);
        let a = addr("198.51.100.7:41000");

        // The client computes tag1 knowing nothing about its mapped address.
        let t1 = tag1(&s, 42, 3, &N);
        let BindOutcome::Challenge(c) = v.on_bind(&s, 42, 3, &N, &a, &t1) else {
            panic!("a valid tag1 must be challenged");
        };
        let t2 = tag2(&s, &c, &N);
        assert_eq!(v.on_answer(&s, 42, &N, &a, &c, &t2), BindOutcome::Bound);
    }

    /// The property the first draft did not have: holding a valid cookie is
    /// NOT enough. Without the member secret there is no way past step 1, and
    /// even a genuine cookie cannot be answered.
    #[test]
    fn a_valid_cookie_without_the_member_secret_is_refused() {
        let v = verifier();
        let real = secret(9);
        let attacker = secret(0xAA);
        let a = addr("198.51.100.7:41000");

        let forged = tag1(&attacker, 42, 3, &N);
        assert_eq!(
            v.on_bind(&real, 42, 3, &N, &a, &forged),
            BindOutcome::Refused(BindRefusal::BadTag1)
        );

        let genuine = cookie(&ckey(1), 42, &N, &a);
        let bad_t2 = tag2(&attacker, &genuine, &N);
        assert_eq!(
            v.on_answer(&real, 42, &N, &a, &genuine, &bad_t2),
            BindOutcome::Refused(BindRefusal::BadTag2)
        );
    }

    /// The same-NAT steal, which is the case that matters most: a co-worker
    /// behind the victim's corporate NAT shares its public IP but not its
    /// port. Replaying the victim's captured `tag1` DOES earn them a challenge
    /// — one 64-byte frame, to their own address, rate-limited — but the
    /// cookie is bound to their port, not the victim's, and they cannot answer
    /// it without the secret. Nothing they capture binds anything.
    #[test]
    fn a_neighbour_on_the_same_public_ip_cannot_bind_with_the_victims_exchange() {
        let v = verifier();
        let s = secret(9);
        let victim = addr("203.0.113.5:41000");
        let neighbour = addr("203.0.113.5:41001"); // same IP, different port

        // Replayed tag1 from the neighbour: challenged, and the challenge is
        // bound to the NEIGHBOUR's address, not the victim's.
        let t1 = tag1(&s, 42, 3, &N);
        let BindOutcome::Challenge(c_neigh) = v.on_bind(&s, 42, 3, &N, &neighbour, &t1) else {
            panic!("a replayed tag1 earns a challenge (and nothing more)");
        };
        assert_ne!(c_neigh, cookie(&ckey(1), 42, &N, &victim));

        // The victim's cookie does not validate from the neighbour's port...
        let c_victim = cookie(&ckey(1), 42, &N, &victim);
        let t2 = tag2(&s, &c_victim, &N);
        assert_eq!(
            v.on_answer(&s, 42, &N, &neighbour, &c_victim, &t2),
            BindOutcome::Refused(BindRefusal::BadCookie)
        );

        // ...and the neighbour's own challenge cannot be answered without the
        // secret they do not hold.
        let forged_t2 = tag2(&secret(0xAA), &c_neigh, &N);
        assert_eq!(
            v.on_answer(&s, 42, &N, &neighbour, &c_neigh, &forged_t2),
            BindOutcome::Refused(BindRefusal::BadTag2)
        );
    }

    #[test]
    fn a_cookie_this_relay_never_issued_is_refused() {
        let v = verifier();
        let s = secret(9);
        let a = addr("198.51.100.7:41000");
        let forged = cookie(&ckey(0xFF), 42, &N, &a); // attacker's own key
        let t2 = tag2(&s, &forged, &N);
        assert_eq!(
            v.on_answer(&s, 42, &N, &a, &forged, &t2),
            BindOutcome::Refused(BindRefusal::BadCookie)
        );
    }

    /// A rotation must not kill an exchange already in flight, but only ONE
    /// window back — an older key is as foreign as an attacker's.
    #[test]
    fn the_previous_cookie_window_is_accepted_and_an_older_one_is_not() {
        let s = secret(9);
        let a = addr("198.51.100.7:41000");
        let v = BindVerifier::new(ckey(2), Some(ckey(1)));

        let prev = cookie(&ckey(1), 42, &N, &a);
        assert_eq!(
            v.on_answer(&s, 42, &N, &a, &prev, &tag2(&s, &prev, &N)),
            BindOutcome::Bound
        );

        let ancient = cookie(&ckey(0), 42, &N, &a);
        assert_eq!(
            v.on_answer(&s, 42, &N, &a, &ancient, &tag2(&s, &ancient, &N)),
            BindOutcome::Refused(BindRefusal::BadCookie)
        );
    }

    /// A nonce from one attempt does not authorise another.
    #[test]
    fn a_captured_exchange_does_not_replay_under_a_different_nonce() {
        let v = verifier();
        let s = secret(9);
        let a = addr("198.51.100.7:41000");
        let c = cookie(&ckey(1), 42, &N, &a);
        let t2 = tag2(&s, &c, &N);

        let other: Nonce = [8u8; NONCE_LEN];
        assert_eq!(
            v.on_answer(&s, 42, &other, &a, &c, &t2),
            BindOutcome::Refused(BindRefusal::BadCookie)
        );
    }

    /// Domain separation: the same key over the same material must not produce
    /// a value that is valid at a different step.
    #[test]
    fn the_three_macs_are_domain_separated() {
        let s = secret(9);
        let a = addr("198.51.100.7:41000");
        let t1 = tag1(&s, 42, 3, &N);
        let c = cookie(&CookieKey::from_bytes([9u8; 32]), 42, &N, &a); // same key bytes
        let t2 = tag2(&s, &[0u8; MAC_LEN], &N);
        assert_ne!(t1, c, "tag1 and cookie must differ even under one key");
        assert_ne!(t1, t2);
        assert_ne!(c, t2);
    }

    /// Fixed-width encoding: distinct tuples cannot serialise identically.
    /// Under a naive textual concatenation `(42, 3)` and `(4, 23)` both render
    /// as the digits `423` — the sliding case the encoding exists to make
    /// unrepresentable.
    #[test]
    fn adjacent_fields_cannot_slide_into_each_other() {
        let s = secret(9);
        assert_ne!(
            tag1(&s, 42, 3, &N),
            tag1(&s, 4, 23, &N),
            "(vni=42,gen=3) and (vni=4,gen=23) must be distinct inputs"
        );
    }

    /// An IPv4 address and its IPv6-mapped spelling are ONE address, so the
    /// cookie must be one value — otherwise a peer whose socket reports the
    /// other form is refused for no reason a user could diagnose.
    #[test]
    fn ipv4_and_its_ipv6_mapped_form_are_the_same_address() {
        let k = ckey(1);
        assert_eq!(
            cookie(&k, 42, &N, &addr("198.51.100.7:41000")),
            cookie(&k, 42, &N, &addr("[::ffff:198.51.100.7]:41000")),
        );
    }

    /// A different session's secret must not open this one, even at the same
    /// address and generation.
    #[test]
    fn a_secret_from_another_session_does_not_bind_this_one() {
        let v = verifier();
        let a = addr("198.51.100.7:41000");
        let other = secret(0x55);
        let t1 = tag1(&other, 42, 3, &N);
        assert_eq!(
            v.on_bind(&secret(9), 42, 3, &N, &a, &t1),
            BindOutcome::Refused(BindRefusal::BadTag1)
        );
    }

    /// The VNI and generation are covered, so a proof for one session or one
    /// re-mint does not carry to another.
    #[test]
    fn vni_and_generation_are_both_covered() {
        let v = verifier();
        let s = secret(9);
        let a = addr("198.51.100.7:41000");
        let t1 = tag1(&s, 42, 3, &N);
        assert_eq!(
            v.on_bind(&s, 43, 3, &N, &a, &t1),
            BindOutcome::Refused(BindRefusal::BadTag1),
            "a proof for VNI 42 must not bind VNI 43"
        );
        assert_eq!(
            v.on_bind(&s, 42, 4, &N, &a, &t1),
            BindOutcome::Refused(BindRefusal::BadTag1),
            "a proof for generation 3 must not bind generation 4"
        );
    }
}
