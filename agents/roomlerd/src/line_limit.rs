// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! #3695 — a line reader with a limit, for the newline-delimited channels
//! between the daemon and the helpers it starts in a person's session: the
//! screen-capture helper's handshake ([`read_line`], blocking) and the session
//! worker's delegation channel ([`Lines`], async).
//!
//! The limit is checked before a line's buffer grows past it: the reader holds
//! at most `max` bytes of one line, and a longer line ends the read with an
//! [`io::ErrorKind::InvalidData`] error that names the limit. Every byte before
//! the newline counts, a `\r` included. In all else it reads as
//! `BufRead::read_line` and tokio's `Lines` do: the newline, and a `\r` before
//! it, are not returned; the end of the input after part of a line returns that
//! part; a line that is not UTF-8 is an error.

use std::io;

/// The error a line over `max` bytes ends the read with.
fn too_long(max: usize) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("a line longer than {max} bytes"),
    )
}

/// One line as it arrives: its bytes so far, and the limit.
#[derive(Debug)]
struct Line {
    bytes: Vec<u8>,
    max: usize,
    /// A byte of this line has arrived, so the end of the input returns it.
    started: bool,
}

impl Line {
    fn new(max: usize) -> Self {
        Self {
            bytes: Vec::new(),
            max,
            started: false,
        }
    }

    /// Take what `available` holds of this line, up to and including its
    /// newline: how many bytes were taken, and whether the line is whole. A
    /// line that would pass the limit is refused before anything is taken.
    fn take(&mut self, available: &[u8]) -> io::Result<(usize, bool)> {
        let (body, whole) = match available.iter().position(|b| *b == b'\n') {
            Some(at) => (&available[..at], true),
            None => (available, false),
        };
        if self.bytes.len() + body.len() > self.max {
            *self = Self::new(self.max);
            return Err(too_long(self.max));
        }
        self.bytes.extend_from_slice(body);
        self.started = true;
        Ok((body.len() + usize::from(whole), whole))
    }

    /// The line read, and this made ready for the next one.
    fn finish(&mut self) -> io::Result<String> {
        let mut bytes = std::mem::take(&mut self.bytes);
        self.started = false;
        if bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
        String::from_utf8(bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }

    /// At the end of the input: the part of a line that arrived, if any.
    fn at_end(&mut self) -> io::Result<Option<String>> {
        if self.started {
            self.finish().map(Some)
        } else {
            Ok(None)
        }
    }
}

/// Read one line of at most `max` bytes. `Ok(None)` at the end of the input.
///
/// Nothing past the newline is consumed, so what follows the line stays in
/// `r`: after the screen-capture helper's handshake, its frames.
pub fn read_line(r: &mut impl io::BufRead, max: usize) -> io::Result<Option<String>> {
    let mut line = Line::new(max);
    loop {
        let available = match r.fill_buf() {
            Ok(available) => available,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if available.is_empty() {
            return line.at_end();
        }
        let (taken, whole) = line.take(available)?;
        r.consume(taken);
        if whole {
            return line.finish().map(Some);
        }
    }
}

/// Lines of at most `max` bytes each, read from `inner`.
///
/// ⚠️ Cancel-safe, as a reader in a `tokio::select!` must be: the part of a
/// line read so far lives in `self`, and the only await is `fill_buf`, which
/// takes nothing from the reader; what it returns is taken within the same
/// poll. A `next_line` dropped halfway through a line loses none of it.
#[derive(Debug)]
pub struct Lines<R> {
    inner: R,
    line: Line,
}

impl<R: tokio::io::AsyncBufRead + Unpin> Lines<R> {
    pub fn new(inner: R, max: usize) -> Self {
        Self {
            inner,
            line: Line::new(max),
        }
    }

    /// The next line. `Ok(None)` at the end of the input.
    pub async fn next_line(&mut self) -> io::Result<Option<String>> {
        use tokio::io::AsyncBufReadExt;
        loop {
            let available = self.inner.fill_buf().await?;
            if available.is_empty() {
                return self.line.at_end();
            }
            let (taken, whole) = self.line.take(available)?;
            self.inner.consume(taken);
            if whole {
                return self.line.finish().map(Some);
            }
        }
    }

    /// The reader, positioned after the last line returned.
    pub fn into_inner(self) -> R {
        self.inner
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A line of exactly the limit is read; one byte more is refused, with
    /// the limit named, and the line's buffer never holds more than it.
    #[test]
    fn the_limit_is_held_to_the_byte() {
        let at = format!("{}\n", "a".repeat(16));
        let mut r = io::BufReader::new(at.as_bytes());
        assert_eq!(read_line(&mut r, 16).unwrap().unwrap().len(), 16);

        let over = format!("{}\n", "a".repeat(17));
        let mut r = io::BufReader::new(over.as_bytes());
        let e = read_line(&mut r, 16).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
        assert!(e.to_string().contains("16 bytes"), "{e}");

        let mut line = Line::new(16);
        assert_eq!(line.take(&[b'a'; 10]).unwrap(), (10, false));
        assert_eq!(line.take(&[b'a'; 6]).unwrap(), (6, false), "16, the limit");
        assert!(line.take(b"a").is_err(), "17 is over it");
        assert!(line.bytes.is_empty(), "nothing of the refused line is kept");
    }

    /// A line that arrives a byte at a time is the same line, and what
    /// follows it stays in the reader.
    #[test]
    fn a_line_split_across_reads_is_whole_and_the_rest_stays() {
        let mut r = io::BufReader::with_capacity(1, &b"one\r\ntwo\nRPWF"[..]);
        assert_eq!(read_line(&mut r, 8).unwrap().as_deref(), Some("one"));
        assert_eq!(read_line(&mut r, 8).unwrap().as_deref(), Some("two"));
        let mut rest = Vec::new();
        io::Read::read_to_end(&mut r, &mut rest).unwrap();
        assert_eq!(rest, b"RPWF", "nothing past the newline was taken");
    }

    /// The end of the input returns the part of a line that arrived, then
    /// `None`; an empty line is a line; a line that is not UTF-8 is an error.
    #[test]
    fn the_end_of_the_input_and_the_odd_lines() {
        let mut r = io::BufReader::new(&b"\npartial"[..]);
        assert_eq!(read_line(&mut r, 8).unwrap().as_deref(), Some(""));
        assert_eq!(read_line(&mut r, 8).unwrap().as_deref(), Some("partial"));
        assert_eq!(read_line(&mut r, 8).unwrap(), None);

        let mut r = io::BufReader::new(&b"\xff\xfe\n"[..]);
        let e = read_line(&mut r, 8).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
    }

    /// The async reader holds the same limit and reads the same lines.
    #[tokio::test]
    async fn the_async_reader_holds_the_limit() {
        // 15 bytes and a `\r`: 16 before the newline, the limit.
        let input = format!("{}\r\nnext\n{}\n", "a".repeat(15), "b".repeat(17));
        let mut lines = Lines::new(tokio::io::BufReader::with_capacity(3, input.as_bytes()), 16);
        assert_eq!(lines.next_line().await.unwrap().unwrap().len(), 15);
        assert_eq!(lines.next_line().await.unwrap().as_deref(), Some("next"));
        let e = lines.next_line().await.unwrap_err();
        assert!(e.to_string().contains("16 bytes"), "{e}");
    }

    /// A `next_line` dropped halfway through a line, as a `select!` drops the
    /// branch that lost, loses nothing: the next call returns the whole line.
    #[tokio::test]
    async fn a_dropped_next_line_loses_nothing() {
        use tokio::io::AsyncWriteExt;
        let (mut tx, rx) = tokio::io::duplex(64);
        let mut lines = Lines::new(tokio::io::BufReader::new(rx), 64);
        tx.write_all(b"half of ").await.unwrap();
        let waited =
            tokio::time::timeout(std::time::Duration::from_millis(50), lines.next_line()).await;
        assert!(waited.is_err(), "no whole line yet");
        tx.write_all(b"a line\nnext").await.unwrap();
        drop(tx);
        assert_eq!(
            lines.next_line().await.unwrap().as_deref(),
            Some("half of a line")
        );
        assert_eq!(lines.next_line().await.unwrap().as_deref(), Some("next"));
        assert_eq!(lines.next_line().await.unwrap(), None);
    }
}
