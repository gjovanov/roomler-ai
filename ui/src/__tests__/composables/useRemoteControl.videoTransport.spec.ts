// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// #4101 — the viewer remembers a person's video transport per browser, under
// `roomler-rc-video-transport.v2`, and every page load reads it back. The read
// accepted a second, hand-kept list of the transports, which had drifted from
// the type: `data-channel-h264` was not on it, so an H.264 pick read back as
// Auto after every reload while VP9, HEVC and AV1 held. It is the class #3651
// removed from the codec choices.
//
// A reload is a new composable on the same localStorage, as in
// `useRemoteControl.chroma420.spec.ts`. Nothing connects here. What a page load
// reads is the per-browser pick: the dropdowns show it before Connect, and a
// device with no override of its own opens with it.
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { ref } from 'vue'
import { createPinia, setActivePinia } from 'pinia'
import {
  CODEC_STORAGE_PREFIX,
  RC_CODEC_CHOICES,
  useRemoteControl,
  type RcVideoTransport,
} from '@/composables/useRemoteControl'

/** The key the pick has been stored under since rc.190.1. A new key would
 *  reset every person's stored pick, so the test names it. */
const KEY = 'roomler-rc-video-transport.v2'

/** Every transport the type names. A record rather than an array, so that
 *  vue-tsc fails when the type gains or loses a transport and this record does
 *  not follow (`satisfies` checks both directions). The round trip below then
 *  cannot skip one. */
const EVERY_TRANSPORT = {
  auto: true,
  webrtc: true,
  'data-channel-vp9-444': true,
  'data-channel-hevc': true,
  'data-channel-av1': true,
  'data-channel-h264': true,
} satisfies Record<RcVideoTransport, true>
const TRANSPORTS = Object.keys(EVERY_TRANSPORT) as RcVideoTransport[]

/** One page load: its own store and composable, on the localStorage every
 *  page of this browser shares. No device is named, so a pick writes no
 *  per-device override: only the per-browser keys. */
function page() {
  setActivePinia(createPinia())
  return useRemoteControl(ref(null), { userId: () => null })
}

function storedKeys(): string[] {
  const keys: string[] = []
  for (let i = 0; i < localStorage.length; i++) keys.push(localStorage.key(i)!)
  return keys
}

beforeEach(() => {
  vi.useFakeTimers()
  localStorage.clear()
})
afterEach(() => {
  vi.useRealTimers()
  vi.restoreAllMocks()
  localStorage.clear()
})

describe('#4101 — the stored video transport holds across a reload', () => {
  it.each(TRANSPORTS)('%s: one page stores it, the next reads it back as itself', (transport) => {
    const first = page()
    first.setVideoTransport(transport)
    expect(localStorage.getItem(KEY)).toBe(transport)

    // `auto` is also what the read falls back to, so that one case cannot tell
    // a read from a fallback. Every other transport can.
    const reloaded = page()
    expect(reloaded.videoTransport.value).toBe(transport)
  })

  // Equality only: no prefix, no trim, no case fold, and no prototype key,
  // which a lookup in an object would accept. `h264` and `hevc` are codec
  // choices, not transports.
  it.each([
    'data-channel-vp8',
    'data-channel',
    'data-channel-h264-444',
    'DATA-CHANNEL-H264',
    ' webrtc',
    'h264',
    'hevc',
    '',
    'toString',
    '__proto__',
  ])('an unknown stored value %j reads as Auto', (raw) => {
    localStorage.setItem(KEY, raw)
    expect(page().videoTransport.value).toBe('auto')
  })

  // A browser can refuse the page its storage: Chrome with the site's data
  // blocked throws on the `localStorage` accessor itself, a storage that fails
  // throws on the call, and Firefox with storage off answers null.
  const refusals: [string, () => void][] = [
    [
      'the accessor throws',
      () => {
        Object.defineProperty(globalThis, 'localStorage', {
          configurable: true,
          get() {
            throw new DOMException('Access is denied for this document.', 'SecurityError')
          },
        })
      },
    ],
    [
      'every read and write throws',
      () => {
        vi.spyOn(Storage.prototype, 'getItem').mockImplementation(() => {
          throw new DOMException('The operation is insecure.', 'SecurityError')
        })
        vi.spyOn(Storage.prototype, 'setItem').mockImplementation(() => {
          throw new DOMException('The quota has been exceeded.', 'QuotaExceededError')
        })
      },
    ],
    [
      'there is none',
      () => {
        Object.defineProperty(globalThis, 'localStorage', { configurable: true, get: () => null })
      },
    ],
  ]

  it.each(refusals)('a refused localStorage (%s) reads as Auto, and a pick still holds in the page', (_, refuse) => {
    localStorage.setItem(KEY, 'data-channel-hevc')
    const real = Object.getOwnPropertyDescriptor(globalThis, 'localStorage')!
    refuse()
    try {
      const p = page()
      expect(p.videoTransport.value).toBe('auto')
      // The write is best effort: it must not throw, and the page keeps the pick.
      p.setVideoTransport('data-channel-h264')
      expect(p.videoTransport.value).toBe('data-channel-h264')
    } finally {
      Object.defineProperty(globalThis, 'localStorage', real)
    }
  })
})

describe('#4101 — every codec choice holds across a reload as the per-browser pick', () => {
  // The field read: pick a codec in the viewer, reload. Before Connect the
  // dropdowns show what the per-browser keys read back as, and a device with
  // no override of its own opens with it. With and without the legacy H.264
  // RTP mapping (`roomler-rc-h264-rtp=1`), which stores H.264 as `webrtc`.
  for (const h264Rtp of [false, true]) {
    const mapping = h264Rtp ? ', with the legacy H.264 RTP mapping' : ''
    it.each(RC_CODEC_CHOICES)(`%s${mapping}: chosen on one page, shown by the next`, (choice) => {
      if (h264Rtp) localStorage.setItem('roomler-rc-h264-rtp', '1')
      page().codecChoice.value = choice
      expect(storedKeys().filter((k) => k.startsWith(CODEC_STORAGE_PREFIX))).toEqual([])
      expect(page().codecChoice.value).toBe(choice)
    })
  }
})
