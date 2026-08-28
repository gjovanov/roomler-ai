// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
import { describe, it, expect } from 'vitest'
import {
  CHUNK_HEADER_BYTES,
  createChunkFraming,
  stripChunkPrefix,
} from '@/workers/rc-chunk-framing'

/** Build one framed message the way `peer.rs::chunk_framed` does. The
 *  agent side locks the same layout in
 *  `video_bytes_wire_tests::chunk_prefix_layout_matches_worker_assembler`;
 *  these two tests are the two halves of one wire contract, and a change
 *  to either that isn't mirrored should fail here. */
function framed(seq: number, idx: number, count: number, payload: number[]): Uint8Array {
  const out = new Uint8Array(CHUNK_HEADER_BYTES + payload.length)
  const view = new DataView(out.buffer)
  view.setUint32(0, seq, true)
  view.setUint16(4, idx, true)
  view.setUint16(6, count, true)
  out.set(payload, CHUNK_HEADER_BYTES)
  return out
}

describe('rc-chunk-framing', () => {
  it('strips the prefix and passes the payload through in order', () => {
    const st = createChunkFraming()
    const a = stripChunkPrefix(st, framed(1, 0, 3, [10, 11]))
    const b = stripChunkPrefix(st, framed(1, 1, 3, [12]))
    const c = stripChunkPrefix(st, framed(1, 2, 3, [13]))
    expect([...(a.payload ?? [])]).toEqual([10, 11])
    expect([...(b.payload ?? [])]).toEqual([12])
    expect([...(c.payload ?? [])]).toEqual([13])
    expect([a.gap, b.gap, c.gap]).toEqual([false, false, false])
    expect(st.gaps).toBe(0)
  })

  it('reports a gap once and discards the rest of a broken frame', () => {
    const st = createChunkFraming()
    stripChunkPrefix(st, framed(1, 0, 4, [1]))
    // chunk 1 is lost; 2 and 3 arrive.
    const r2 = stripChunkPrefix(st, framed(1, 2, 4, [3]))
    const r3 = stripChunkPrefix(st, framed(1, 3, 4, [4]))
    expect(r2.gap).toBe(true)
    expect(r2.payload).toBeNull()
    // The break is reported ONCE — a long resync must not look like a
    // burst of independent losses, and each `gap` costs a keyframe request.
    expect(r3.gap).toBe(false)
    expect(r3.payload).toBeNull()
    expect(st.gaps).toBe(1)
    expect(st.discarded).toBe(1)
  })

  it('resynchronises on the next chunk 0 and resumes cleanly', () => {
    const st = createChunkFraming()
    stripChunkPrefix(st, framed(1, 0, 2, [1]))
    stripChunkPrefix(st, framed(2, 1, 2, [9])) // wrong seq -> gap
    expect(st.resyncing).toBe(true)
    const fresh = stripChunkPrefix(st, framed(3, 0, 2, [7]))
    expect(fresh.payload).not.toBeNull()
    expect(fresh.gap).toBe(false)
    expect(st.resyncing).toBe(false)
    const second = stripChunkPrefix(st, framed(3, 1, 2, [8]))
    expect([...(second.payload ?? [])]).toEqual([8])
    expect(st.gaps).toBe(1)
  })

  it('accepts a new frame that arrives while the previous one is truncated, and reports the loss', () => {
    const st = createChunkFraming()
    stripChunkPrefix(st, framed(1, 0, 3, [1]))
    // Frame 1's chunks 1-2 never arrive; frame 2 starts.
    const r = stripChunkPrefix(st, framed(2, 0, 1, [2]))
    // The new frame is DELIVERED — dropping it too would turn one lost
    // chunk into two lost frames — but the truncation is still reported
    // so the caller resets its byte assembler and asks for an IDR.
    expect([...(r.payload ?? [])]).toEqual([2])
    expect(r.gap).toBe(true)
    expect(st.gaps).toBe(1)
  })

  it('treats an out-of-spec prefix as a break rather than guessing', () => {
    const st = createChunkFraming()
    // chunk_count of 0: the agent floors it at 1, so this cannot be ours.
    expect(stripChunkPrefix(st, framed(1, 0, 0, [1])).gap).toBe(true)
    const st2 = createChunkFraming()
    // idx >= count is unsatisfiable.
    expect(stripChunkPrefix(st2, framed(1, 5, 2, [1])).gap).toBe(true)
    const st3 = createChunkFraming()
    // Too short to even hold a prefix — reading chunk_idx out of payload
    // bytes would be worse than declaring a break.
    expect(stripChunkPrefix(st3, new Uint8Array([1, 2, 3])).gap).toBe(true)
  })

  it('handles a single-chunk frame without leaving state behind', () => {
    const st = createChunkFraming()
    const r = stripChunkPrefix(st, framed(1, 0, 1, [42]))
    expect([...(r.payload ?? [])]).toEqual([42])
    expect(st.expectSeq).toBeNull()
    // The very next single-chunk frame must not read as a gap.
    expect(stripChunkPrefix(st, framed(2, 0, 1, [43])).gap).toBe(false)
    expect(st.gaps).toBe(0)
  })

  it('survives frame_seq wrapping past u32', () => {
    const st = createChunkFraming()
    stripChunkPrefix(st, framed(0xffffffff, 0, 2, [1]))
    const r = stripChunkPrefix(st, framed(0xffffffff, 1, 2, [2]))
    expect(r.gap).toBe(false)
    // The agent wraps with `wrapping_add`, so 0 legitimately follows.
    expect(stripChunkPrefix(st, framed(0, 0, 1, [3])).gap).toBe(false)
  })
})
