// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-52 — the **connect code**: how someone outside the organization names a
//! device (`docs/fr/FR-52-cross-org-remote-access.md` §5).
//!
//! It exists because an outsider cannot browse the org's device list and must
//! not be able to. They are given a code out of band — read down a phone,
//! pasted into a chat — and that code is the only handle they have.
//!
//! ## Why not the `agent_id`
//!
//! `agent_id` is an `ObjectId`: an internal key, 24 hex characters, and
//! *timestamp-prefixed*, so consecutive enrollments produce visibly adjacent
//! ids. It is the wrong shape to dictate and the wrong thing to publish.
//!
//! ## The alphabet
//!
//! [Crockford base32](https://www.crockford.com/base32.html) — `0-9` plus the
//! letters minus `I`, `L`, `O` and `U`. The first three are dropped because
//! they are misread as `1`/`1`/`0`; `U` is dropped so a random draw cannot
//! spell something the operator has to apologise for. Twelve characters is
//! **60 bits**, which is not enumerable, and grouped as `XXXX-XXXX-XXXX` it is
//! still dictatable.
//!
//! ⚠️ Entropy is not the security boundary here — the device-held password is
//! (gate 4). What these 60 bits buy is that the fleet cannot be *enumerated*:
//! without them, resolving codes would be a device-discovery oracle, and the
//! rate limiter would be the only thing standing in front of it.
//!
//! ## Reading it back
//!
//! [`normalize`] is deliberately forgiving in exactly the ways a human is
//! wrong and strict everywhere else: case is ignored, dashes and spaces are
//! dropped, and `I`/`L` fold to `1` while `O` folds to `0` — the substitutions
//! Crockford defines, and the ones someone reading a code aloud actually
//! causes. It does **not** guess at anything else: a `U` is a refusal, not a
//! `V`, because inventing a second interpretation of a character would make
//! two different codes resolve to one device.

use ring::rand::{SecureRandom, SystemRandom};

/// Crockford base32, in value order. 32 symbols, so a uniform byte masked to
/// its low 5 bits selects one with no modulo bias (256 = 8 × 32).
const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// Characters in a code. 12 × 5 bits = 60 bits.
pub const CODE_LEN: usize = 12;

/// Size of a dictation group in the display form.
const GROUP: usize = 4;

/// Mint a fresh code, in canonical (ungrouped, uppercase) form.
///
/// Returns `None` if the system RNG refuses — which must be treated as "no
/// code", never as a reason to fall back to a weaker source. A device without
/// a code is simply unaddressable from outside, which is the safe direction.
pub fn generate() -> Option<String> {
    let mut bytes = [0u8; CODE_LEN];
    SystemRandom::new().fill(&mut bytes).ok()?;
    Some(
        bytes
            .iter()
            .map(|b| ALPHABET[(b & 0x1F) as usize] as char)
            .collect(),
    )
}

/// Parse what a human typed into the canonical form, or `None` if it is not a
/// connect code.
///
/// Accepts any casing, any grouping (or none), and the three Crockford
/// confusables. Rejects everything else — including `U`, and including a
/// string of the right shape but the wrong length, because a truncated code
/// that resolved to *something* would be worse than one that resolved to
/// nothing.
pub fn normalize(input: &str) -> Option<String> {
    let mut out = String::with_capacity(CODE_LEN);
    for ch in input.chars() {
        // Separators a human or a form will introduce. Silently dropped —
        // they carry no information and every rendering of a code has them.
        if ch == '-' || ch == ' ' || ch == '\t' || ch == '_' {
            continue;
        }
        let up = ch.to_ascii_uppercase();
        let mapped = match up {
            // Crockford's defined confusables, and the only substitutions
            // made anywhere in this function.
            'I' | 'L' => '1',
            'O' => '0',
            c => c,
        };
        if !ALPHABET.contains(&(mapped as u8)) {
            return None;
        }
        out.push(mapped);
        // Bail early rather than reading an unbounded string into memory: the
        // input is attacker-supplied on the resolve path.
        if out.len() > CODE_LEN {
            return None;
        }
    }
    (out.len() == CODE_LEN).then_some(out)
}

/// The display form: `XXXX-XXXX-XXXX`.
///
/// Takes an already-canonical code. Anything else is returned unchanged rather
/// than mangled — this is a formatter, and a caller handing it garbage has a
/// bug that a silently reshaped string would hide.
pub fn format_grouped(code: &str) -> String {
    if code.len() != CODE_LEN {
        return code.to_string();
    }
    code.as_bytes()
        .chunks(GROUP)
        .map(|c| std::str::from_utf8(c).unwrap_or_default())
        .collect::<Vec<_>>()
        .join("-")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_codes_are_canonical_and_the_right_length() {
        for _ in 0..64 {
            let c = generate().expect("system RNG");
            assert_eq!(c.len(), CODE_LEN);
            assert!(
                c.bytes().all(|b| ALPHABET.contains(&b)),
                "generated a symbol outside the alphabet: {c}"
            );
            // A freshly generated code must survive its own reader.
            assert_eq!(normalize(&c).as_deref(), Some(c.as_str()));
        }
    }

    /// Not a randomness test — a wiring test. Two draws colliding would mean
    /// the RNG is not being consumed at all (a constant seed, a reused buffer),
    /// which is exactly the bug that would ship a fleet-wide identical code.
    #[test]
    fn two_draws_differ() {
        assert_ne!(generate().unwrap(), generate().unwrap());
    }

    /// The alphabet is a compatibility surface: a code already read down a
    /// phone and written on a sticky note has to keep resolving.
    #[test]
    fn alphabet_is_locked() {
        assert_eq!(&ALPHABET[..], b"0123456789ABCDEFGHJKMNPQRSTVWXYZ");
        for excluded in [b'I', b'L', b'O', b'U'] {
            assert!(
                !ALPHABET.contains(&excluded),
                "{} must stay out of the alphabet",
                excluded as char
            );
        }
    }

    #[test]
    fn normalize_accepts_the_forms_a_human_produces() {
        let canonical = "K7Q29XM4TB3F";
        for input in [
            "K7Q29XM4TB3F",
            "k7q29xm4tb3f",
            "K7Q2-9XM4-TB3F",
            "k7q2 9xm4 tb3f",
            " K7Q2-9XM4-TB3F ",
            "K7Q2_9XM4_TB3F",
        ] {
            assert_eq!(
                normalize(input).as_deref(),
                Some(canonical),
                "failed to read {input:?}"
            );
        }
    }

    /// The three Crockford substitutions, and only those. Someone reading a
    /// code aloud says "oh" for zero and "ell" for one; the alternative is a
    /// support call that ends in "it just says invalid".
    #[test]
    fn normalize_folds_the_confusables() {
        assert_eq!(normalize("OOOO-IIII-LLLL").as_deref(), Some("000011111111"));
    }

    /// `U` is excluded from the alphabet, so it is a REFUSAL rather than a
    /// fourth substitution. Guessing it meant `V` would make two distinct
    /// codes resolve to one device.
    #[test]
    fn normalize_rejects_u_rather_than_guessing() {
        assert_eq!(normalize("UUUU-UUUU-UUUU"), None);
    }

    #[test]
    fn normalize_rejects_wrong_length_and_bad_symbols() {
        assert_eq!(normalize(""), None);
        assert_eq!(normalize("K7Q2-9XM4-TB3"), None, "one short");
        assert_eq!(normalize("K7Q2-9XM4-TB3FF"), None, "one long");
        assert_eq!(normalize("K7Q2-9XM4-TB3!"), None, "punctuation");
        // A very long input must be refused, not buffered.
        assert_eq!(normalize(&"A".repeat(10_000)), None);
    }

    #[test]
    fn format_groups_in_fours_and_leaves_anything_else_alone() {
        assert_eq!(format_grouped("K7Q29XM4TB3F"), "K7Q2-9XM4-TB3F");
        assert_eq!(format_grouped("short"), "short");
        assert_eq!(format_grouped(""), "");
    }

    /// Round-trip: the form we PRINT has to be a form we can READ. Trivial
    /// until someone changes the group size on one side only.
    #[test]
    fn display_form_round_trips() {
        let c = generate().unwrap();
        assert_eq!(normalize(&format_grouped(&c)).as_deref(), Some(c.as_str()));
    }
}
