// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! #3553 — the one line the daemon writes for an error and the causes behind
//! it. The signalling loop, the preflight and the self-view all format an
//! error's `source()` chain through [`error_chain`], so the rule below holds
//! on every line that walks one.

/// `err` and its `source()` chain as one colon-joined line, each cause once.
///
/// rc.58: a top-level `Display` alone hides the root cause. reqwest says
/// `error sending request`, and the signalling loop's own contexts say
/// `ws connect` or `ws read`, while the useful part (a TLS failure,
/// `ECONNREFUSED`, `EAI_NONAME`) sits further down the chain. Field repro
/// 2026-05-24: a flaky network turned every cold start into `error=ws connect`
/// with no further detail, so a DNS failure could not be told from a TLS one.
///
/// A cause whose text is already on the line is not appended again. Many
/// wrappers print the error they wrap inside their own message and also return
/// it from `source()` (thiserror's `#[from]`). tungstenite's `IO error: {0}`
/// over an `io::Error`, and `WebSocket protocol error: {0}` over a
/// `ProtocolError`, made the #3530 lab's reconnect line read
/// `heartbeat send: ws send: IO error: Broken pipe (os error 32): Broken pipe
/// (os error 32)`. The walk goes on below a cause it skips, so a deeper cause
/// with new text still prints. A chain whose levels do not repeat each other
/// prints every level.
///
/// ⚠️ Only the text changes. The error is not touched, so a caller that
/// downcasts it still reads it as before: FR-28's load-balancer 502 check, and
/// the reconnect's `AgentDidNotRun`.
pub(crate) fn error_chain(err: &(dyn std::error::Error + 'static)) -> String {
    let mut line = err.to_string();
    let mut next = err.source();
    while let Some(cause) = next {
        let text = cause.to_string();
        if !line.contains(&text) {
            line.push_str(": ");
            line.push_str(&text);
        }
        next = cause.source();
    }
    line
}

#[cfg(test)]
mod tests {
    use super::error_chain;
    use std::fmt;
    use tokio_tungstenite::tungstenite::{self, error::ProtocolError};

    /// A hand-built error: its own text, and the error below it.
    #[derive(Debug)]
    struct Level {
        text: &'static str,
        below: Option<Box<Level>>,
    }

    impl fmt::Display for Level {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(self.text)
        }
    }

    impl std::error::Error for Level {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            self.below
                .as_deref()
                .map(|below| below as &(dyn std::error::Error + 'static))
        }
    }

    /// A chain of levels, the first text on top.
    fn levels(texts: &[&'static str]) -> Level {
        let (top, rest) = texts.split_first().expect("at least one level");
        Level {
            text: top,
            below: (!rest.is_empty()).then(|| Box::new(levels(rest))),
        }
    }

    /// AC1 — the #3530 lab's heartbeat line. tungstenite's `Io` prints the io
    /// error it wraps and also returns it from `source()`.
    #[test]
    fn a_websocket_io_error_names_its_io_cause_once() {
        // EPIPE is 32 on Linux and macOS, the daemon's two unix targets.
        let io = std::io::Error::from_raw_os_error(32);
        let io_text = io.to_string();
        let e = anyhow::Error::new(tungstenite::Error::Io(io))
            .context("ws send")
            .context("heartbeat send");
        let line = error_chain(e.as_ref());
        assert_eq!(
            line,
            format!("heartbeat send: ws send: IO error: {io_text}")
        );
        assert_eq!(line.matches(io_text.as_str()).count(), 1, "{line}");
        #[cfg(unix)]
        assert_eq!(
            line,
            "heartbeat send: ws send: IO error: Broken pipe (os error 32)"
        );
    }

    /// AC2 — the #3530 lab's read line. tungstenite's `Protocol` prints the
    /// `ProtocolError` it wraps and also returns it from `source()`.
    #[test]
    fn a_websocket_protocol_reset_names_it_once() {
        let reset = tungstenite::Error::Protocol(ProtocolError::ResetWithoutClosingHandshake);
        let e = anyhow::Error::new(reset).context("ws read");
        let line = error_chain(e.as_ref());
        assert_eq!(
            line,
            "ws read: WebSocket protocol error: Connection reset without closing handshake"
        );
        assert_eq!(
            line.matches("Connection reset without closing handshake")
                .count(),
            1,
            "{line}"
        );
    }

    /// AC3 — a cause that repeats the line's tail is skipped, and the walk
    /// goes on below it: a deeper cause with new text still prints.
    #[test]
    fn a_new_cause_below_a_repeated_one_still_prints() {
        let e = levels(&[
            "TLS error: alert received",
            "alert received",
            "certificate expired",
        ]);
        assert_eq!(
            error_chain(&e),
            "TLS error: alert received: certificate expired"
        );
    }

    /// Levels that do not repeat each other all print, outer to inner, as
    /// before #3553. Field logs are grepped in this shape.
    #[test]
    fn levels_that_do_not_repeat_each_other_all_print() {
        let e = levels(&["ws connect", "tls handshake", "ECONNREFUSED"]);
        assert_eq!(error_chain(&e), "ws connect: tls handshake: ECONNREFUSED");
        let e = anyhow::Error::new(std::io::Error::other("ECONNREFUSED"))
            .context("tls handshake")
            .context("ws connect");
        assert_eq!(
            error_chain(e.as_ref()),
            "ws connect: tls handshake: ECONNREFUSED"
        );
        assert_eq!(error_chain(&levels(&["simple"])), "simple");
    }

    /// A cause whose text is on the line anywhere, not only at its end, is
    /// the self-view's rule from before #3553, and its message keeps it.
    #[test]
    fn a_cause_already_anywhere_on_the_line_is_not_repeated() {
        let e = levels(&["no answer (timed out) after 5 s", "timed out"]);
        assert_eq!(error_chain(&e), "no answer (timed out) after 5 s");
        // A cause with no text adds nothing, not a dangling separator.
        let e = levels(&["ws read", ""]);
        assert_eq!(error_chain(&e), "ws read");
    }
}
