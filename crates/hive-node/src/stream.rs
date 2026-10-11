// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P2d — the replica stream between a member and its source (spec §3b,
//! "The stream"): one QUIC connection per session, opened by the member, so a
//! member that was away catches up from where it stopped. This module is the
//! part with no I/O: the messages, their framing, and each side's half of
//! the opening exchange, read from a store.
//!
//! | Message | From | Says |
//! |---|---|---|
//! | `hello {v, session, fence, tip, floor, checkpoints}` | member | where its copy ends, and the `(seq, hash)` of its newest checkpoints |
//! | `hello {v, tip}` | source | the member's copy is a prefix of its chain; the stream goes on from the member's tip |
//! | `diverged {at}` | source | the member's tip is not in its chain: the newest checkpoint both hold |
//! | `refused {reason}` | source | it cannot serve: a version it does not speak, a session it does not hold |
//!
//! The events, blobs and acks that follow the opening exchange are P2d-3.
//!
//! ⚠️ A frame announces its length before its body, and a reader refuses a
//! length past [`MAX_FRAME`] before it allocates: the other side is a device
//! of the org, not a trusted one.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::chain::GENESIS;
use crate::store::{Store, StoreError, hex};

/// The stream's version. A source refuses a hello of any other.
pub const STREAM_V: u32 = 1;
/// The largest frame either side reads.
pub const MAX_FRAME: usize = 1024 * 1024;
/// How many of its newest checkpoints a member names in its hello: enough
/// to find one both sides hold after a divergence a promotion left behind.
pub const MAX_CHECKPOINTS: usize = 8;

/// A point in a session's chain: an event's `seq`, and its hash as
/// lowercase hex. Seq 0 is before the first event, at [`GENESIS`].
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Mark {
    pub seq: u64,
    pub hash: String,
}

impl Mark {
    /// Before the first event: where every chain starts.
    pub fn genesis() -> Self {
        Self {
            seq: 0,
            hash: hex(&GENESIS),
        }
    }

    fn of(seq: u64, hash: &[u8; 32]) -> Self {
        Self {
            seq,
            hash: hex(hash),
        }
    }
}

/// What the member sends.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum MemberMsg {
    /// The first frame: where the member's copy ends.
    Hello {
        v: u32,
        session: String,
        fence: u64,
        tip: Mark,
        floor: u64,
        /// Newest first, at most [`MAX_CHECKPOINTS`].
        checkpoints: Vec<Mark>,
    },
}

/// What the source sends.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum SourceMsg {
    /// The member's copy is a prefix of the source's chain, which ends at
    /// `tip`.
    Hello { v: u32, tip: Mark },
    /// The member's tip is not in the source's chain: `at` is the newest
    /// checkpoint both hold (genesis, if none), past which the member takes
    /// the source's events (P2d-3).
    Diverged { at: Mark },
    /// The source cannot serve this stream.
    Refused { reason: String },
}

/// Why a frame could not be read.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FrameError {
    #[error("a frame of {0} bytes is past the {MAX_FRAME}-byte limit")]
    TooLong(usize),
    #[error("an empty frame")]
    Empty,
    #[error("a frame that is not one: {0}")]
    Malformed(String),
}

/// One frame: a little-endian `u32` length, then that many bytes of JSON.
pub fn encode<T: Serialize>(msg: &T) -> Vec<u8> {
    let body = serde_json::to_vec(msg).expect("a stream message always serializes");
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    out
}

/// The body length a frame's header announces, refused before the reader
/// allocates it when it is past [`MAX_FRAME`].
pub fn frame_len(header: [u8; 4]) -> Result<usize, FrameError> {
    let len = u32::from_le_bytes(header) as usize;
    match len {
        0 => Err(FrameError::Empty),
        n if n > MAX_FRAME => Err(FrameError::TooLong(n)),
        n => Ok(n),
    }
}

/// A frame's body, as a message.
pub fn decode<T: DeserializeOwned>(body: &[u8]) -> Result<T, FrameError> {
    serde_json::from_slice(body).map_err(|e| FrameError::Malformed(e.to_string()))
}

/// The member's hello for `session`, from its own store.
pub fn member_hello(store: &Store, session: &str, fence: u64) -> Result<MemberMsg, StoreError> {
    let tip = store
        .tip(session)?
        .map_or_else(Mark::genesis, |t| Mark::of(t.seq, &t.hash));
    let floor = store.floor(session)?.unwrap_or(0);
    let checkpoints = store
        .checkpoint_marks(session, MAX_CHECKPOINTS)?
        .into_iter()
        .map(|(seq, hash)| Mark::of(seq, &hash))
        .collect();
    Ok(MemberMsg::Hello {
        v: STREAM_V,
        session: session.to_string(),
        fence,
        tip,
        floor,
        checkpoints,
    })
}

/// The source's answer to a member's hello, from its own store.
pub fn answer_hello(store: &Store, hello: &MemberMsg) -> Result<SourceMsg, StoreError> {
    let MemberMsg::Hello {
        v,
        session,
        tip,
        checkpoints,
        ..
    } = hello;
    if *v != STREAM_V {
        return Ok(SourceMsg::Refused {
            reason: "version".into(),
        });
    }
    let Some(own) = store.tip(session)? else {
        return Ok(SourceMsg::Refused {
            reason: "not_held".into(),
        });
    };
    if holds(store, session, tip)? {
        return Ok(SourceMsg::Hello {
            v: STREAM_V,
            tip: Mark::of(own.seq, &own.hash),
        });
    }
    // Newest first, as the member listed them; at most as many as it may.
    for cp in checkpoints.iter().take(MAX_CHECKPOINTS) {
        if holds(store, session, cp)? {
            return Ok(SourceMsg::Diverged { at: cp.clone() });
        }
    }
    Ok(SourceMsg::Diverged {
        at: Mark::genesis(),
    })
}

/// Whether the source's chain holds `mark`: genesis always, and otherwise
/// its event at `mark.seq`, hashed to `mark.hash`.
fn holds(store: &Store, session: &str, mark: &Mark) -> Result<bool, StoreError> {
    if mark.seq == 0 {
        return Ok(mark.hash == hex(&GENESIS));
    }
    Ok(store
        .event_at(session, mark.seq)?
        .is_some_and(|e| hex(&e.hash()) == mark.hash))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EventEnvelope, TranscriptEvent};

    /// Append `n` notes to `session` at `fence`, each chained on the last.
    fn grow(store: &mut Store, session: &str, fence: u64, n: usize) {
        for i in 0..n {
            let tip = store.tip(session).unwrap();
            let env = EventEnvelope::next(
                session,
                tip,
                fence,
                0,
                &TranscriptEvent::Note {
                    text: format!("note {i}"),
                },
            );
            store.append(&env).unwrap();
        }
    }

    fn hello(tip: Mark, checkpoints: Vec<Mark>) -> MemberMsg {
        MemberMsg::Hello {
            v: STREAM_V,
            session: "s".into(),
            fence: 1,
            tip,
            floor: 1,
            checkpoints,
        }
    }

    fn mark_at(store: &Store, seq: u64) -> Mark {
        let env = store.event_at("s", seq).unwrap().unwrap();
        Mark::of(seq, &env.hash())
    }

    /// The wire is locked: each message's shape, tagged by `t`.
    #[test]
    fn the_stream_messages_are_locked() {
        let m = hello(Mark::genesis(), vec![]);
        assert_eq!(
            serde_json::to_value(&m).unwrap(),
            serde_json::json!({
                "t": "hello", "v": 1, "session": "s", "fence": 1,
                "tip": {"seq": 0, "hash": "0".repeat(64)}, "floor": 1, "checkpoints": []
            })
        );
        let a = SourceMsg::Diverged {
            at: Mark::genesis(),
        };
        assert_eq!(
            serde_json::to_value(&a).unwrap(),
            serde_json::json!({"t": "diverged", "at": {"seq": 0, "hash": "0".repeat(64)}})
        );
        assert_eq!(
            serde_json::to_value(SourceMsg::Refused {
                reason: "not_held".into()
            })
            .unwrap(),
            serde_json::json!({"t": "refused", "reason": "not_held"})
        );
    }

    /// A frame is its length, then its body; a length past the limit, or
    /// none, is refused before anything is read into memory.
    #[test]
    fn a_frame_announces_its_length_and_a_long_one_is_refused_first() {
        let m = hello(Mark::genesis(), vec![]);
        let bytes = encode(&m);
        let len = frame_len(bytes[..4].try_into().unwrap()).unwrap();
        assert_eq!(len, bytes.len() - 4);
        assert_eq!(decode::<MemberMsg>(&bytes[4..]).unwrap(), m);
        assert_eq!(
            frame_len(((MAX_FRAME + 1) as u32).to_le_bytes()),
            Err(FrameError::TooLong(MAX_FRAME + 1))
        );
        assert_eq!(frame_len([0; 4]), Err(FrameError::Empty));
        assert!(matches!(
            decode::<MemberMsg>(b"{\"t\":\"goodbye\"}"),
            Err(FrameError::Malformed(_))
        ));
    }

    /// A member's copy that is a prefix of the source's chain is answered
    /// `hello` with the source's tip: an empty copy, one part of the way,
    /// and one at the very end.
    #[test]
    fn a_prefix_is_answered_hello_with_the_sources_tip() {
        let mut store = Store::open_in_memory().unwrap();
        grow(&mut store, "s", 1, 5);
        let own = mark_at(&store, 5);
        for tip in [Mark::genesis(), mark_at(&store, 3), own.clone()] {
            assert_eq!(
                answer_hello(&store, &hello(tip, vec![])).unwrap(),
                SourceMsg::Hello {
                    v: STREAM_V,
                    tip: own.clone()
                }
            );
        }
    }

    /// A tip the source's chain does not hold is answered `diverged`, at
    /// the newest of the member's checkpoints the source holds too, or at
    /// genesis when it holds none of them.
    #[test]
    fn a_tip_not_in_the_chain_is_answered_diverged_at_the_newest_common_checkpoint() {
        let mut store = Store::open_in_memory().unwrap();
        grow(&mut store, "s", 1, 6);
        let foreign = Mark {
            seq: 4,
            hash: "ee".repeat(32),
        };
        let common = mark_at(&store, 2);
        let theirs = Mark {
            seq: 3,
            hash: "dd".repeat(32),
        };
        assert_eq!(
            answer_hello(
                &store,
                &hello(foreign.clone(), vec![theirs.clone(), common.clone()])
            )
            .unwrap(),
            SourceMsg::Diverged { at: common },
            "the newest checkpoint both hold"
        );
        assert_eq!(
            answer_hello(&store, &hello(foreign.clone(), vec![theirs])).unwrap(),
            SourceMsg::Diverged {
                at: Mark::genesis()
            },
            "none both hold"
        );
        let ahead = Mark {
            seq: 9,
            hash: "aa".repeat(32),
        };
        assert_eq!(
            answer_hello(&store, &hello(ahead, vec![])).unwrap(),
            SourceMsg::Diverged {
                at: Mark::genesis()
            },
            "a tip past the source's own end is not in its chain"
        );
    }

    /// A version the source does not speak, or a session it does not hold,
    /// is refused in words.
    #[test]
    fn a_version_or_session_the_source_cannot_serve_is_refused() {
        let mut store = Store::open_in_memory().unwrap();
        grow(&mut store, "s", 1, 1);
        let MemberMsg::Hello {
            session,
            fence,
            tip,
            floor,
            checkpoints,
            ..
        } = hello(Mark::genesis(), vec![]);
        let older = MemberMsg::Hello {
            v: STREAM_V + 1,
            session: session.clone(),
            fence,
            tip: tip.clone(),
            floor,
            checkpoints: checkpoints.clone(),
        };
        assert_eq!(
            answer_hello(&store, &older).unwrap(),
            SourceMsg::Refused {
                reason: "version".into()
            }
        );
        let elsewhere = MemberMsg::Hello {
            v: STREAM_V,
            session: "other".into(),
            fence,
            tip,
            floor,
            checkpoints,
        };
        assert_eq!(
            answer_hello(&store, &elsewhere).unwrap(),
            SourceMsg::Refused {
                reason: "not_held".into()
            }
        );
    }

    /// A member's hello says where its copy ends: genesis when it holds
    /// nothing yet, its tip, its floor, and its newest checkpoints first.
    #[test]
    fn a_members_hello_reads_its_own_store() {
        let mut store = Store::open_in_memory().unwrap();
        store.join("s", "owner", 2).unwrap();
        assert_eq!(
            member_hello(&store, "s", 2).unwrap(),
            MemberMsg::Hello {
                v: STREAM_V,
                session: "s".into(),
                fence: 2,
                tip: Mark::genesis(),
                floor: 2,
                checkpoints: vec![],
            }
        );
        grow(&mut store, "s", 2, 2);
        let MemberMsg::Hello { tip, .. } = member_hello(&store, "s", 2).unwrap();
        assert_eq!(tip, mark_at(&store, 2));
    }
}
