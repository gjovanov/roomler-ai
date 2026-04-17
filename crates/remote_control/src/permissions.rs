use serde::{Deserialize, Serialize};

bitflags::bitflags! {
    /// Per-session capability bitfield. The agent enforces these — the server
    /// only signals what was negotiated. This is the source of truth on what
    /// the controller can actually do.
    #[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
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
}
