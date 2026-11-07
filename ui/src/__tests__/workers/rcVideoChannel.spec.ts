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
