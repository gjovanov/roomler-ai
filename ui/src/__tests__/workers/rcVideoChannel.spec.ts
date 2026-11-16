// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * FR-135 R3 — moving the `video-bytes` channel into the data-channel worker:
 * the page's half (`moveVideoChannelToWorker`, the fallback when a browser
 * cannot transfer one) and the delivery-gap clock both sides keep.
 */
import { describe, it, expect } from 'vitest'
import {
  VIDEO_DC_GAP_MS,
  createChannelMoveWatch,
  createDeliveryGapClock,
  moveVideoChannelToWorker,
} from '@/workers/rc-video-channel'

const channel = { label: 'video-bytes', readyState: 'connecting' } as unknown as RTCDataChannel

describe('FR-135 R3 — moveVideoChannelToWorker', () => {
  it('hands the channel over in the transfer list when the browser takes it', () => {
    const posts: [unknown, unknown[]][] = []
    const worker = { postMessage: (m: unknown, t: unknown[]) => void posts.push([m, t]) }
    expect(moveVideoChannelToWorker(worker, channel)).toBeNull()
    expect(posts).toEqual([[{ type: 'video-channel', channel }, [channel]]])
  })

  it('leaves the channel on the page, and says why, where the browser cannot transfer one', () => {
    // A browser without transferable channels refuses in postMessage, before
    // anything is detached (HTML's StructuredSerializeWithTransfer).
    const worker = {
      postMessage: () => {
        throw new DOMException('RTCDataChannel object could not be cloned.', 'DataCloneError')
      },
    }
    expect(moveVideoChannelToWorker(worker, channel)).toBe(
      'DataCloneError: RTCDataChannel object could not be cloned.',
    )
  })

  it('says why for a refusal that is no Error too', () => {
    const worker = {
      postMessage: () => {
        throw 'not transferable'
      },
    }
    expect(moveVideoChannelToWorker(worker, channel)).toBe('not transferable')
  })
})

describe('FR-135 R3 — the delivery-gap clock', () => {
  it('reports a silence only past the bar, and none before the first message', () => {
    const c = createDeliveryGapClock()
    expect(c.note(10_000)).toBeNull() // a fresh path: the setup's silence is no gap
    expect(c.note(10_000 + VIDEO_DC_GAP_MS)).toBeNull() // exactly the bar
    expect(c.note(10_000 + VIDEO_DC_GAP_MS + 40)).toBeNull()
    expect(c.note(10_000 + 2 * VIDEO_DC_GAP_MS + 41)).toBe(VIDEO_DC_GAP_MS + 1)
  })

  it('starts over at reset', () => {
    const c = createDeliveryGapClock()
    c.note(1_000)
    c.reset()
    expect(c.note(9_000)).toBeNull()
    expect(VIDEO_DC_GAP_MS).toBe(1500)
  })
})

describe('FR-135 R3 — the wait for a moved channel to arrive', () => {
  /** A watch on a timer the test fires by hand. */
  function watch() {
    const failures: string[] = []
    const timers: { fn: () => void; ms: number; cleared: boolean }[] = []
    const w = createChannelMoveWatch({
      graceMs: 2000,
      onFailed: (why) => failures.push(why),
      setTimer: (fn, ms) => {
        const t = { fn, ms, cleared: false }
        timers.push(t)
        return t
      },
      clearTimer: (t) => {
        ;(t as { cleared: boolean }).cleared = true
      },
    })
    const fire = () => timers.filter((t) => !t.cleared).forEach((t) => t.fn())
    return { w, failures, timers, fire }
  }

  it('is satisfied when the worker says it has the channel', () => {
    const { w, failures, timers, fire } = watch()
    w.moved()
    expect(w.onWorkerMessage('decoder-configured')).toBe(false)
    expect(timers).toHaveLength(1)
    expect(timers[0]!.ms).toBe(2000)
    expect(w.onWorkerMessage('video-channel-attached')).toBe(true)
    fire()
    expect(failures).toEqual([])
    expect(w.pending).toBe(false)
  })

  it('fails once when the word does not come within the grace of the worker’s first answer', () => {
    const { w, failures, fire } = watch()
    w.moved()
    w.onWorkerMessage('decoder-configure-error')
    fire()
    fire()
    expect(failures).toEqual(['the worker never said it had it'])
  })

  it('does not start the wait before the worker answers: a slow module load is no failure', () => {
    const { w, failures, timers } = watch()
    w.moved()
    w.onWorkerMessage('stats')
    expect(timers).toEqual([])
    expect(failures).toEqual([])
  })

  it('fails at once when the worker could not receive the channel', () => {
    const { w, failures } = watch()
    w.moved()
    w.onWorkerMessage('decoder-configured')
    expect(w.onWorkerMessage('message-error')).toBe(true)
    expect(failures).toEqual(['the worker could not receive it (messageerror)'])
  })

  it('leaves a relayed channel alone, and says nothing once the path stopped', () => {
    const relay = watch()
    expect(relay.w.onWorkerMessage('message-error'), 'not its message on the relay').toBe(false)
    relay.w.onWorkerMessage('decoder-configured')
    expect(relay.timers).toEqual([])
    const stopped = watch()
    stopped.w.moved()
    stopped.w.onWorkerMessage('decoder-configured')
    stopped.w.stop()
    stopped.fire()
    expect(stopped.timers[0]!.cleared).toBe(true)
    expect(stopped.failures).toEqual([])
  })
})
