//! `flow_id` framing for multiplexing many TCP flows onto a fixed
//! pool of WebRTC DataChannels.
//!
//! Wire shape: every DC message is `[flow_id: u32 LE | payload: bytes]`.
//! 4-byte overhead per message; at 64 KiB chunks that's 0.006%.
//!
//! Why multiplex instead of DC-per-flow: per-flow DCs (a) pay an RTT
//! setup per accepted TCP connection (50 ms on a TURN-relayed peer)
//! and (b) leak SCTP stream ids under JDBC-pool-style churn (the
//! upstream stream-id reclaim story is rudimentary). See plan §"What
//! changed from v1" #1. The pool size lives in T2 — start with 8.
//!
//! Real impl lands in T2. This stub locks the wire shape so other
//! modules (forward.rs, signaling.rs) can already reference it.

/// Size of the per-message `flow_id` prefix on the wire.
pub const FLOW_ID_HEADER_BYTES: usize = 4;

/// Encode a `flow_id` + payload into a single DC message.
pub fn encode(flow_id: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(FLOW_ID_HEADER_BYTES + payload.len());
    out.extend_from_slice(&flow_id.to_le_bytes());
    out.extend_from_slice(payload);
    out
}

/// Decode a DC message into `(flow_id, payload)`. Returns `None` if
/// the message is shorter than the header.
pub fn decode(msg: &[u8]) -> Option<(u32, &[u8])> {
    if msg.len() < FLOW_ID_HEADER_BYTES {
        return None;
    }
    let (head, rest) = msg.split_at(FLOW_ID_HEADER_BYTES);
    let flow_id = u32::from_le_bytes(head.try_into().ok()?);
    Some((flow_id, rest))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let body = b"hello world";
        let m = encode(0xdead_beef, body);
        let (id, p) = decode(&m).unwrap();
        assert_eq!(id, 0xdead_beef);
        assert_eq!(p, body);
    }

    #[test]
    fn short_message_returns_none() {
        assert!(decode(&[]).is_none());
        assert!(decode(&[0u8; 3]).is_none());
        // Exactly 4 bytes = flow_id with empty payload — valid.
        assert_eq!(decode(&[1, 0, 0, 0]).unwrap(), (1u32, &[][..]));
    }
}
