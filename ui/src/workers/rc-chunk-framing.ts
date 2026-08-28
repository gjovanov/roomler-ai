/**
 * FR-17 stage A — the receive half of per-chunk framing.
 *
 * The agent splits each frame into 16 KiB DataChannel messages. Before
 * FR-17 those messages were a bare byte stream: the receiver concatenated
 * them and trusted SCTP's reliable+ordered guarantee to deliver every one,
 * in order, forever. That guarantee is exactly what stage B gives up — an
 * unordered / partially-reliable channel is what stops one lost chunk from
 * head-of-line-blocking every frame behind it (`send_wait_max` hit
 * 10,263 ms in the field on a healthy agent).
 *
 * A byte stream cannot survive that, because a receiver has no way to tell
 * "the next 16 KiB of this frame" from "the first 16 KiB of the next one".
 * So each message now carries an 8-byte prefix:
 *
 *   bytes [0..4)  frame_seq,   u32 little-endian
 *   bytes [4..6)  chunk_idx,   u16 little-endian
 *   bytes [6..8)  chunk_count, u16 little-endian
 *
 * which is enough to detect a gap, discard the frame it belongs to, and
 * resynchronise on the next frame boundary instead of feeding the decoder
 * a spliced bitstream.
 *
 * ⚠️ This module is deliberately SHARED by both workers rather than copied
 * into each. The rule "what counts as a gap" has to be one function: FR-10
 * shipped a spacing rule that lived in one of its two call sites and the
 * other one silently didn't have it, which is the defect this file's
 * existence is meant to prevent.
 *
 * ⚠️ Stage A leaves the channel `{ ordered: true }`, so in production a gap
 * here is not expected — the value of landing it first is that the
 * assembler's gap handling gets validated while the transport still
 * guarantees it can't fire. Stage B then flips one property instead of
 * debugging two at once.
 */

/** Size of the per-message prefix, in bytes. Locked against the agent's
 *  `peer.rs::CHUNK_HEADER_BYTES` by tests on both sides. */
export const CHUNK_HEADER_BYTES = 8

export interface ChunkFramingState {
  /** `frame_seq` of the frame currently being assembled; null between
   *  frames (i.e. the next chunk must be a chunk 0). */
  expectSeq: number | null
  /** The `chunk_idx` the next message of the current frame must carry. */
  expectIdx: number
  /** `chunk_count` announced by the current frame's chunk 0. */
  expectCount: number
  /** After a gap we refuse everything until a fresh chunk 0 arrives —
   *  the tail of a broken frame is not a frame, and passing it on would
   *  produce exactly the spliced bitstream this design exists to avoid. */
  resyncing: boolean
  /** Cumulative gap count, surfaced in the worker's stats so a field
   *  session can distinguish "the transport lost chunks" from "the
   *  decoder is unhappy". A counter nothing reads is not evidence. */
  gaps: number
  /** Cumulative count of messages discarded while resyncing. */
  discarded: number
  /** `frame_seq` of the most recently STARTED frame, kept after that
   *  frame completes so a late chunk of it can be recognised as a
   *  straggler rather than mistaken for a break in the next one. */
  lastSeq: number | null
  /** Cumulative count of late chunks belonging to a frame already moved
   *  past. Under an unordered channel these are expected; a rising count
   *  with steady fps is the transport working, not failing. */
  stragglers: number
}

export function createChunkFraming(): ChunkFramingState {
  return {
    expectSeq: null,
    expectIdx: 0,
    expectCount: 0,
    resyncing: false,
    gaps: 0,
    discarded: 0,
    lastSeq: null,
    stragglers: 0,
  }
}

export interface ChunkFramingResult {
  /** The framed message's payload with the prefix removed, or null when
   *  this message must not reach the byte assembler. */
  payload: Uint8Array | null
  /** True when this message revealed a break in the stream. The caller
   *  must reset its byte assembler and request a keyframe — a partially
   *  assembled frame's header/size state is meaningless once a chunk of
   *  it is missing. Reported ONCE per break, not once per discarded
   *  message, so a long resync doesn't look like a burst of losses. */
  gap: boolean
}

/**
 * Validate and strip one framed DataChannel message.
 *
 * Returns the payload to append to the byte assembler, or `null` when the
 * message is a fragment of a frame that can no longer be completed.
 */
/** How far back a `frame_seq` may be and still count as a late chunk of a
 *  frame we have moved past, rather than the start of a new stream.
 *
 *  ⚠️ Bounded on purpose. "Older than what we have" is the natural rule,
 *  but an UNBOUNDED version deadlocks the moment the sender's counter
 *  restarts (a fresh send task begins again at 1): every subsequent frame
 *  would look ancient, be straggled, and the picture would stop for good
 *  with no gap reported and nothing in the logs. A window turns that
 *  failure into a single resync. 64 frames is ~1 s of video at 60 fps —
 *  far beyond any plausible reordering, far below a counter restart. */
const STRAGGLER_WINDOW = 64

/** Wrap-safe "`seq` belongs to a frame we have already moved past, and
 *  recently enough that reordering explains it". The agent advances
 *  `frame_seq` with `wrapping_add`, so 0 legitimately follows
 *  0xffffffff — a plain `<` would read that wrap as a jump backwards and
 *  mis-classify every chunk of the frame after it. */
function isStraggler(seq: number, lastSeq: number | null): boolean {
  if (lastSeq === null) return false
  const back = (lastSeq - seq) >>> 0
  return back > 0 && back <= STRAGGLER_WINDOW
}

export function stripChunkPrefix(
  st: ChunkFramingState,
  bytes: Uint8Array,
): ChunkFramingResult {
  // A message too short to carry a prefix cannot be classified at all —
  // treat it as a break rather than guessing, since the alternative is
  // reading `chunk_idx` out of frame payload bytes.
  if (bytes.byteLength < CHUNK_HEADER_BYTES) {
    return breakStream(st)
  }
  const view = new DataView(bytes.buffer, bytes.byteOffset, CHUNK_HEADER_BYTES)
  const seq = view.getUint32(0, true)
  const idx = view.getUint16(4, true)
  const count = view.getUint16(6, true)
  const payload = bytes.subarray(CHUNK_HEADER_BYTES)

  // `chunk_count` of zero is out of spec: the agent floors it at one so
  // that even a zero-length frame is announced as one chunk.
  if (count === 0 || idx >= count) {
    return breakStream(st)
  }

  // ⚠️ A STRAGGLER: a chunk of a frame we have already moved past.
  //
  // Under stage A's ordered channel this cannot happen. Under stage B's
  // unordered one it is routine — chunk 3 of frame N can arrive after
  // chunk 0 of frame N+1. Treating it as a gap would let a late chunk of
  // an already-lost frame destroy the HEALTHY frame after it, and that
  // cascades: every dropped frame costs an IDR, and each IDR is the
  // largest frame on the thinnest pipe.
  //
  // Checked BEFORE the chunk-0 branch, because a late chunk 0 is just as
  // able to restart assembly on a dead frame as a late chunk 3 is to
  // break a live one.
  if (isStraggler(seq, st.lastSeq)) {
    st.stragglers++
    return { payload: null, gap: false }
  }

  if (idx === 0) {
    // A fresh frame always resynchronises us, INCLUDING out of a resync —
    // that is the whole point of a frame boundary. If we were mid-frame,
    // the previous one is truncated and its loss is reported here; the
    // new frame is still accepted, because dropping it too would turn one
    // lost chunk into two lost frames.
    const truncated = !st.resyncing && st.expectSeq !== null
    st.expectSeq = seq
    st.lastSeq = seq
    st.expectIdx = 1
    st.expectCount = count
    st.resyncing = false
    // A single-chunk frame is complete on arrival. Skipping this check
    // would leave `expectSeq` set, and the NEXT frame's chunk 0 would
    // then read as a truncation — a false gap, and a keyframe request,
    // on every frame small enough to fit one message.
    closeIfComplete(st)
    if (truncated) {
      st.gaps++
      return { payload, gap: true }
    }
    return { payload, gap: false }
  }

  // Mid-frame chunk while resyncing: the frame it belongs to was already
  // written off. Drop it quietly — the break was reported when it happened.
  if (st.resyncing) {
    st.discarded++
    return { payload: null, gap: false }
  }

  if (st.expectSeq === null) {
    // Between frames. A chunk of the frame we JUST finished is a late
    // duplicate — harmless, and not covered by `isStraggler`, whose
    // "back > 0" is what keeps the normal mid-frame case (seq === lastSeq)
    // from being straggled.
    if (seq === st.lastSeq) {
      st.stragglers++
      return { payload: null, gap: false }
    }
    // Otherwise a NEWER frame whose chunk 0 never arrived. Its opening
    // bytes carry the size the byte assembler needs, so there is nothing
    // to salvage.
    return breakStream(st)
  }

  // Mid-frame chunk that isn't the one we expect: a lost chunk, or a
  // newer frame whose chunk 0 we missed. Both end this frame.
  if (seq !== st.expectSeq || idx !== st.expectIdx || count !== st.expectCount) {
    return breakStream(st)
  }

  st.expectIdx++
  closeIfComplete(st)
  return { payload, gap: false }
}

/** Clear the per-frame state once every announced chunk has arrived, so
 *  the next message is required to open a new frame. One function rather
 *  than a check at each call site: the two paths that can complete a
 *  frame (a single-chunk frame, and the last chunk of a multi-chunk one)
 *  must agree on what "complete" means. */
function closeIfComplete(st: ChunkFramingState): void {
  if (st.expectIdx >= st.expectCount) {
    st.expectSeq = null
    st.expectIdx = 0
    st.expectCount = 0
  }
}

/** Enter resync and report the break once. */
function breakStream(st: ChunkFramingState): ChunkFramingResult {
  const firstBreak = !st.resyncing
  st.resyncing = true
  st.expectSeq = null
  st.expectIdx = 0
  st.expectCount = 0
  if (firstBreak) {
    st.gaps++
    return { payload: null, gap: true }
  }
  st.discarded++
  return { payload: null, gap: false }
}
