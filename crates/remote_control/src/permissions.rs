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
/// **What is actually on disk — measured per entry point, not assumed
/// (#1166, #1630).** bitflags 2.x's serde impl branches on
/// `is_human_readable()`: pipe-separated names when true, raw bits when false.
/// bson 2.15 answers that question differently for each of its entry points,
/// and the mongodb driver's typed API sits on the *raw* ones:
///
/// | entry point (bson 2.15.0 / mongodb 3.7.0) | `is_human_readable()` | a `Permissions` is |
/// |---|---|---|
/// | `to_bson` / `to_document` — `$set` values, the `remote_sessions` projection in `audit.rs` | `true` | `"VIEW \| INPUT"` |
/// | `to_vec` / `to_raw_document_buf` — the driver's typed `insert_one` / `insert_many` | **`false`** | `Int32(3)` |
/// | `from_bson` / `from_document` | `true` | — |
/// | `from_slice` — the driver's typed cursor, a plain struct field | **`false`** | — |
/// | `from_slice` — a field under `#[serde(tag)]` / `untagged` / `flatten` | **`true`** (see below) | — |
///
/// So `remote_sessions.permissions` (written with `bson::to_document`) holds
/// NAMES, while `remote_audit.event.permissions` (written by the driver's
/// `insert_many` in the same file) has held BITS since the field appeared in
/// multi-user P3. Both shapes are in production and both stay: an older binary
/// keeps writing whatever it writes, so every reader must accept either,
/// permanently.
///
/// **Why the numeric form is gated on `!is_human_readable()` here.** The
/// `rc:*` JSON wire is deliberately name-only — `deserialise_numeric_is_rejected`
/// locks that, and the agent plus the TS store depend on it. Accepting bits
/// unconditionally would loosen that wire contract as a side effect of a
/// storage fix, so this impl stays the WIRE rule: names always, bits only when
/// the format itself says it is binary.
///
/// **Why that gate cannot carry storage on its own (#1630).** serde reads an
/// internally-tagged enum such as `AuditKind` by buffering the whole value into
/// its private `Content` and re-deserialising each variant field from a
/// `ContentDeserializer`, which does not forward the outer format's flag and
/// answers the trait default, `true`. The `Int32` the driver had written under
/// `RemoteAuditEvent.event` therefore reached this visitor as "human-readable"
/// and was refused, and `GET …/session/{id}/audit` 500'd on every session
/// (0.4.101). The same buffering happens under `#[serde(untagged)]`,
/// `#[serde(tag, content)]` and `#[serde(flatten)]`. Where a field sits in the
/// document is not something this impl can see, so every PERSISTED field opts
/// in explicitly with [`deserialize_stored`] — do that for any new stored
/// `Permissions`, top-level or nested, and leave this impl as the wire rule.
///
/// **Why only the read side changed, both times.** Every reader in the field
/// expects the name form on the wire, and both shapes already exist on disk.
/// "Symmetrising" the writer to `u16` would strand every name-shaped row and
/// break the wire for older agents; switching the driver path to names would
/// not remove the bits already stored. Do neither.
impl<'de> Deserialize<'de> for Permissions {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        // Mirrors the deserializer's own flag; see the type docs.
        let accept_bits = !d.is_human_readable();
        d.deserialize_any(NamesOrBits { accept_bits })
    }
}

/// Read a `Permissions` that came out of STORAGE: names or bits, whatever the
/// deserializer claims about itself.
///
/// Put `#[serde(deserialize_with = "crate::permissions::deserialize_stored")]`
/// on every persisted `Permissions` field. The blanket impl keys its tolerance
/// on `is_human_readable()`, and that flag does not survive serde's `Content`
/// buffering (tagged / untagged enums, `flatten`): `AuditKind`'s two fields
/// read as human-readable under the raw driver cursor and refused the `Int32`
/// the raw driver insert had written (#1630). A field-level opt-in is the one
/// signal independent of where in the document the field sits. Unknown bits
/// are truncated — the fail-safe direction; an unknown NAME is still an error,
/// exactly as on the wire.
///
/// ⚠️ Only for types that never arrive over the `rc:*` JSON wire: a struct
/// carrying this attribute accepts `"permissions": 3` from JSON too.
/// `RemoteSession` and `RemoteAuditEvent` are read from Mongo and only ever
/// serialised towards the browser, so nothing loosens there.
pub fn deserialize_stored<'de, D>(d: D) -> Result<Permissions, D::Error>
where
    D: serde::Deserializer<'de>,
{
    d.deserialize_any(NamesOrBits { accept_bits: true })
}

/// The one visitor behind both entry points; `accept_bits` is the whole
/// difference between the wire rule and the storage rule.
struct NamesOrBits {
    accept_bits: bool,
}

impl serde::de::Visitor<'_> for NamesOrBits {
    type Value = Permissions;
    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        if self.accept_bits {
            f.write_str("a pipe-separated permission name list or a u16 bitfield")
        } else {
            f.write_str("a pipe-separated permission name list, e.g. \"VIEW | INPUT\"")
        }
    }
    fn visit_str<E: serde::de::Error>(self, s: &str) -> Result<Permissions, E> {
        parse_wire_names(s).ok_or_else(|| E::custom(format!("unknown permission in {s:?}")))
    }
    fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Permissions, E> {
        if !self.accept_bits {
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

    // ── #1166 / #1630: the bson paths ───────────────────────────────────────
    //
    // These are deliberately bson, not JSON. The JSON tests above pass even
    // when storage is completely broken, because serde_json is human-readable
    // in BOTH directions — so bitflags takes the same branch on the way in and
    // out. bson does not, and it is not even one answer: the VALUE API
    // (`to_bson`/`to_document`/`from_bson`/`from_document`) says true, the RAW
    // API the mongodb driver uses (`to_vec`/`to_raw_document_buf`/`from_slice`)
    // says false — except under serde's `Content` buffering, where the flag is
    // lost and reads as true again. A JSON-only suite cannot catch any of that
    // by construction, which is exactly why both bugs shipped.

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

    /// Pin the shape the VALUE API stores (`to_bson` / `to_document` — every
    /// `remote_sessions` row): the name form. If this ever flips to bits it
    /// fails loudly, because readers in the field still expect names on the
    /// wire. ⚠️ It is one of TWO writers: the driver's typed insert goes
    /// through the raw serializer and stores bits — see
    /// `the_raw_serializer_the_driver_uses_stores_bits`. #1166's version of
    /// this comment said "every row in prod holds the name form"; measured on
    /// 2026-09-25, every `remote_audit` row held an `Int32`.
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

    // ── #1630: the gate is blind under serde's Content buffering ───────────
    //
    // `AuditKind` is `#[serde(tag = "kind")]`. Under `from_slice` the OUTER
    // deserializer says `false`, but serde re-reads the variant fields from a
    // buffered `Content` whose deserializer says `true` — so the raw driver
    // cursor refused the `Int32` the raw driver insert had written. Nothing
    // here touches a database; the pair below is byte-for-byte the driver's.

    /// The stored shape, read by the blanket impl (the wire rule).
    #[derive(serde::Deserialize, Debug)]
    #[serde(tag = "kind", rename_all = "snake_case")]
    enum TaggedWire {
        Requested { permissions: Permissions },
    }

    /// The same shape with the storage opt-in.
    #[derive(serde::Deserialize, Debug)]
    #[serde(tag = "kind", rename_all = "snake_case")]
    enum TaggedStored {
        Requested {
            #[serde(deserialize_with = "deserialize_stored")]
            permissions: Permissions,
        },
    }

    #[derive(serde::Deserialize, Debug)]
    struct Row<E> {
        event: E,
    }

    fn tagged_row(permissions: bson::Bson) -> Vec<u8> {
        bson::to_vec(&bson::doc! { "event": { "kind": "requested", "permissions": permissions } })
            .unwrap()
    }

    /// The driver's typed insert writes BITS — pin it. #1166's comment said
    /// every stored row held names because "the Serializer has always
    /// defaulted human-readable"; the RAW serializer never has, and every
    /// `remote_audit` row in prod held an `Int32` when measured (2026-09-25).
    #[test]
    fn the_raw_serializer_the_driver_uses_stores_bits() {
        let p = Permissions::VIEW | Permissions::INPUT;
        let raw = bson::to_raw_document_buf(&StoredPerms { permissions: p }).unwrap();
        let elem = raw.get("permissions").unwrap().expect("present");
        assert_eq!(
            elem.element_type(),
            bson::spec::ElementType::Int32,
            "{elem:?}"
        );
        assert_eq!(elem.as_i32(), Some(3));
    }

    /// The canary behind the type docs: serde's `ContentDeserializer` reports
    /// human-readable whatever the format, so the blanket impl refuses bits
    /// under a tagged enum EVEN on the raw path. If this ever starts passing,
    /// serde began forwarding the flag — the opt-ins stay (both shapes are on
    /// disk), but the docs can be simplified.
    #[test]
    fn under_a_tagged_enum_the_raw_path_reads_as_human_readable() {
        let err = bson::from_slice::<Row<TaggedWire>>(&tagged_row(bson::Bson::Int32(3)))
            .expect_err("the flag does not survive Content buffering");
        assert!(
            err.to_string()
                .contains("numeric permissions are not accepted here"),
            "{err}"
        );
    }

    /// The fix: the opt-in reads bits regardless of what the deserializer
    /// claims — through the driver's raw path AND the value API.
    #[test]
    fn deserialize_stored_reads_bits_under_a_tagged_enum() {
        let row: Row<TaggedStored> =
            bson::from_slice(&tagged_row(bson::Bson::Int32(3))).expect("raw path");
        let TaggedStored::Requested { permissions } = row.event;
        assert_eq!(permissions, Permissions::VIEW | Permissions::INPUT);

        let row: Row<TaggedStored> = bson::from_document(
            bson::doc! { "event": { "kind": "requested", "permissions": bson::Bson::Int64(1) } },
        )
        .expect("value API");
        let TaggedStored::Requested { permissions } = row.event;
        assert_eq!(permissions, Permissions::VIEW);
    }

    /// The other shape keeps reading: names are what `to_bson` writes and what
    /// an operator would hand-edit.
    #[test]
    fn deserialize_stored_reads_names_too() {
        let row: Row<TaggedStored> =
            bson::from_slice(&tagged_row("VIEW | INPUT".into())).expect("names");
        let TaggedStored::Requested { permissions } = row.event;
        assert_eq!(permissions, Permissions::VIEW | Permissions::INPUT);
    }

    /// Same strictness as the wire on names, fail-safe on bits.
    #[test]
    fn deserialize_stored_truncates_unknown_bits_and_refuses_unknown_names() {
        let row: Row<TaggedStored> =
            bson::from_slice(&tagged_row(bson::Bson::Int32(0x4001))).unwrap();
        let TaggedStored::Requested { permissions } = row.event;
        assert_eq!(
            permissions,
            Permissions::VIEW,
            "an unknown bit is dropped, never granted"
        );
        assert!(bson::from_slice::<Row<TaggedStored>>(&tagged_row("VIEW | NOPE".into())).is_err());
    }

    /// And the wire rule is untouched by any of it: names read, numbers do
    /// not, including through a tagged enum.
    #[test]
    fn the_json_wire_still_refuses_numbers_after_1630() {
        assert!(serde_json::from_str::<Permissions>("3").is_err());
        assert!(
            serde_json::from_str::<Row<TaggedWire>>(
                r#"{"event":{"kind":"requested","permissions":3}}"#
            )
            .is_err()
        );
        let row: Row<TaggedWire> =
            serde_json::from_str(r#"{"event":{"kind":"requested","permissions":"VIEW | INPUT"}}"#)
                .expect("names on the wire");
        let TaggedWire::Requested { permissions } = row.event;
        assert_eq!(permissions, Permissions::VIEW | Permissions::INPUT);
    }
}
