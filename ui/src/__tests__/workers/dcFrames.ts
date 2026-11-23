// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * FR-148 P4b-2 — video frames as the agent puts them on a data channel, for
 * the black-box worker specs.
 *
 * A frame is `frame_video_bytes`'s record: a 13-byte header (u32 payload size,
 * u8 flags with bit 0 the keyframe, u64 timestamp in µs) and the payload. With
 * chunk framing (FR-17) the agent's send task cuts the record into slices of at
 * most 16 KiB (`SCTP_CHUNK_SIZE` in `peer.rs`) and prefixes each with 8 bytes:
 * u32 frame_seq, u16 chunk_idx, u16 chunk_count, all little-endian.
 */

/** Frame bytes per message, as the agent's send tasks cut a frame. */
export const AGENT_CHUNK_BYTES = 16 * 1024

/** One frame as `frame_video_bytes` builds it. Its payload bytes are its own
 *  (they follow its timestamp), so a frame spliced from two shows. */
export function wireFrame(key: boolean, timestampUs: number, payloadBytes: number): Uint8Array {
  const out = new Uint8Array(13 + payloadBytes)
  const view = new DataView(out.buffer)
  view.setUint32(0, payloadBytes, true)
  view.setUint8(4, key ? 1 : 0)
  view.setUint32(5, timestampUs >>> 0, true)
  view.setUint32(9, Math.floor(timestampUs / 2 ** 32), true)
  for (let i = 0; i < payloadBytes; i++) out[13 + i] = (timestampUs + i) & 0xff
  return out
}

/** The messages one frame goes out as, in order: its 16 KiB slices, each
 *  behind the FR-17 prefix. */
export function agentChunks(seq: number, frame: Uint8Array): ArrayBuffer[] {
  const count = Math.max(1, Math.ceil(frame.length / AGENT_CHUNK_BYTES))
  const out: ArrayBuffer[] = []
  for (let idx = 0; idx < count; idx++) {
    const part = frame.subarray(idx * AGENT_CHUNK_BYTES, Math.min(frame.length, (idx + 1) * AGENT_CHUNK_BYTES))
    const buf = new ArrayBuffer(8 + part.length)
    const view = new DataView(buf)
    view.setUint32(0, seq >>> 0, true)
    view.setUint16(4, idx, true)
    view.setUint16(6, count, true)
    new Uint8Array(buf, 8).set(part)
    out.push(buf)
  }
  return out
}

/** A seeded generator, `n` → an integer in [0, n): the same seed gives the
 *  same stream on every run and every machine. */
export function seeded(seed: number): (n: number) => number {
  let x = seed >>> 0
  return (n) => {
    x = (Math.imul(x, 1103515245) + 12345) >>> 0
    return (x >>> 8) % n
  }
}

export interface StreamFrame {
  seq: number
  key: boolean
  timestampUs: number
  /** The whole record, header included. */
  frame: Uint8Array
  /** Time since the frame before it reached the viewer. */
  gapMs: number
}

/**
 * What an agent of today sends on `video-bytes`, in order and whole: a few
 * leading deltas first (an async encoder's queue drains ahead of the forced
 * key frame, rc.103), then key frames at random spacing, frame sizes that fall
 * on, and either side of, the 16 KiB chunk edges, and a random time between
 * frames with the odd long silence.
 */
export function oldAgentStream(seed: number, frames = 160): StreamFrame[] {
  const rnd = seeded(seed)
  const out: StreamFrame[] = []
  let untilKey = rnd(4)
  let timestampUs = 1_000_000 + rnd(1_000)
  for (let i = 0; i < frames; i++) {
    const key = untilKey === 0
    untilKey = key ? 1 + rnd(90) : untilKey - 1
    let payload: number
    if (rnd(6) === 0) {
      // A record that ends on a chunk edge, or a byte either side of one.
      const chunks = 1 + rnd(key ? 4 : 2)
      payload = chunks * AGENT_CHUNK_BYTES - 13 + (rnd(3) - 1)
    } else {
      payload = key ? 20_000 + rnd(70_000) : 1 + rnd(20_000)
    }
    const gapMs = rnd(40) === 0 ? 1_600 + rnd(400) : 1 + rnd(40)
    timestampUs += gapMs * 1_000
    out.push({ seq: i + 1, key, timestampUs, frame: wireFrame(key, timestampUs, payload), gapMs })
  }
  return out
}
