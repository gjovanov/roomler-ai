// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
use serde::{Deserialize, Serialize};

bitflags::bitflags! {
    /// Per-session capability bitfield. The agent enforces these — the server
    /// only signals what was negotiated. This is the source of truth on what
    /// the controller can actually do.
    ///
    /// ⚠️ `Serialize` is bitflags' own impl and MUST stay that way — see the
    /// hand-written `Deserialize` below for why the write side is deliberately
    /// untouched.
    #[derive(Serialize, Debug, Clone, Copy, PartialEq, Eq)]
    #[serde(transparent)]
    pub struct Permissions: u16 {
        const VIEW       = 0b0000_0000_0000_0001;
        const INPUT      = 0b0000_0000_0000_0010;
        const CLIPBOARD  = 0b0000_0000_0000_0100;
        const FILES      = 0b0000_0000_0000_1000;
        const AUDIO      = 0b0000_0000_0001_0000;
        const RECORD     = 0b0000_0000_0010_0000;
    }
}

/// Hand-written so a `Permissions` stored in Mongo can be read back.
///
/// **The bug this fixes.** bitflags 2.x's serde impl branches on
/// `Serializer::is_human_readable()`: pipe-separated names when true, raw bits
/// when false. bson 2.x's **Serializer defaults `human_readable = true` while
/// its Deserializer defaults to `false`** — so the same type wrote
/// `"VIEW | INPUT | CLIPBOARD | FILES"` into Mongo and then demanded a `u16`
/// reading it back. Every read of a stored `RemoteSession` 500'd with
/// `invalid type: string "VIEW | INPUT …", expected u16`, while routes that
/// only touch live hub state kept working. Issue #1166.
///
/// **Why only the read side changed.** The write is *already correct*: every
/// reader in the field expects the name form, and every row ever stored holds
/// it (the Serializer has always defaulted human-readable). "Symmetrising" this
/// to `u16` on both ends would break currently-working paths in order to repair
/// a broken one, and would strand 100 % of existing rows. Do not do it.
///
/// **Why the numeric form is gated on `!is_human_readable()`.** The `rc:*` JSON
/// wire is deliberately name-only — `deserialise_numeric_is_rejected` locks
/// that, and the agent plus the TS store depend on it. Accepting bits
/// unconditionally would loosen that wire contract as a side effect of a
/// storage fix. Gating on the same flag bitflags itself branches on keeps JSON
/// strictly name-only while letting the non-human-readable (bson) path accept
/// either shape — which it must, permanently: an older binary in the field
/// keeps writing names, so the old shape never stops arriving.
impl<'de> Deserialize<'de> for Permissions {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V {
            /// Mirrors the deserializer's own flag; see the type docs.
            human_readable: bool,
        }
        impl serde::de::Visitor<'_> for V {
            type Value = Permissions;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                if self.human_readable {
                    f.write_str("a pipe-separated permission name list, e.g. \"VIEW | INPUT\"")
                } else {
                    f.write_str("a pipe-separated permission name list or a u16 bitfield")
                }
            }
            fn visit_str<E: serde::de::Error>(self, s: &str) -> Result<Permissions, E> {
                parse_wire_names(s).ok_or_else(|| E::custom(format!("unknown permission in {s:?}")))
            }
            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Permissions, E> {
                if self.human_readable {
                    // Keep the rc:* wire name-only.
                    return Err(E::custom("numeric permissions are not accepted here"));
                }
                // Truncating is the fail-SAFE direction: an unknown bit from a
                // newer writer drops the permission rather than granting it.
                Ok(Permissions::from_bits_truncate(v as u16))
            }
            fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Permissions, E> {
                // bson stores integers as i32/i64, so this arm is the one a
                // bits-shaped stored row actually lands on.
                self.visit_u64(v.max(0) as u64)
            }
        }
        let human_readable = d.is_human_readable();
        d.deserialize_any(V { human_readable })
    }
}

/// Parse the pipe-separated name form that [`Permissions::wire_names`] writes.
/// `None` if any name is unrecognised — matching bitflags' own strictness, so a
/// typo fails loudly instead of silently granting less than intended.
fn parse_wire_names(s: &str) -> Option<Permissions> {
    let mut out = Permissions::empty();
    let s = s.trim();
    if s.is_empty() {
        // An empty grant is representable; `wire_names` emits "" for it.
        return Some(out);
    }
    for part in s.split('|') {
        let name = part.trim();
        if name.is_empty() {
            continue;
        }
        out |= Permissions::from_name(name)?;
    }
    Some(out)
}

impl Default for Permissions {
    fn default() -> Self {
        Self::VIEW | Self::INPUT | Self::CLIPBOARD
    }
}

impl Permissions {
    pub fn view_only(self) -> Self {
        Self::VIEW
    }

    pub fn requires_consent_prompt(self) -> bool {
        self.intersects(Self::INPUT | Self::FILES | Self::AUDIO | Self::RECORD)
    }

    /// The pipe-separated name form (`"VIEW | INPUT"`) — byte-identical to what
    /// the serde impl emits, without going through `serde_json` to get it.
    ///
    /// FR-27 needed this for the consent-prompt marker, whose body is now built
    /// from a typed struct rather than an ad-hoc `json!` literal. The old
    /// literal got the string for free by serializing the bitflags inline; a
    /// `String` field does not, and `to_value(..).as_str()` to read back what we
    /// just wrote is a worse way to spell it.
    pub fn wire_names(self) -> String {
        self.iter_names()
            .map(|(name, _)| name)
            .collect::<Vec<_>>()
            .join(" | ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // These lock in the wire format used by `rc:*` messages.
    //
    // bitflags 2.x with its `serde` feature serializes flag sets as a
    // pipe-separated string like `"VIEW | INPUT"` — the struct-level
    // `#[serde(transparent)]` attribute is *ignored* by bitflags' own
    // Serialize/Deserialize impl, so changing it has no effect.
    //
    // If this test starts failing because bitflags changed its default,
    // update the TS-side agent store (and any manual JSON in tests)
    // accordingly. Numeric-form payloads will NOT deserialise.

    #[test]
    fn serialises_as_pipe_separated_string() {
        let p = Permissions::VIEW | Permissions::INPUT;
        assert_eq!(serde_json::to_string(&p).unwrap(), "\"VIEW | INPUT\"");
    }

    /// FR-27 — `wire_names` must stay byte-identical to the serde form, since
    /// the consent marker and the `rc:*` wire are read by the same UI code.
    #[test]
    fn wire_names_matches_the_serde_form() {
        for p in [
            Permissions::VIEW | Permissions::INPUT,
            Permissions::VIEW,
            Permissions::default(),
            Permissions::all(),
        ] {
            let via_serde = serde_json::to_string(&p).unwrap();
            assert_eq!(
                format!("\"{}\"", p.wire_names()),
                via_serde,
                "wire_names drifted from the serde form for {p:?}"
            );
        }
    }

    /// An empty set is an empty string, not `"(empty)"` or a panic — a
    /// view-only-nothing grant is representable and must render.
    #[test]
    fn wire_names_of_an_empty_set_is_empty() {
        assert_eq!(Permissions::empty().wire_names(), "");
    }

    #[test]
    fn deserialises_string_names() {
        let p: Permissions = serde_json::from_str("\"VIEW | INPUT\"").unwrap();
        assert_eq!(p, Permissions::VIEW | Permissions::INPUT);
    }

    #[test]
    fn deserialise_numeric_is_rejected() {
        let r: Result<Permissions, _> = serde_json::from_str("3");
        assert!(r.is_err(), "numeric form must not be accepted");
    }

    // ── #1166: the bson path ────────────────────────────────────────────────
    //
    // These are deliberately bson, not JSON. The JSON tests above pass even
    // when storage is completely broken, because serde_json is human-readable
    // in BOTH directions — so bitflags takes the same branch on the way in and
    // out. bson does not: its Serializer defaults human_readable = true and its
    // Deserializer defaults to false. A JSON-only suite cannot catch that class
    // by construction, which is exactly why this shipped.

    #[derive(serde::Serialize, serde::Deserialize, Debug, PartialEq, Eq)]
    struct StoredPerms {
        permissions: Permissions,
    }

    /// The regression, exercised through **raw BSON bytes** — the only path
    /// that reproduces it.
    ///
    /// ⚠️ Measured, because it is not obvious and it decides whether this test
    /// is worth anything: `bson::from_bson` and `bson::from_document` both
    /// report `is_human_readable() == true`, while **`bson::from_slice` (raw)
    /// reports `false`**. The mongodb driver reads raw bytes, so it takes the
    /// `false` branch — which is exactly where bitflags demanded a `u16` and
    /// every stored-session read 500'd. A round trip written with
    /// `to_bson`/`from_bson` passes even on the BROKEN code, so it would have
    /// been a test that proves nothing. Go through `to_vec`/`from_slice`.
    #[test]
    fn a_stored_permissions_reads_back_through_raw_bson() {
        for p in [
            Permissions::VIEW | Permissions::INPUT,
            Permissions::VIEW,
            Permissions::default(),
            Permissions::all(),
            Permissions::empty(),
        ] {
            let doc = bson::to_document(&StoredPerms { permissions: p }).expect("serialize");
            let bytes = bson::to_vec(&doc).expect("to_vec");
            let back: StoredPerms = bson::from_slice(&bytes).expect("raw deserialize");
            assert_eq!(back.permissions, p, "raw-bson round trip for {p:?}");
        }
    }

    /// Pin the STORED shape. Every row in prod holds the name form, so if the
    /// write side ever flips to bits this fails loudly — that change would
    /// strand every existing row and break readers still in the field.
    #[test]
    fn bson_stores_the_name_form() {
        let p = Permissions::VIEW | Permissions::INPUT;
        assert_eq!(
            bson::to_bson(&p).unwrap(),
            bson::Bson::String("VIEW | INPUT".into()),
            "the stored representation must stay the name form"
        );
    }

    /// The compatibility half: a **bits**-shaped stored row must also read, so
    /// a document written by anything that serialised non-human-readably still
    /// loads. Raw path again, for the reason documented above.
    #[test]
    fn a_bits_shaped_stored_row_also_reads_back() {
        let want = Permissions::VIEW | Permissions::INPUT;
        let bytes =
            bson::to_vec(&bson::doc! { "permissions": bson::Bson::Int32(want.bits() as i32) })
                .unwrap();
        let got: StoredPerms = bson::from_slice(&bytes).expect("bits-shaped row must read");
        assert_eq!(got.permissions, want);
    }

    /// The gate: tolerating bits in storage must NOT loosen the `rc:*` JSON
    /// wire. Companion to `deserialise_numeric_is_rejected` — together they
    /// assert the tolerance is scoped to the non-human-readable path only.
    #[test]
    fn the_json_wire_stays_name_only_even_though_storage_accepts_bits() {
        assert!(
            serde_json::from_str::<Permissions>("3").is_err(),
            "the rc:* JSON wire must stay name-only"
        );
        let bytes = bson::to_vec(&bson::doc! { "permissions": bson::Bson::Int32(3) }).unwrap();
        assert!(
            bson::from_slice::<StoredPerms>(&bytes).is_ok(),
            "raw bson must tolerate the bits form"
        );
    }

    /// An unrecognised name is an error, not a silent partial grant.
    #[test]
    fn an_unknown_permission_name_is_refused() {
        assert!(serde_json::from_str::<Permissions>("\"VIEW | NOPE\"").is_err());
        let bytes = bson::to_vec(&bson::doc! { "permissions": "VIEW | NOPE" }).unwrap();
        assert!(bson::from_slice::<StoredPerms>(&bytes).is_err());
    }
}
