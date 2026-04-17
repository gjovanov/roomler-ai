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
