// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
import { describe, it, expect, vi, afterEach } from 'vitest'
import {
  BENCH_PROBE_EVERY_MS,
  BENCH_PROBE_JITTER_MS,
  BENCH_PROBE_TTL_MS,
  BENCH_STORAGE_KEY,
  BENCH_UPLOAD_EVERY,
  benchSessionActive,
  buildL4Upload,
  createBenchProber,
  createL4Uploader,
  createVideoBenchSampler,
  storedBench,
  type BenchBatch,
} from '@/composables/rcBench'

/** A hand-cranked timer: the prober's only clock is what the test sets. */
function manualTimers() {
  const pending: Array<{ fn: () => void; ms: number }> = []
  return {
    pending,
    setTimer: (fn: () => void, ms: number) => {
      const h = { fn, ms }
      pending.push(h)
      return h
    },
    clearTimer: (h: unknown) => {
      const i = pending.indexOf(h as { fn: () => void; ms: number })
      if (i >= 0) pending.splice(i, 1)
    },
    fire() {
      const h = pending.shift()
      h?.fn()
      return h
    },
  }
}

afterEach(() => {
  localStorage.removeItem(BENCH_STORAGE_KEY)
})

describe('rcBench — the gate', () => {
  it('needs the knob AND an agent that advertised bench', () => {
    expect(benchSessionActive(true, ['chunk-framing', 'bench'])).toBe(true)
    expect(benchSessionActive(true, ['chunk-framing'])).toBe(false)
    expect(benchSessionActive(true, undefined)).toBe(false)
    expect(benchSessionActive(false, ['bench'])).toBe(false)
    // Equality, never a prefix match: a future `bench-v2` is not `bench`.
    expect(benchSessionActive(true, ['bench-v2'])).toBe(false)
  })

  it('reads the knob only as an exact "1"', () => {
    expect(storedBench()).toBe(false)
    localStorage.setItem(BENCH_STORAGE_KEY, 'true')
    expect(storedBench()).toBe(false)
    localStorage.setItem(BENCH_STORAGE_KEY, '1')
    expect(storedBench()).toBe(true)
  })
})

describe('rcBench — the prober', () => {
  it('probes at a jittered interval: never phase-locked to the frame clock', () => {
    const t = manualTimers()
    let r = 0
    const randoms = [0, 0.5, 0.999]
    const p = createBenchProber({
      send: () => true,
      arm: () => {},
      now: () => 0,
      random: () => randoms[r++ % randoms.length]!,
      setTimer: t.setTimer,
      clearTimer: t.clearTimer,
    })
    p.start()
    const gaps: number[] = []
    for (let i = 0; i < 3; i++) gaps.push(t.fire()!.ms)
    for (const g of gaps) {
      expect(g).toBeGreaterThanOrEqual(BENCH_PROBE_EVERY_MS)
      expect(g).toBeLessThan(BENCH_PROBE_EVERY_MS + BENCH_PROBE_JITTER_MS)
    }
    expect(new Set(gaps).size).toBe(3)
  })

  it('numbers probes 1..4095 and never 0, arming each one it sent, after sending', () => {
    const t = manualTimers()
    const order: string[] = []
    const p = createBenchProber({
      send: (m) => {
        order.push(`send ${m.seq}`)
        return true
      },
      arm: (s) => order.push(`arm ${s}`),
      now: () => 0,
      random: () => 0,
      setTimer: t.setTimer,
      clearTimer: t.clearTimer,
    })
    p.start()
    t.fire()
    t.fire()
    expect(order).toEqual(['send 1', 'arm 1', 'send 2', 'arm 2'])
    for (let i = 0; i < 4093; i++) t.fire()
    order.length = 0
    t.fire() // the 4096th probe wraps past 4095 to 1, skipping 0
    expect(order).toEqual(['send 1', 'arm 1'])
  })

  it('arms nothing for a probe the channel refused', () => {
    const t = manualTimers()
    const armed: number[] = []
    const p = createBenchProber({
      send: () => false,
      arm: (s) => armed.push(s),
      now: () => 0,
      random: () => 0,
      setTimer: t.setTimer,
      clearTimer: t.clearTimer,
    })
    p.start()
    t.fire()
    expect(armed).toEqual([])
    expect(p.onPainted(1, 50, 'video')).toBeNull()
  })

  it('measures paint − send once per probe, and ignores what it never sent', () => {
    const t = manualTimers()
    let now = 1_000
    const p = createBenchProber({
      send: () => true,
      arm: () => {},
      now: () => now,
      random: () => 0,
      setTimer: t.setTimer,
      clearTimer: t.clearTimer,
    })
    p.start()
    t.fire() // probe 1 sent at 1000
    expect(p.onPainted(1, 1_045.5, 'hevc')).toBe(45.5)
    // The frames after it carry the same probe: only the FIRST counts.
    expect(p.onPainted(1, 1_062, 'hevc')).toBeNull()
    expect(p.onPainted(9, 1_070, 'hevc')).toBeNull()
    now = 1_300
    t.fire() // probe 2
    expect(p.onPainted(2, 1_290, 'hevc'), 'a paint before the send is no measurement').toBeNull()
    expect(p.stats()).toMatchObject({ n: 1, lost: 0, p50: 45.5 })
  })

  it('counts an unanswered probe lost after its TTL, and drops the in-flight ones on stop', () => {
    const t = manualTimers()
    let now = 0
    const batches: BenchBatch[] = []
    const p = createBenchProber({
      send: () => true,
      arm: () => {},
      flush: (b) => batches.push(b),
      now: () => now,
      random: () => 0,
      setTimer: t.setTimer,
      clearTimer: t.clearTimer,
    })
    p.start()
    t.fire() // probe 1 at 0, never answered
    now = BENCH_PROBE_TTL_MS + 1
    t.fire() // probe 2; probe 1 expires here
    expect(p.onPainted(1, now, 'video')).toBeNull()
    expect(p.stats().lost).toBe(1)
    p.stop() // probe 2 is in flight: dropped, NOT lost
    expect(p.stats().lost).toBe(1)
    expect(p.running()).toBe(false)
    expect(batches).toEqual([{ rtts: [], lost: 1, paths: [] }])
  })

  it(`flushes a batch every ${BENCH_UPLOAD_EVERY} round trips`, () => {
    const t = manualTimers()
    let now = 0
    const batches: BenchBatch[] = []
    const p = createBenchProber({
      send: () => true,
      arm: () => {},
      flush: (b) => batches.push(b),
      now: () => now,
      random: () => 0,
      setTimer: t.setTimer,
      clearTimer: t.clearTimer,
    })
    p.start()
    for (let i = 1; i <= BENCH_UPLOAD_EVERY + 1; i++) {
      now = i * 1_000
      t.fire()
      p.onPainted(i, now + 40, 'video')
    }
    expect(batches).toHaveLength(1)
    expect(batches[0]!.rtts).toHaveLength(BENCH_UPLOAD_EVERY)
    expect(batches[0]!.paths).toEqual(['video'])
    p.stop()
    expect(batches).toHaveLength(2)
    expect(batches[1]!.rtts).toEqual([40])
  })
})

describe('rcBench — the <video> sampler', () => {
  function fakeVideo() {
    const cbs: Array<(now: number, md: { presentationTime?: number }) => void> = []
    const video = {
      requestVideoFrameCallback: vi.fn((cb: (now: number, md: { presentationTime?: number }) => void) => {
        cbs.push(cb)
        return cbs.length
      }),
      cancelVideoFrameCallback: vi.fn(),
    } as unknown as HTMLVideoElement
    return { video, cbs }
  }

  it('answers the armed probe from the first presented frame carrying it, then stops asking', () => {
    const { video, cbs } = fakeVideo()
    const got: Array<[number, number]> = []
    let reads = [17, 42, 42]
    const s = createVideoBenchSampler({
      findVideo: () => video,
      onProbe: (seq, at) => got.push([seq, at]),
      read: () => reads.shift() ?? null,
      now: () => 0,
    })
    s.arm(42)
    expect(cbs).toHaveLength(1)
    cbs[0]!(0, { presentationTime: 100 }) // still carries probe 17
    expect(got).toEqual([])
    expect(cbs).toHaveLength(2)
    cbs[1]!(0, { presentationTime: 117 }) // the first frame with 42
    expect(got).toEqual([[42, performance.timeOrigin + 117]])
    // Nothing armed any more: no further frame callbacks, so no readbacks.
    expect(cbs).toHaveLength(2)
    reads = []
  })

  it('lets an unanswered probe expire instead of reading frames forever', () => {
    const { video, cbs } = fakeVideo()
    let now = 0
    const s = createVideoBenchSampler({
      findVideo: () => video,
      onProbe: () => {},
      read: () => null,
      now: () => now,
    })
    s.arm(5)
    now = BENCH_PROBE_TTL_MS + 1
    cbs[0]!(0, { presentationTime: 1 })
    expect(cbs).toHaveLength(1)
  })

  it('does nothing without a <video> to read', () => {
    const s = createVideoBenchSampler({ findVideo: () => null, onProbe: () => {}, read: () => 1 })
    expect(() => s.arm(1)).not.toThrow()
    s.stop()
  })
})

describe('rcBench — the rc.l4 record', () => {
  const ctx = {
    tenantId: '69f0000000000000000000a1',
    agentId: '69f0000000000000000000c3',
    sessionId: '69f0000000000000000000b2',
  }

  it('is exactly the body the server test `browser_l4_bench_record_parses` locks', () => {
    const body = buildL4Upload({ rtts: [45.6, 38, 61.2], lost: 1, paths: ['video'] }, ctx, 1_760_364_000_000)
    expect(body).toEqual({
      tenant_id: '69f0000000000000000000a1',
      source: 'browser',
      session_id: '69f0000000000000000000b2',
      lines: [
        {
          ts: { $date: { $numberLong: '1760364000000' } },
          level: 'INFO',
          target: 'rc.l4',
          msg: 'L4 n=3 lost=1 p50=45.6ms p95=61.2ms max=61.2ms path=video',
          fields: {
            v: 1,
            paths: ['video'],
            n: 3,
            lost: 1,
            p50_ms: 45.6,
            p95_ms: 61.2,
            min_ms: 38,
            max_ms: 61.2,
            rtt_ms: [45.6, 38, 61.2],
            agent_id: '69f0000000000000000000c3',
          },
        },
      ],
    })
  })

  it('files nothing without a tenant, and names the device org only when it differs', () => {
    expect(buildL4Upload({ rtts: [1], lost: 0, paths: ['hevc'] }, { ...ctx, tenantId: null })).toBeNull()
    const same = buildL4Upload({ rtts: [1], lost: 0, paths: ['hevc'] }, { ...ctx, agentOrgId: ctx.tenantId })
    expect(same!.lines[0].fields.agent_org_id).toBeUndefined()
    const other = buildL4Upload(
      { rtts: [1], lost: 0, paths: ['hevc'] },
      { ...ctx, agentOrgId: '69f0000000000000000000d4', sessionId: 'not-hex' },
    )
    expect(other!.lines[0].fields.agent_org_id).toBe('69f0000000000000000000d4')
    expect(other!.session_id).toBeUndefined()
  })

  it('posts to the browser log route, under its own per-minute cap, and never throws', async () => {
    const calls: string[] = []
    let now = 0
    const up = createL4Uploader({
      fetch: async (url) => {
        calls.push(url)
        return {}
      },
      now: () => now,
      maxPerMinute: 2,
    })
    const b: BenchBatch = { rtts: [40], lost: 0, paths: ['video'] }
    up.send(b, ctx)
    up.send(b, ctx)
    up.send(b, ctx)
    expect(calls).toEqual(['/api/log/browser', '/api/log/browser'])
    now = 60_001
    up.send(b, ctx)
    expect(up.sentCount()).toBe(3)
    const boom = createL4Uploader({
      fetch: () => {
        throw new Error('network')
      },
    })
    expect(() => boom.send(b, ctx)).not.toThrow()
  })
})
