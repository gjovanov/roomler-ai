//! Unit tests for the TURNS-over-TLS-over-TCP `Conn`-adapter framer.
//!
//! The full path (TCP connect + TLS handshake + TURN allocate +
//! ChannelData round-trip) is covered by an integration test that
//! spins up a local coturn — see `crates/tests/`. These tests pin
//! the byte-level framing rules without touching the network.

use super::tcp_turn_conn::parse_frame_len;

// ─────────────────────────────────────────────────────────────────────
// Synthetic STUN / ChannelData byte sequences
// ─────────────────────────────────────────────────────────────────────

/// Build the first 4 bytes of a STUN Binding Request with the given
/// attributes-length. Bytes 4..20 (magic cookie + txn ID) are filler.
fn stun_header(attributes_len: u16) -> Vec<u8> {
    // STUN type 0x0001 (Binding Request, class=00 indicator bits are 00).
    let mut hdr = vec![0x00, 0x01];
    hdr.extend_from_slice(&attributes_len.to_be_bytes());
    // Magic cookie 0x2112A442 + 12-byte txn ID — pad with zero so the
    // header length is the full 20 bytes that parse_frame_len adds to
    // the attributes_len.
    hdr.extend_from_slice(&[0x21, 0x12, 0xA4, 0x42]);
    hdr.extend_from_slice(&[0u8; 12]);
    hdr
}

/// Build a ChannelData header for channel 0x4000 + given data length.
/// Pads the body to a 4-byte boundary per RFC 5766 §11.5 (TCP).
fn chandata_frame(channel: u16, data_len: u16) -> Vec<u8> {
    assert!(channel & 0xC000 == 0x4000, "channel num must start with 01");
    let mut frame = Vec::new();
    frame.extend_from_slice(&channel.to_be_bytes());
    frame.extend_from_slice(&data_len.to_be_bytes());
    frame.extend(std::iter::repeat(0xCC).take(data_len as usize));
    while frame.len() % 4 != 0 {
        frame.push(0x00);
    }
    frame
}

// ─────────────────────────────────────────────────────────────────────
// parse_frame_len: STUN
// ─────────────────────────────────────────────────────────────────────

#[test]
fn stun_zero_attributes_is_20_bytes() {
    let hdr = stun_header(0);
    assert_eq!(parse_frame_len(&hdr).unwrap(), Some(20));
}

#[test]
fn stun_with_attributes_includes_length_field() {
    let hdr = stun_header(48);
    assert_eq!(parse_frame_len(&hdr).unwrap(), Some(20 + 48));
}

#[test]
fn stun_max_length_is_handled() {
    let hdr = stun_header(0xFFFF);
    assert_eq!(parse_frame_len(&hdr).unwrap(), Some(20 + 0xFFFF));
}

// ─────────────────────────────────────────────────────────────────────
// parse_frame_len: ChannelData
//
// CRITICAL — this is the R1 fix from the plan critique. ChannelData
// over TCP MUST be 4-byte aligned (RFC 5766 §11.5). The framer is
// responsible for consuming the pad bytes; missing them causes the
// next frame parse to read the pad bytes as a malformed header.
// ─────────────────────────────────────────────────────────────────────

#[test]
fn chandata_no_padding_when_length_is_aligned() {
    // Channel 0x4000, 4 bytes data → total = 4 + 4 = 8 (already aligned).
    let frame = chandata_frame(0x4000, 4);
    assert_eq!(frame.len(), 8);
    assert_eq!(parse_frame_len(&frame).unwrap(), Some(8));
}

#[test]
fn chandata_pads_to_next_4byte_boundary() {
    // 1 byte data → 4+1=5, rounded up to 8.
    let frame = chandata_frame(0x4000, 1);
    assert_eq!(frame.len(), 8);
    assert_eq!(parse_frame_len(&frame).unwrap(), Some(8));

    // 2 bytes → 4+2=6 → 8.
    let frame = chandata_frame(0x4000, 2);
    assert_eq!(frame.len(), 8);
    assert_eq!(parse_frame_len(&frame).unwrap(), Some(8));

    // 3 bytes → 4+3=7 → 8.
    let frame = chandata_frame(0x4000, 3);
    assert_eq!(frame.len(), 8);
    assert_eq!(parse_frame_len(&frame).unwrap(), Some(8));

    // 5 bytes → 4+5=9 → 12.
    let frame = chandata_frame(0x4000, 5);
    assert_eq!(frame.len(), 12);
    assert_eq!(parse_frame_len(&frame).unwrap(), Some(12));

    // 1450 bytes (typical MTU-ish RTP) → 4+1450=1454 → 1456.
    let frame = chandata_frame(0x4000, 1450);
    assert_eq!(frame.len(), 1456);
    assert_eq!(parse_frame_len(&frame).unwrap(), Some(1456));
}

#[test]
fn chandata_max_channel_number_is_valid() {
    // 0x7FFE is the last valid channel per RFC 5766. The first 2 bits
    // must be 01, so anything 0x4000..=0x7FFF passes the class check.
    let frame = chandata_frame(0x7FFE, 100);
    assert!(parse_frame_len(&frame).is_ok());
}

// ─────────────────────────────────────────────────────────────────────
// parse_frame_len: incomplete + malformed
// ─────────────────────────────────────────────────────────────────────

#[test]
fn incomplete_header_returns_none() {
    for buf in [
        &[][..],
        &[0x00][..],
        &[0x00, 0x01][..],
        &[0x00, 0x01, 0x00][..],
    ] {
        assert_eq!(parse_frame_len(buf).unwrap(), None, "buf={buf:?}");
    }
}

#[test]
fn malformed_top_bits_rejected() {
    // Top 2 bits 10 — reserved.
    let bad = vec![0x80, 0x00, 0x00, 0x00];
    assert!(parse_frame_len(&bad).is_err(), "0x80... must reject");
    // Top 2 bits 11 — reserved.
    let bad = vec![0xC0, 0x00, 0x00, 0x00];
    assert!(parse_frame_len(&bad).is_err(), "0xC0... must reject");
    let bad = vec![0xFF, 0xFF, 0x00, 0x00];
    assert!(parse_frame_len(&bad).is_err(), "0xFF... must reject");
}

// ─────────────────────────────────────────────────────────────────────
// Realistic sequences — multiple frames concatenated
//
// These don't exercise parse_frame_len directly but verify the
// invariant the recv_from() loop depends on: after consuming a frame's
// declared length, the next byte starts a new well-formed frame.
// ─────────────────────────────────────────────────────────────────────

#[test]
fn back_to_back_chandata_frames_align_correctly() {
    // Two frames: 5 bytes data (→ 12-byte frame with 3 pad), then
    // 4 bytes data (→ 8-byte frame, no pad). Concatenated.
    let mut stream = chandata_frame(0x4000, 5);
    stream.extend_from_slice(&chandata_frame(0x4001, 4));

    let first_len = parse_frame_len(&stream).unwrap().unwrap();
    assert_eq!(first_len, 12);
    let next = &stream[first_len..];
    let second_len = parse_frame_len(next).unwrap().unwrap();
    assert_eq!(second_len, 8);
    assert_eq!(next.len(), 8);
}

#[test]
fn stun_followed_by_chandata_aligns_correctly() {
    // STUN (Binding Request, no attributes → 20 bytes) followed by
    // ChannelData(5 bytes → 12 bytes). The "+3 to 4" rounding on the
    // ChannelData side must not bleed into the STUN parse.
    let mut stream = stun_header(0);
    stream.extend_from_slice(&chandata_frame(0x4000, 5));

    let first = parse_frame_len(&stream).unwrap().unwrap();
    assert_eq!(first, 20);
    let next = &stream[first..];
    let second = parse_frame_len(next).unwrap().unwrap();
    assert_eq!(second, 12);
    assert_eq!(next.len(), 12);
}
