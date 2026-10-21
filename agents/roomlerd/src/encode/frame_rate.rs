// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-119 P1 — the frame rate a viewer asks for, and the answer it gets.
//!
//! A viewer on a high-refresh screen asks with `rc:fps {fps}` on the control
//! channel, and only of an agent that advertises [`CAP`] in `AgentCaps.video`.
//! The control handler stores the ask (`ViewerFeedback::store_fps_ask`). The
//! pump answers in `rc:video-info`, the message that already describes the
//! stream: `fps` is what the stream runs at, and when the viewer asked for
//! more, `fps_asked` and `fps_reason` say what was asked and why it was not
//! granted. The pump knows its own cap and transport, so it is the one place
//! the answer can be correct, including before the encoder exists.
//!
//! P1 grants nothing above the session's current cap. P2 builds the grant: a
//! direct path, a rate class a session has proven, and a pipe that covers the
//! ceiling (`docs/fr/FR-119-high-refresh-streaming.md` §3.2).

/// The capability verb in `AgentCaps.video`.
///
/// ⚠️ A wire spelling, matched by equality: renaming it makes every agent look
/// like it lacks the feature.
pub const CAP: &str = "fps-high";

/// The rates a viewer may ask for.
pub const CHOICES: [u32; 3] = [60, 120, 144];

/// The ask in an `rc:fps` message, when it is one of [`CHOICES`]. Anything else
/// is dropped, the way `rc:resolution` drops a mode it does not know.
pub fn parse_ask(msg: &serde_json::Value) -> Option<u32> {
    let fps = u32::try_from(msg.get("fps")?.as_u64()?).ok()?;
    CHOICES.contains(&fps).then_some(fps)
}

/// Why the stream runs below what the viewer asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// A relayed (constrained) path. Above its cap waits for FR-17 stage C,
    /// FR-63 and FR-70 M3.
    Constrained,
    /// The host has not proven it can sustain the rate. In P1 no host has.
    Unproven,
}

impl Reason {
    /// ⚠️ A wire spelling, read by the viewer.
    pub fn wire(self) -> &'static str {
        match self {
            Self::Constrained => "constrained",
            Self::Unproven => "unproven",
        }
    }
}

/// What `rc:video-info` says about the frame rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Answer {
    /// The rate the stream runs at: the pump's target.
    pub fps: u32,
    /// The viewer's ask and why it was not granted, when it asked for more than
    /// the stream runs at. `None` when nothing needs explaining.
    pub refused: Option<(u32, Reason)>,
}

/// P1's answer: the stream stays at the pump's `cap`. An ask at or below it needs
/// no explanation. An ask above it is named, with the reason.
pub fn answer(asked: Option<u32>, cap: u32, constrained: bool) -> Answer {
    let refused = asked.filter(|&a| a > cap).map(|a| {
        let why = if constrained {
            Reason::Constrained
        } else {
            Reason::Unproven
        };
        (a, why)
    });
    Answer { fps: cap, refused }
}

/// The `rc:video-info` fields for an answer, with a leading comma, so the
/// payload builder appends them as it does its other optional fields.
pub fn video_info_fields(a: &Answer) -> String {
    match a.refused {
        Some((asked, why)) => format!(
            r#","fps":{},"fps_asked":{asked},"fps_reason":"{}""#,
            a.fps,
            why.wire()
        ),
        None => format!(r#","fps":{}"#, a.fps),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_offered_rates_are_an_ask() {
        let ask = |v: serde_json::Value| parse_ask(&v);
        assert_eq!(
            ask(serde_json::json!({"t": "rc:fps", "fps": 120})),
            Some(120)
        );
        assert_eq!(ask(serde_json::json!({"t": "rc:fps", "fps": 60})), Some(60));
        assert_eq!(
            ask(serde_json::json!({"t": "rc:fps", "fps": 144})),
            Some(144)
        );
        // Not offered, not a number, absent, or out of range: dropped.
        assert_eq!(ask(serde_json::json!({"t": "rc:fps", "fps": 90})), None);
        assert_eq!(ask(serde_json::json!({"t": "rc:fps", "fps": "120"})), None);
        assert_eq!(ask(serde_json::json!({"t": "rc:fps"})), None);
        assert_eq!(ask(serde_json::json!({"t": "rc:fps", "fps": -60})), None);
        assert_eq!(
            ask(serde_json::json!({"t": "rc:fps", "fps": 4_294_967_416u64})),
            None
        );
    }

    /// P1 grants nothing above the cap, and names why.
    #[test]
    fn an_ask_above_the_cap_is_refused_with_its_reason() {
        assert_eq!(
            answer(Some(120), 60, false),
            Answer {
                fps: 60,
                refused: Some((120, Reason::Unproven)),
            }
        );
        assert_eq!(
            answer(Some(60), 30, true),
            Answer {
                fps: 30,
                refused: Some((60, Reason::Constrained)),
            }
        );
    }

    /// Nothing to explain when the stream already runs at what was asked, or
    /// faster, or when nobody asked.
    #[test]
    fn an_ask_the_cap_covers_needs_no_explanation() {
        let plain = Answer {
            fps: 60,
            refused: None,
        };
        assert_eq!(answer(Some(60), 60, false), plain);
        assert_eq!(answer(None, 60, false), plain);
        assert_eq!(answer(None, 60, true), plain);
    }

    /// The wire, byte for byte: the viewer parses these names.
    #[test]
    fn the_video_info_fields_are_what_the_viewer_reads() {
        assert_eq!(video_info_fields(&answer(None, 60, false)), r#","fps":60"#);
        assert_eq!(
            video_info_fields(&answer(Some(120), 60, false)),
            r#","fps":60,"fps_asked":120,"fps_reason":"unproven""#
        );
        assert_eq!(
            video_info_fields(&answer(Some(144), 30, true)),
            r#","fps":30,"fps_asked":144,"fps_reason":"constrained""#
        );
        assert_eq!(CAP, "fps-high");
    }
}
