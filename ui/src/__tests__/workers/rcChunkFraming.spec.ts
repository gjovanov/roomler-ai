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

describe('rc-chunk-framing under UNORDERED delivery (FR-17 stage B)', () => {
  it('does not let a late chunk of a lost frame destroy the next one', () => {
    // The cascade this exists to prevent: frame 1 loses chunk 1, frame 2
    // starts cleanly, then frame 1's chunk 2 finally arrives. Before the
    // straggler rule that late chunk broke frame 2 as well — one lost
    // frame becoming two, and each dropped frame costs an IDR.
    const st = createChunkFraming()
    stripChunkPrefix(st, framed(1, 0, 3, [1]))
    stripChunkPrefix(st, framed(2, 0, 2, [2]))   // frame 1 truncated, frame 2 opens
    expect(st.gaps).toBe(1)
    const late = stripChunkPrefix(st, framed(1, 2, 3, [9]))  // frame 1 straggler
    expect(late.gap).toBe(false)
    expect(late.payload).toBeNull()
    expect(st.stragglers).toBe(1)
    // Frame 2 must still complete normally.
    const finish = stripChunkPrefix(st, framed(2, 1, 2, [3]))
    expect(finish.gap).toBe(false)
    expect([...(finish.payload ?? [])]).toEqual([3])
    expect(st.gaps).toBe(1) // no second gap
  })

  it('treats a late chunk of a COMPLETED frame as a straggler, not a break', () => {
    const st = createChunkFraming()
    stripChunkPrefix(st, framed(5, 0, 2, [1]))
    stripChunkPrefix(st, framed(5, 1, 2, [2]))   // frame 5 complete
    const dup = stripChunkPrefix(st, framed(5, 1, 2, [2]))
    expect(dup.gap).toBe(false)
    expect(st.stragglers).toBe(1)
    expect(st.gaps).toBe(0)
  })

  it('still reports a real break when a new frame loses its chunk 0', () => {
    // A straggler is "older than what we have"; a mid-chunk of a NEWER
    // frame means its opening bytes — which carry the frame size — never
    // arrived, and that is a genuine break.
    const st = createChunkFraming()
    stripChunkPrefix(st, framed(1, 0, 1, [1]))   // frame 1 complete
    const r = stripChunkPrefix(st, framed(2, 1, 2, [7]))
    expect(r.gap).toBe(true)
    expect(st.gaps).toBe(1)
  })

  it('classifies stragglers correctly across a u32 wrap', () => {
    // The agent advances frame_seq with `wrapping_add`, so 0 legitimately
    // follows 0xffffffff. A plain `<` comparison would read that wrap as
    // a jump backwards and mis-classify every chunk after it.
    const st = createChunkFraming()
    stripChunkPrefix(st, framed(0xfffffffe, 0, 1, [1]))
    stripChunkPrefix(st, framed(0xffffffff, 0, 2, [2]))
    // A re-delivered chunk 0 from before the wrap is a straggler, NOT a
    // reason to restart assembly on a frame already moved past.
    const late = stripChunkPrefix(st, framed(0xfffffffe, 0, 1, [1]))
    expect(late.payload).toBeNull()
    expect(st.stragglers).toBe(1)
    // ...and a fresh frame after the wrap is accepted, not straggled.
    const fresh = stripChunkPrefix(st, framed(0, 0, 1, [3]))
    expect(fresh.payload).not.toBeNull()
    expect([...(fresh.payload ?? [])]).toEqual([3])
  })

  it('resyncs instead of stalling forever if the sender restarts its counter', () => {
    // ⚠️ The failure an UNBOUNDED "older than what we have" rule would
    // cause: a fresh send task begins again at 1, every frame looks
    // ancient, everything is straggled, and the picture stops for good
    // with no gap reported and nothing in the logs. The window turns that
    // into one resync.
    const st = createChunkFraming()
    stripChunkPrefix(st, framed(50_000, 0, 1, [1]))
    const restarted = stripChunkPrefix(st, framed(1, 0, 1, [2]))
    expect(restarted.payload).not.toBeNull()
    expect([...(restarted.payload ?? [])]).toEqual([2])
    // The next frame of the restarted stream flows normally.
    expect(stripChunkPrefix(st, framed(2, 0, 1, [3])).payload).not.toBeNull()
  })
})
