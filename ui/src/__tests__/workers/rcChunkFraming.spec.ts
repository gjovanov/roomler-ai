// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
import { describe, it, expect } from 'vitest'
import {
  CHUNK_HEADER_BYTES,
  MAX_PARTIAL_FRAMES,
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

const bytes = (r: { payload: Uint8Array | null }) => [...(r.payload ?? [])]

describe('rc-chunk-framing', () => {
  it('emits the whole frame once, on its last chunk', () => {
    const st = createChunkFraming()
    const a = stripChunkPrefix(st, framed(1, 0, 3, [10, 11]))
    const b = stripChunkPrefix(st, framed(1, 1, 3, [12]))
    const c = stripChunkPrefix(st, framed(1, 2, 3, [13]))
    // ⚠️ Stage C changed WHERE the bytes wait, not how many arrive: the
    // frame is emitted whole rather than chunk by chunk, because an
    // out-of-order chunk cannot be appended to a stream.
    expect(a.payload).toBeNull()
    expect(b.payload).toBeNull()
    expect(bytes(c)).toEqual([10, 11, 12, 13])
    expect([a.gap, b.gap, c.gap]).toEqual([false, false, false])
    expect(st.gaps).toBe(0)
  })

  it('handles a single-chunk frame without leaving state behind', () => {
    const st = createChunkFraming()
    const r = stripChunkPrefix(st, framed(1, 0, 1, [42]))
    expect(bytes(r)).toEqual([42])
    expect(st.partials.size).toBe(0)
    // The very next single-chunk frame must not read as a gap.
    expect(stripChunkPrefix(st, framed(2, 0, 1, [43])).gap).toBe(false)
    expect(st.gaps).toBe(0)
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

  it('reports a run of garbage as ONE break, not one per message', () => {
    // Each gap costs a keyframe request, and an IDR is the largest frame
    // on the thinnest pipe: reporting per message would answer a burst of
    // loss with a burst of the most expensive frames there are.
    const st = createChunkFraming()
    expect(stripChunkPrefix(st, new Uint8Array([1])).gap).toBe(true)
    expect(stripChunkPrefix(st, new Uint8Array([2])).gap).toBe(false)
    expect(stripChunkPrefix(st, new Uint8Array([3])).gap).toBe(false)
    expect(st.gaps).toBe(1)
    expect(st.discarded).toBe(2)
    // …and a placeable message ends the episode, so the NEXT break is
    // reported again rather than swallowed forever.
    stripChunkPrefix(st, framed(9, 0, 1, [1]))
    expect(st.resyncing).toBe(false)
    expect(stripChunkPrefix(st, new Uint8Array([4])).gap).toBe(true)
    expect(st.gaps).toBe(2)
  })

  it('survives frame_seq wrapping past u32', () => {
    const st = createChunkFraming()
    stripChunkPrefix(st, framed(0xffffffff, 0, 2, [1]))
    const r = stripChunkPrefix(st, framed(0xffffffff, 1, 2, [2]))
    expect(r.gap).toBe(false)
    expect(bytes(r)).toEqual([1, 2])
    // The agent wraps with `wrapping_add`, so 0 legitimately follows.
    expect(stripChunkPrefix(st, framed(0, 0, 1, [3])).gap).toBe(false)
  })
})

describe('rc-chunk-framing under UNORDERED delivery (FR-17 stages B + C)', () => {
  it('assembles a frame whose chunks arrive in ANY order', () => {
    // The case stage A got wrong: under an unordered channel this is the
    // COMMON path, not the rare one, and the strict rule read every such
    // frame as a break — a keyframe request per frame, worse than the
    // head-of-line blocking stage B removes.
    const st = createChunkFraming()
    expect(stripChunkPrefix(st, framed(7, 2, 4, [3])).payload).toBeNull()
    expect(stripChunkPrefix(st, framed(7, 0, 4, [1])).payload).toBeNull()
    expect(stripChunkPrefix(st, framed(7, 3, 4, [4])).payload).toBeNull()
    const done = stripChunkPrefix(st, framed(7, 1, 4, [2]))
    // Reassembled in INDEX order, not arrival order.
    expect(bytes(done)).toEqual([1, 2, 3, 4])
    expect(done.gap).toBe(false)
    expect(st.gaps).toBe(0)
  })

  it('interleaves two frames without either destroying the other', () => {
    const st = createChunkFraming()
    stripChunkPrefix(st, framed(1, 0, 2, [1]))
    stripChunkPrefix(st, framed(2, 0, 2, [10]))
    const one = stripChunkPrefix(st, framed(1, 1, 2, [2]))
    expect(bytes(one)).toEqual([1, 2])
    const two = stripChunkPrefix(st, framed(2, 1, 2, [11]))
    expect(bytes(two)).toEqual([10, 11])
    expect(st.gaps).toBe(0)
  })

  it('abandons a frame a NEWER completed one has overtaken, and says so', () => {
    // Holding it would re-introduce the head-of-line delay the unordered
    // channel exists to remove, and buy nothing: the codec is
    // delta-chained, so a frame whose predecessor never arrived is
    // undecodable anyway. The gap is what asks for the repairing IDR.
    const st = createChunkFraming()
    stripChunkPrefix(st, framed(1, 0, 3, [1])) // frame 1 loses chunks 1-2
    const r = stripChunkPrefix(st, framed(2, 0, 1, [9]))
    // The newer frame is still DELIVERED — dropping it too would turn one
    // lost frame into two.
    expect(bytes(r)).toEqual([9])
    expect(st.gaps).toBe(1)
    expect(st.partials.size).toBe(0)
    // ⚠️ The FLAG, not only the counter. The workers act on `gap` alone
    // (reset the byte assembler, ask for the resync IDR) and read the
    // counter only into their stats. From #1169 to 2026-10-29 this path
    // counted the abandonment and returned `gap: false`, so frame 2 went
    // to the decoder as a delta against a reference that never arrived,
    // with no IDR asked for — and this test, asserting `st.gaps` only,
    // stayed green the whole time.
    expect(r.gap).toBe(true)
  })

  it('signals an abandonment made on the LAST chunk of the overtaking frame', () => {
    // The completion path proper: frame 2 was already open when its last
    // chunk arrived, so no eviction ran and `finish()` alone did the
    // abandoning — the path that returned `gap: false` unconditionally.
    const st = createChunkFraming()
    stripChunkPrefix(st, framed(1, 0, 2, [1])) // frame 1, chunk 1 lost
    stripChunkPrefix(st, framed(2, 0, 2, [10])) // frame 2 opens beside it
    const r = stripChunkPrefix(st, framed(2, 1, 2, [11])) // …and completes
    expect(bytes(r)).toEqual([10, 11])
    expect(r.gap).toBe(true)
    expect(st.gaps).toBe(1)
    expect(st.partials.size).toBe(0)
    // Reported once: the next frame is clean, and frame 1's late chunk
    // is a straggler, not a second gap.
    expect(stripChunkPrefix(st, framed(3, 0, 1, [12])).gap).toBe(false)
    const late = stripChunkPrefix(st, framed(1, 1, 2, [2]))
    expect(late.gap).toBe(false)
    expect(late.payload).toBeNull()
    expect(st.stragglers).toBe(1)
    expect(st.gaps).toBe(1)
  })

  it('raises the flag once per MESSAGE however many frames that message abandoned', () => {
    // An eviction and a completion in one message. The flag is a boolean
    // because one IDR request is all a message can usefully ask for; the
    // counter is what says how many frames died.
    const st = createChunkFraming()
    for (let seq = 1; seq <= MAX_PARTIAL_FRAMES; seq++) {
      stripChunkPrefix(st, framed(seq, 0, 2, [seq])) // each one chunk short
    }
    const r = stripChunkPrefix(st, framed(MAX_PARTIAL_FRAMES + 1, 0, 1, [9]))
    expect(bytes(r)).toEqual([9])
    expect(r.gap).toBe(true)
    // One evicted to make room, the rest abandoned when the newcomer completed.
    expect(st.gaps).toBe(MAX_PARTIAL_FRAMES)
    expect(st.partials.size).toBe(0)
    expect(stripChunkPrefix(st, framed(MAX_PARTIAL_FRAMES + 2, 0, 1, [10])).gap).toBe(false)
  })

  it('does not let a late chunk of a lost frame destroy the next one', () => {
    // The cascade this exists to prevent: frame 1 loses chunk 1, frame 2
    // completes, then frame 1's chunk 2 finally arrives. Without the
    // straggler rule that late chunk restarts assembly on a dead frame —
    // one lost frame becoming two, and each dropped frame costs an IDR.
    const st = createChunkFraming()
    stripChunkPrefix(st, framed(1, 0, 3, [1]))
    stripChunkPrefix(st, framed(2, 0, 1, [2])) // frame 2 completes, 1 abandoned
    expect(st.gaps).toBe(1)
    const late = stripChunkPrefix(st, framed(1, 2, 3, [9]))
    expect(late.gap).toBe(false)
    expect(late.payload).toBeNull()
    expect(st.stragglers).toBe(1)
    expect(st.gaps).toBe(1) // no second gap
  })

  it('treats a duplicate slot as a straggler, not a break', () => {
    const st = createChunkFraming()
    stripChunkPrefix(st, framed(5, 0, 2, [1]))
    const dup = stripChunkPrefix(st, framed(5, 0, 2, [1]))
    expect(dup.gap).toBe(false)
    expect(dup.payload).toBeNull()
    expect(st.stragglers).toBe(1)
    // The frame still completes on its real second chunk.
    expect(bytes(stripChunkPrefix(st, framed(5, 1, 2, [2])))).toEqual([1, 2])
    expect(st.gaps).toBe(0)
  })

  it('treats a late chunk of a COMPLETED frame as a straggler', () => {
    const st = createChunkFraming()
    stripChunkPrefix(st, framed(5, 0, 2, [1]))
    stripChunkPrefix(st, framed(5, 1, 2, [2])) // frame 5 complete
    const dup = stripChunkPrefix(st, framed(5, 1, 2, [2]))
    expect(dup.gap).toBe(false)
    expect(st.stragglers).toBe(1)
    expect(st.gaps).toBe(0)
  })

  it('bounds how many partial frames it will hold', () => {
    // Unbounded buffering on a lossy link is a memory leak with a slow
    // fuse: every frame that loses a chunk would be retained forever, on
    // the exact link where frames lose chunks.
    const st = createChunkFraming()
    for (let seq = 1; seq <= MAX_PARTIAL_FRAMES + 2; seq++) {
      stripChunkPrefix(st, framed(seq, 0, 2, [seq]))
      expect(st.partials.size).toBeLessThanOrEqual(MAX_PARTIAL_FRAMES)
    }
    // Each eviction is a real loss and is reported as one.
    expect(st.gaps).toBe(2)
  })

  it('classifies stragglers correctly across a u32 wrap', () => {
    // The agent advances frame_seq with `wrapping_add`, so 0 legitimately
    // follows 0xffffffff. A plain `<` comparison would read that wrap as
    // a jump backwards and mis-classify every chunk after it.
    const st = createChunkFraming()
    stripChunkPrefix(st, framed(0xfffffffe, 0, 1, [1])) // completes
    stripChunkPrefix(st, framed(0xffffffff, 0, 1, [2])) // completes
    // A re-delivered chunk from before the wrap is a straggler, NOT a
    // reason to start assembling a frame already moved past.
    const late = stripChunkPrefix(st, framed(0xfffffffe, 0, 1, [1]))
    expect(late.payload).toBeNull()
    expect(st.stragglers).toBe(1)
    // …and a fresh frame after the wrap is accepted, not straggled.
    const fresh = stripChunkPrefix(st, framed(0, 0, 1, [3]))
    expect(bytes(fresh)).toEqual([3])
  })

  it('resyncs instead of stalling forever if the sender restarts its counter', () => {
    // ⚠️ The failure an UNBOUNDED "older than what we have" rule would
    // cause: a fresh send task begins again at 1, every frame looks
    // ancient, is straggled, and the picture stops for good — with no gap
    // reported and nothing in the logs.
    const st = createChunkFraming()
    stripChunkPrefix(st, framed(5000, 0, 1, [1]))
    const restarted = stripChunkPrefix(st, framed(1, 0, 1, [2]))
    expect(bytes(restarted)).toEqual([2])
    expect(st.stragglers).toBe(0)
  })

  it('refuses a seq that changes its own chunk_count mid-frame', () => {
    // Two different frames colliding on one sequence number; guessing
    // which is right would splice their bytes together.
    const st = createChunkFraming()
    stripChunkPrefix(st, framed(3, 0, 4, [1]))
    const r = stripChunkPrefix(st, framed(3, 1, 2, [2]))
    expect(r.payload).toBeNull()
    expect(st.gaps).toBe(1)
    expect(st.partials.size).toBe(0)
    // The frame is written off: the chunk of it we held never reaches the
    // decoder and the next frame is a delta against it, so the caller
    // needs the flag here as much as on an eviction. (Until 2026-10-29
    // this path, too, counted the gap and returned `gap: false`.)
    expect(r.gap).toBe(true)
    // The frame after it is clean — the gap was reported once.
    expect(stripChunkPrefix(st, framed(4, 0, 1, [3])).gap).toBe(false)
    expect(st.gaps).toBe(1)
  })
})

describe('rc-chunk-framing — the flag is the counter, made visible', () => {
  it('raises the flag exactly when the counter moves, on every message of a lossy, reordered stream', () => {
    // The one rule, as a property: `gap` is true on precisely the messages
    // that make `gaps` grow, whichever path handled them. Both 2026-10-29
    // defects were paths that moved the counter and returned `gap: false`;
    // a third would fail here before it reached a decoder.
    let x = 0x2545f491
    const rnd = (n: number): number => {
      x = (Math.imul(x, 1103515245) + 12345) >>> 0
      return (x >>> 8) % n
    }
    const msgs: Uint8Array[] = []
    for (let seq = 1; seq <= 600; seq++) {
      const count = 1 + rnd(4)
      for (let idx = 0; idx < count; idx++) {
        // ~8 % chunk loss: a four-chunk frame loses one about 30 % of the time.
        if (rnd(12) === 0) continue
        msgs.push(framed(seq, idx, count, [seq & 0xff, idx]))
      }
      // The odd unclassifiable message, so `abandonAll` is on the path too.
      if (rnd(60) === 0) msgs.push(new Uint8Array([seq & 0xff]))
    }
    // Reorder within a short window, as an unordered channel does.
    for (let i = 0; i + 1 < msgs.length; i++) {
      const j = Math.min(i + rnd(4), msgs.length - 1)
      ;[msgs[i], msgs[j]] = [msgs[j], msgs[i]]
    }
    const st = createChunkFraming()
    let flagged = 0
    let delivered = 0
    for (const m of msgs) {
      const before = st.gaps
      const r = stripChunkPrefix(st, m)
      expect(r.gap).toBe(st.gaps > before)
      if (r.gap) flagged++
      if (r.payload) {
        delivered++
        // A delivered frame is whole and in index order: every chunk's
        // payload carries its own idx, so the bytes say which slot it
        // sat in, and its seq byte says no other frame was spliced in.
        for (let k = 0; k < r.payload.length; k += 2) {
          expect(r.payload[k + 1]).toBe(k / 2)
          expect(r.payload[k]).toBe(r.payload[0])
        }
      }
    }
    // The stream did lose frames and did deliver frames — otherwise the
    // property above was vacuous.
    expect(flagged).toBeGreaterThan(0)
    expect(delivered).toBeGreaterThan(0)
    expect(st.gaps).toBeGreaterThanOrEqual(flagged)
  })

  it('never raises the flag on an ORDERED stream — the default channel is untouched', () => {
    // On `{ ordered: true }` a frame cannot be overtaken, a seq cannot
    // collide and nothing is evicted, so this fix changes nothing there.
    const st = createChunkFraming()
    let seq = 1
    for (const count of [1, 3, 2, 1, 1, 4, 2, 1, 3]) {
      for (let idx = 0; idx < count; idx++) {
        const r = stripChunkPrefix(st, framed(seq, idx, count, [idx]))
        expect(r.gap).toBe(false)
        expect(r.payload !== null).toBe(idx === count - 1)
      }
      seq++
    }
    expect(st.gaps).toBe(0)
    expect(st.stragglers).toBe(0)
    expect(st.partials.size).toBe(0)
  })
})
