// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! #3517 — how an agent socket ended, named for the line that says it did.
//!
//! Until #3517 the agent socket's disconnect line carried the agent id and
//! nothing else, so a device that said goodbye, one whose TCP leg was reset,
//! one the receive deadline reaped and one the Hub cancelled all read the same
//! `remote-control agent WS disconnected`. Every exit of the read loop now
//! breaks with a [`SocketEnd`], so a new exit does not compile until it names
//! its cause, and the line carries it as fields: `cause`, then `code` and
//! `reason` for a close frame, `kind` and `error` for a failed read,
//! `elapsed_s` for a reap and `by` for a cancel. A field that does not apply is
//! left off the line.
//!
//! Server-side only: nothing here reaches the wire, and nothing logs per frame.
//! The grep that reads the line is in `docs/real-time.md` § How an agent
//! socket ended.

use axum::extract::ws::CloseFrame;

use crate::hub::CancelReason;

/// How one agent socket's read loop ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SocketEnd {
    /// The Hub's cancel fired, with the trigger it recorded
    /// ([`crate::hub::AgentCancel`]). `None` reads `by=unknown`.
    Cancelled(Option<CancelReason>),
    /// No frame for longer than the receive deadline: a half-open socket,
    /// reaped (Phase A-1).
    LivenessDeadline { elapsed_s: u64 },
    /// The stream ended with neither a frame nor an error. tokio-tungstenite
    /// ends it that way only after a close handshake or an error, and the loop
    /// leaves on both first, so this is not expected. It is an exit, so it is
    /// named.
    StreamEnd,
    /// The device said it is going to sleep (FR-55 P4).
    Sleep,
    /// A close frame with a status code. The reason is the peer's text, at
    /// most 123 bytes (a control frame's bound, which tungstenite enforces).
    Close { code: u16, reason: String },
    /// A close frame with no status code. The daemon's own clean stop sends
    /// this (`agents/roomlerd/src/signaling.rs`).
    CloseNoFrame,
    /// The read failed: [`error_kind`]'s word for it, and the error's text.
    Error { kind: &'static str, detail: String },
}

impl SocketEnd {
    /// A close frame, as the read loop received it.
    pub(crate) fn of_close(frame: Option<CloseFrame>) -> Self {
        match frame {
            Some(frame) => SocketEnd::Close {
                code: frame.code,
                reason: frame.reason.to_string(),
            },
            None => SocketEnd::CloseNoFrame,
        }
    }

    /// A failed read.
    pub(crate) fn of_error(e: &axum::Error) -> Self {
        SocketEnd::Error {
            kind: error_kind(e),
            detail: e.to_string(),
        }
    }

    /// The `cause` field: one word per exit.
    pub(crate) fn cause(&self) -> &'static str {
        match self {
            SocketEnd::Cancelled(_) => "cancelled",
            SocketEnd::LivenessDeadline { .. } => "liveness_deadline",
            SocketEnd::StreamEnd => "stream_end",
            SocketEnd::Sleep => "sleep",
            SocketEnd::Close { .. } => "close",
            SocketEnd::CloseNoFrame => "close_no_frame",
            SocketEnd::Error { .. } => "error",
        }
    }

    /// `code`: a close frame's status code.
    pub(crate) fn code(&self) -> Option<u16> {
        match self {
            SocketEnd::Close { code, .. } => Some(*code),
            _ => None,
        }
    }

    /// `reason`: a close frame's text.
    pub(crate) fn reason(&self) -> Option<&str> {
        match self {
            SocketEnd::Close { reason, .. } => Some(reason),
            _ => None,
        }
    }

    /// `kind`: a failed read's one word.
    pub(crate) fn kind(&self) -> Option<&'static str> {
        match self {
            SocketEnd::Error { kind, .. } => Some(kind),
            _ => None,
        }
    }

    /// `error`: a failed read's text.
    pub(crate) fn error(&self) -> Option<&str> {
        match self {
            SocketEnd::Error { detail, .. } => Some(detail),
            _ => None,
        }
    }

    /// `elapsed_s`: how long the reaped socket had been silent.
    pub(crate) fn elapsed_s(&self) -> Option<u64> {
        match self {
            SocketEnd::LivenessDeadline { elapsed_s } => Some(*elapsed_s),
            _ => None,
        }
    }

    /// `by`: which of the Hub's triggers cancelled the loop.
    pub(crate) fn by(&self) -> Option<&'static str> {
        match self {
            SocketEnd::Cancelled(reason) => Some(reason.map_or("unknown", CancelReason::label)),
            _ => None,
        }
    }
}

/// A failed read in one word: what the transport under the socket said.
///
/// axum hands back the error tungstenite returned, boxed (`axum::Error::new`),
/// so this reads it through `source()`. ⚠️ The downcast names the tungstenite
/// this crate pins, which must be the one axum links: if axum moves and the pin
/// does not, every error reads `other`. The loopback test
/// `a_dropped_tcp_leg_reads_eof` in `socket.rs` fails first.
pub(crate) fn error_kind(e: &axum::Error) -> &'static str {
    use std::io::ErrorKind;
    use tungstenite::error::ProtocolError;
    let Some(e) = std::error::Error::source(e).and_then(|s| s.downcast_ref::<tungstenite::Error>())
    else {
        return "other";
    };
    match e {
        tungstenite::Error::Io(io) => match io.kind() {
            ErrorKind::ConnectionReset => "reset",
            ErrorKind::ConnectionAborted => "aborted",
            ErrorKind::TimedOut => "timeout",
            ErrorKind::UnexpectedEof => "eof",
            ErrorKind::BrokenPipe => "broken_pipe",
            _ => "io",
        },
        // The TCP leg ended with no close frame: the device's process went
        // away, or the hop in front of the pod closed its side.
        tungstenite::Error::Protocol(ProtocolError::ResetWithoutClosingHandshake) => "eof",
        // A frame the protocol forbids, invalid UTF-8 in a text frame among
        // them (RFC 6455 fails the connection for both).
        tungstenite::Error::Protocol(_) | tungstenite::Error::Utf8(_) => "protocol",
        // A frame over `MAX_WS_MESSAGE_BYTES`.
        tungstenite::Error::Capacity(_) => "capacity",
        _ => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;
    use tungstenite::error::{CapacityError, ProtocolError};

    fn kind_of(e: tungstenite::Error) -> &'static str {
        error_kind(&axum::Error::new(e))
    }

    /// Each end carries its cause and its own fields, and no other end's:
    /// the line leaves an absent field off, so a stray `Some` would print a
    /// field that means nothing for that end.
    #[test]
    fn every_end_names_its_cause_and_only_its_own_fields() {
        let ends = [
            SocketEnd::Cancelled(Some(CancelReason::Replaced)),
            SocketEnd::LivenessDeadline { elapsed_s: 93 },
            SocketEnd::StreamEnd,
            SocketEnd::Sleep,
            SocketEnd::Close {
                code: 1000,
                reason: "bye".into(),
            },
            SocketEnd::CloseNoFrame,
            SocketEnd::Error {
                kind: "reset",
                detail: "IO error: Connection reset by peer (os error 104)".into(),
            },
        ];
        let rendered: Vec<String> = ends.iter().map(fields).collect();
        assert_eq!(
            rendered,
            [
                "cause=cancelled by=replaced",
                "cause=liveness_deadline elapsed_s=93",
                "cause=stream_end",
                "cause=sleep",
                "cause=close code=1000 reason=\"bye\"",
                "cause=close_no_frame",
                "cause=error kind=reset error=\"IO error: Connection reset by peer (os error 104)\"",
            ]
        );
    }

    /// The fields in the order and shape the disconnect line prints them.
    fn fields(end: &SocketEnd) -> String {
        let mut out = format!("cause={}", end.cause());
        if let Some(code) = end.code() {
            out += &format!(" code={code}");
        }
        if let Some(reason) = end.reason() {
            out += &format!(" reason={reason:?}");
        }
        if let Some(kind) = end.kind() {
            out += &format!(" kind={kind}");
        }
        if let Some(error) = end.error() {
            out += &format!(" error={error:?}");
        }
        if let Some(elapsed_s) = end.elapsed_s() {
            out += &format!(" elapsed_s={elapsed_s}");
        }
        if let Some(by) = end.by() {
            out += &format!(" by={by}");
        }
        out
    }

    /// Every trigger has its own word, and a cancel with none recorded still
    /// reads as a cancel.
    #[test]
    fn a_cancel_names_its_trigger_or_unknown() {
        let by = |reason| SocketEnd::Cancelled(reason).by();
        assert_eq!(by(Some(CancelReason::Replaced)), Some("replaced"));
        assert_eq!(by(Some(CancelReason::IdleNudge)), Some("idle_nudge"));
        assert_eq!(by(Some(CancelReason::Renumber)), Some("renumber"));
        assert_eq!(by(Some(CancelReason::Shutdown)), Some("shutdown"));
        assert_eq!(by(None), Some("unknown"));
    }

    #[test]
    fn a_close_frame_keeps_its_code_and_reason() {
        let frame = CloseFrame {
            code: 4003,
            reason: "agent_deleted".into(),
        };
        assert_eq!(
            SocketEnd::of_close(Some(frame)),
            SocketEnd::Close {
                code: 4003,
                reason: "agent_deleted".into(),
            }
        );
        assert_eq!(SocketEnd::of_close(None), SocketEnd::CloseNoFrame);
    }

    #[test]
    fn an_io_error_reads_as_its_kind() {
        let io = |kind| kind_of(tungstenite::Error::Io(io::Error::from(kind)));
        assert_eq!(io(io::ErrorKind::ConnectionReset), "reset");
        assert_eq!(io(io::ErrorKind::ConnectionAborted), "aborted");
        assert_eq!(io(io::ErrorKind::TimedOut), "timeout");
        assert_eq!(io(io::ErrorKind::UnexpectedEof), "eof");
        assert_eq!(io(io::ErrorKind::BrokenPipe), "broken_pipe");
        assert_eq!(io(io::ErrorKind::PermissionDenied), "io");
    }

    #[test]
    fn a_protocol_error_reads_eof_only_for_a_missing_close() {
        assert_eq!(
            kind_of(tungstenite::Error::Protocol(
                ProtocolError::ResetWithoutClosingHandshake
            )),
            "eof"
        );
        assert_eq!(
            kind_of(tungstenite::Error::Protocol(
                ProtocolError::UnmaskedFrameFromClient
            )),
            "protocol"
        );
        assert_eq!(
            kind_of(tungstenite::Error::Utf8("invalid".into())),
            "protocol"
        );
        assert_eq!(
            kind_of(tungstenite::Error::Capacity(
                CapacityError::MessageTooLong {
                    size: 2,
                    max_size: 1
                }
            )),
            "capacity"
        );
        assert_eq!(kind_of(tungstenite::Error::AttackAttempt), "other");
    }

    /// An error that is not tungstenite's (axum wraps nothing else today)
    /// reads `other` and keeps its text.
    #[test]
    fn a_foreign_error_reads_other() {
        let e = axum::Error::new(io::Error::from(io::ErrorKind::ConnectionReset));
        assert_eq!(
            SocketEnd::of_error(&e),
            SocketEnd::Error {
                kind: "other",
                detail: e.to_string(),
            }
        );
    }
}
