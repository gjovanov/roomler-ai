//! Serde helpers to pin ObjectId fields on the `rc:*` wire format to raw
//! 24-character hex strings — matching the REST responses and what the
//! browser/agent code can reasonably produce.
//!
//! Why this exists: `bson::oid::ObjectId` has a `Serialize` impl that, when
//! asked for human-readable output (serde_json), emits bson-extended JSON
//! `{"$oid":"<hex>"}`. That's correct for bson tooling but awkward for a
//! plain WS protocol where every other id is a hex string. The default
//! `Deserialize` already accepts both forms, so we only need to override
//! serialization.
//!
//! These helpers are ONLY for the wire protocol types (`signaling::*`).
//! Do not apply them to Mongo-backed model types — MongoDB relies on native
//! ObjectId encoding for indexes.

use bson::oid::ObjectId;
use serde::{Deserialize, Deserializer, Serializer};

pub mod oid_hex {
    use super::*;

    pub fn serialize<S: Serializer>(oid: &ObjectId, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&oid.to_hex())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<ObjectId, D::Error> {
        // Delegates to ObjectId's own impl, which accepts both `"hex"` and
        // `{"$oid":"hex"}`. We only care about pinning the serialize side;
        // inbound parsing stays lenient.
        ObjectId::deserialize(d)
    }
}

pub mod option_oid_hex {
    use super::*;

    pub fn serialize<S: Serializer>(oid: &Option<ObjectId>, s: S) -> Result<S::Ok, S::Error> {
        match oid {
            Some(o) => s.serialize_str(&o.to_hex()),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<ObjectId>, D::Error> {
        Option::<ObjectId>::deserialize(d)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Serialize;

    #[derive(Serialize, serde::Deserialize, Debug, PartialEq)]
    struct Wrap {
        #[serde(with = "oid_hex")]
        id: ObjectId,
        #[serde(with = "option_oid_hex")]
        maybe: Option<ObjectId>,
    }

    #[test]
    fn serialises_as_raw_hex() {
        let id = ObjectId::parse_str("507f1f77bcf86cd799439011").unwrap();
        let w = Wrap { id, maybe: Some(id) };
        let s = serde_json::to_string(&w).unwrap();
        // No `$oid` wrapping on either field.
        assert!(
            !s.contains("$oid"),
            "expected hex strings, got: {s}"
        );
        assert!(s.contains("\"507f1f77bcf86cd799439011\""));
    }

    #[test]
    fn accepts_plain_hex_inbound() {
        let json = r#"{"id":"507f1f77bcf86cd799439011","maybe":"507f1f77bcf86cd799439011"}"#;
        let w: Wrap = serde_json::from_str(json).unwrap();
        assert_eq!(w.id.to_hex(), "507f1f77bcf86cd799439011");
        assert!(w.maybe.is_some());
    }

    #[test]
    fn accepts_extended_json_inbound() {
        // Backward compat: servers / agents that still emit extended JSON
        // (say, while we're rolling out the fix) parse fine.
        let json = r#"{"id":{"$oid":"507f1f77bcf86cd799439011"},"maybe":{"$oid":"507f1f77bcf86cd799439011"}}"#;
        let w: Wrap = serde_json::from_str(json).unwrap();
        assert_eq!(w.id.to_hex(), "507f1f77bcf86cd799439011");
        assert!(w.maybe.is_some());
    }

    #[test]
    fn null_option_serialises_as_null() {
        let w = Wrap {
            id: ObjectId::new(),
            maybe: None,
        };
        let s = serde_json::to_string(&w).unwrap();
        assert!(s.contains("\"maybe\":null"));
    }
}
