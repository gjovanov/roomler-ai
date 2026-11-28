// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-22 P5a — what a control socket's close frame says when the socket was a
//! hand-over probe that was not used.
//!
//! At a network change a device opens a second control socket beside the one
//! it has, up to the upgrade's answer, and says no hello on it. The server
//! registers a device at its hello and not before, so that socket registers
//! nothing. When the device does not need it, because the socket it already
//! had answered first, it closes the second one with this reason. The server
//! then reads that end as the expected one it is, and not as a device that
//! opened a socket and never spoke.
//!
//! The agent writes it and the server reads it, compared whole.

/// The close frame's reason. A control frame carries at most 125 bytes, two of
/// them the status code, which leaves 123 for a reason.
pub const UNUSED_HANDOVER_PROBE: &str = "handover-probe-unused";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_reason_fits_a_close_frame() {
        assert!(UNUSED_HANDOVER_PROBE.len() <= 123);
        assert!(UNUSED_HANDOVER_PROBE.is_ascii());
    }
}
