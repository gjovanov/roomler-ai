// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// #3651 — "Standard (4:2:0)" holds for HEVC and H.264, end to end through the
// real composable. The two dropdowns are driven exactly as RemoteControl.vue
// binds them (an axis is written through `choiceFromPicker`, the other read
// back through `pickerFromChoice`), a reload is a new page on the same
// localStorage, and what counts at connect is the `rc:session.request` itself:
// on the Priority dial's Sharper (the default), against a device and a browser
// that can both do HEVC and H.264 in 4:4:4, so that a chroma the choice lost
// would show as `chroma_pref: 'yuv444'`.
//
// The peer is a fake `RTCPeerConnection`, the socket the WS store with its I/O
// stubbed, and the browser's decoders a WebCodecs + MediaCapabilities stub that
// accepts every codec in hardware. Everything between them is the composable
// as it ships.
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest'
import { ref } from 'vue'
import { createPinia, setActivePinia } from 'pinia'

vi.mock('@/api/client', () => ({ api: { get: vi.fn(async () => ({ ice_servers: [] })) } }))

import { CODEC_STORAGE_PREFIX, useRemoteControl, type RcPriority } from '@/composables/useRemoteControl'
import {
  choiceFromPicker,
  pickerFromChoice,
  type PickerChroma,
  type PickerCodec,
} from '@/composables/videoCells'
import type { Agent, AgentCapabilities } from '@/stores/agents'
import { useWsStore } from '@/stores/ws'

const AGENT = '69f0000000000000000000c3'
const OTHER = '69f0000000000000000000c4'
const TENANT = '69f0000000000000000000a1'
const PERMS = 'VIEW | INPUT | CLIPBOARD | FILES | RECORD'

const CAPS_BASE = {
  codecs: ['h264', 'h265'],
  has_input_permission: true,
  supports_clipboard: true,
  supports_file_transfer: true,
  max_simultaneous_sessions: 2,
}

/** The device of #3651's field read: a probe that opened `hevc_nvenc` and
 *  `h264_nvenc` in both chroma formats, both data-channel transports offered. */
const NVENC_BOTH = {
  ...CAPS_BASE,
  hw_encoders: ['ffmpeg-hevc_nvenc', 'ffmpeg-h264_nvenc'],
  transports: ['webrtc', 'data-channel-hevc', 'data-channel-h264'],
  video_cells: [
    { codec: 'hevc', backend: 'nvenc', chroma: ['yuv420', 'yuv444'], hw: true },
    { codec: 'h264', backend: 'nvenc', chroma: ['yuv420', 'yuv444'], hw: true },
  ],
} as unknown as AgentCapabilities

/** A device with no HEVC at all (no HEVC encoder, no HEVC transport) and no
 *  hardware encoder: OpenH264 on the RTP track and VP9 in libvpx, whose
 *  profile 1 is the auto rank's one 4:4:4 cell here (Sharper takes it). */
const NO_HEVC = {
  ...CAPS_BASE,
  codecs: ['h264', 'vp9'],
  hw_encoders: ['openh264-sw', 'libvpx-vp9-444-sw'],
  transports: ['webrtc', 'data-channel-vp9-444'],
  video_cells: [
    { codec: 'h264', backend: 'openh264', chroma: ['yuv420'], hw: false },
    { codec: 'vp9', backend: 'libvpx', chroma: ['yuv420', 'yuv444'], hw: false },
  ],
} as unknown as AgentCapabilities

class FakeChannel {
  readyState: 'connecting' | 'open' | 'closed' = 'connecting'
  binaryType = 'blob'
  onopen: (() => void) | null = null
  onclose: (() => void) | null = null
  onmessage: ((ev: { data: unknown }) => void) | null = null
  constructor(readonly label: string) {}
  send() {}
  close() {
    this.readyState = 'closed'
  }
  addEventListener() {}
  removeEventListener() {}
}

class FakePc {
  connectionState = 'new'
  onconnectionstatechange: (() => void) | null = null
  ontrack: unknown = null
  onicecandidate: unknown = null
  constructor(public config: unknown) {}
  createDataChannel(label: string) {
    return new FakeChannel(label)
  }
  addTransceiver() {
    return {}
  }
  getConfiguration() {
    return this.config
  }
  setConfiguration(c: unknown) {
    this.config = c
  }
  async getStats() {
    return new Map()
  }
  async createOffer() {
    return { type: 'offer', sdp: 'v=0\r\n' }
  }
  async setLocalDescription() {}
  async setRemoteDescription() {}
  async addIceCandidate() {}
  close() {
    this.connectionState = 'closed'
  }
  addEventListener() {}
  removeEventListener() {}
  getReceivers() {
    return []
  }
  getTransceivers() {
    return []
  }
  getSenders() {
    return []
  }
}

type Msg = Record<string, any>

/** One page load for one device: its own store and composable, on the
 *  localStorage every page of this browser shares. */
function page(agentId = AGENT, caps: AgentCapabilities = NVENC_BOTH) {
  setActivePinia(createPinia())
  const ws = useWsStore()
  const sent: Msg[] = []
  ws.sendRaw = (m: unknown) => {
    sent.push(m as Msg)
  }
  ws.onRcMessage = () => () => {}
  ws.forceRedial = vi.fn()
  ws.setTenantAffinity = vi.fn()
  ws.getDialedTid = () => TENANT
  ws.status = 'connected'
  globalThis.fetch = vi.fn(async () => ({})) as unknown as typeof fetch
  const agent = ref({ id: agentId, capabilities: caps } as unknown as Agent)
  const rc = useRemoteControl(agent, { userId: () => null })
  return {
    rc,
    /** The two dropdowns, bound as RemoteControl.vue binds `pickerCodec` and
     *  `pickerChroma`: the codec first, then the colour detail. */
    choose(codec: PickerCodec, chroma: PickerChroma) {
      rc.codecChoice.value = choiceFromPicker(codec, pickerFromChoice(rc.codecChoice.value).chroma)
      rc.codecChoice.value = choiceFromPicker(pickerFromChoice(rc.codecChoice.value).codec, chroma)
    },
    /** What the two dropdowns show. */
    shown: () => pickerFromChoice(rc.codecChoice.value),
    dial(p: RcPriority) {
      rc.setPriority(p)
    },
    /** Connect, and return the one `rc:session.request` it sent. */
    async request(): Promise<Msg> {
      await rc.connect(agentId, PERMS, false, TENANT)
      await vi.advanceTimersByTimeAsync(0)
      const all = sent.filter((m) => m.t === 'rc:session.request')
      expect(all).toHaveLength(1)
      return all[0]!
    },
  }
}

beforeAll(() => {
  // A Chrome with hardware HEVC (Main and Rext) and H.264 (High and High
  // 4:4:4) decode: every config is supported, smooth and power-efficient.
  ;(globalThis as unknown as { VideoDecoder: unknown }).VideoDecoder = {
    isConfigSupported: async (config: { codec: string }) => ({ supported: true, config }),
  }
  Object.defineProperty(navigator, 'mediaCapabilities', {
    configurable: true,
    value: { decodingInfo: async () => ({ supported: true, smooth: true, powerEfficient: true }) },
  })
})
beforeEach(() => {
  vi.useFakeTimers()
  vi.setSystemTime(1_800_000_000_000)
  localStorage.clear()
  // No loopback probe: the page would fetch 127.0.0.1 for a relay.
  localStorage.setItem('roomler-rc-local-relay', '0')
  ;(globalThis as unknown as { RTCPeerConnection: unknown }).RTCPeerConnection = FakePc
})
afterEach(() => {
  vi.useRealTimers()
  localStorage.clear()
})

const PAIRS = [
  { codec: 'hevc', transport: 'data-channel-hevc' },
  { codec: 'h264', transport: 'data-channel-h264' },
] as const

describe('#3651 AC1 — "Standard (4:2:0)" stays chosen: after the choice, after a reload, per device', () => {
  for (const { codec } of PAIRS) {
    it(`${codec}: the dropdown keeps it, connect stores it for the device, and a reload restores it there`, async () => {
      const first = page()
      first.dial('sharper')
      first.choose(codec, 'yuv420')
      expect(first.shown()).toEqual({ codec, chroma: 'yuv420' })
      await first.request()
      expect(first.shown()).toEqual({ codec, chroma: 'yuv420' })

      // A reload: a new page on the same storage; connect restores the
      // device's own override.
      const reloaded = page()
      await reloaded.request()
      expect(reloaded.shown()).toEqual({ codec, chroma: 'yuv420' })
    })

    it(`${codec}: per device — another device's own override is its own, and does not move this one`, async () => {
      const other = page(OTHER)
      other.choose('vp9', 'yuv444')
      await other.request()

      const here = page()
      here.choose(codec, 'yuv420')
      await here.request()

      const otherAgain = page(OTHER)
      await otherAgain.request()
      expect(otherAgain.shown()).toEqual({ codec: 'vp9', chroma: 'yuv444' })
      const hereAgain = page()
      await hereAgain.request()
      expect(hereAgain.shown()).toEqual({ codec, chroma: 'yuv420' })
    })
  }

  it('hevc: the per-browser default holds it too — a device with no override of its own opens on HEVC + Standard (4:2:0)', async () => {
    const first = page()
    first.choose('hevc', 'yuv420')

    const elsewhere = page(OTHER)
    expect(elsewhere.shown()).toEqual({ codec: 'hevc', chroma: 'yuv420' })
    elsewhere.dial('sharper')
    const req = await elsewhere.request()
    expect(elsewhere.shown()).toEqual({ codec: 'hevc', chroma: 'yuv420' })
    expect(req.preferred_transport).toBe('data-channel-hevc')
    expect(req).not.toHaveProperty('chroma_pref')
  })

  it('h264: the per-browser default keeps its 4:2:0; its codec reads back as Auto (class row 3, left for a decision)', async () => {
    // `readStoredVideoTransport` has no `data-channel-h264` in its allow-list,
    // so every H.264 per-browser default (plain, 4:4:4, and this one) reads
    // back as the transport `auto` on a device with no override of its own.
    // The chroma is a key of its own and survives: such a device runs the
    // auto rank pinned to 4:2:0, never 4:4:4.
    const first = page()
    first.choose('h264', 'yuv420')

    const elsewhere = page(OTHER)
    expect(elsewhere.shown()).toEqual({ codec: 'auto', chroma: 'yuv420' })
    elsewhere.dial('sharper')
    const req = await elsewhere.request()
    expect(req).not.toHaveProperty('chroma_pref')
  })
})

describe('#3651 AC2 — on Sharper, a pair that can do 4:4:4 asks for 4:2:0 when 4:2:0 was chosen', () => {
  for (const { codec, transport } of PAIRS) {
    it(`${codec}: the pair does take 4:4:4 on Sharper when the colour detail is Auto`, async () => {
      // The control: without it, a 4:2:0 request below could mean this rig
      // never reaches 4:4:4 at all.
      const p = page()
      p.dial('sharper')
      p.choose(codec, 'auto')
      const req = await p.request()
      expect(req.preferred_transport).toBe(transport)
      expect(req.chroma_pref).toBe('yuv444')
    })

    it(`${codec} + Standard (4:2:0) on Sharper: the request is the 4:2:0 one, equal to the one the codec sends on Balanced today`, async () => {
      const p = page()
      p.dial('sharper')
      p.choose(codec, 'yuv420')
      const req = await p.request()
      expect(req.preferred_transport).toBe(transport)
      expect(req).not.toHaveProperty('chroma_pref')

      // Nothing new on the wire: what the plain codec has always sent off
      // Sharper (the dial is not in the request).
      const today = page()
      today.dial('balanced')
      today.choose(codec, 'auto')
      expect(req).toEqual(await today.request())
    })

    it(`${codec} + Standard (4:2:0) restored from the device's override after a reload, on Sharper: still the 4:2:0 request`, async () => {
      const first = page()
      first.choose(codec, 'yuv420')
      await first.request()

      const reloaded = page()
      reloaded.dial('sharper')
      const req = await reloaded.request()
      expect(req.preferred_transport).toBe(transport)
      expect(req).not.toHaveProperty('chroma_pref')
    })
  }

  it('hevc + Standard (4:2:0) on a device without HEVC: the auto rank it falls back to stays at 4:2:0', async () => {
    // #3167 runs the auto rank for a codec the device does not offer, and the
    // explicit chroma rides into that rank. The control: the plain HEVC
    // choice (chroma Auto) takes VP9 4:4:4 on Sharper here.
    const plain = page(AGENT, NO_HEVC)
    plain.dial('sharper')
    plain.choose('hevc', 'auto')
    const plainReq = await plain.request()
    expect(plainReq.preferred_transport).toBe('data-channel-vp9-444')
    expect(plainReq.chroma_pref).toBe('yuv444')

    const p = page(AGENT, NO_HEVC)
    p.dial('sharper')
    p.choose('hevc', 'yuv420')
    const req = await p.request()
    expect(req.preferred_transport).toBe('data-channel-vp9-444')
    expect(req.chroma_pref).toBe('yuv420')

    // Nothing new on the wire: the request codec Auto + Standard (4:2:0)
    // already sends on this device.
    const today = page(AGENT, NO_HEVC)
    today.dial('sharper')
    today.choose('auto', 'yuv420')
    expect(req).toEqual(await today.request())
  })
})

describe('#3651 AC3 — a value stored before the change still reads as Auto (follows Priority)', () => {
  for (const { codec, transport } of PAIRS) {
    it(`a per-device '${codec}' an older page stored: shown as Auto, and it follows the dial`, async () => {
      localStorage.setItem(CODEC_STORAGE_PREFIX + AGENT, codec)
      const sharper = page()
      sharper.dial('sharper')
      const req = await sharper.request()
      expect(sharper.shown()).toEqual({ codec, chroma: 'auto' })
      expect(req.preferred_transport).toBe(transport)
      expect(req.chroma_pref).toBe('yuv444')

      const balanced = page()
      balanced.dial('balanced')
      const req2 = await balanced.request()
      expect(balanced.shown()).toEqual({ codec, chroma: 'auto' })
      expect(req2).not.toHaveProperty('chroma_pref')
    })
  }

  it('a per-browser default an older page stored for HEVC (chroma auto) reads as HEVC + Auto', () => {
    localStorage.setItem('roomler-rc-video-transport.v2', 'data-channel-hevc')
    localStorage.setItem('roomler-rc-vp9-chroma', 'auto')
    expect(page(OTHER).shown()).toEqual({ codec: 'hevc', chroma: 'auto' })
  })
})

describe('#3651 the legacy H.264 RTP mapping (roomler-rc-h264-rtp=1)', () => {
  it('H.264 + Standard (4:2:0) holds and takes the RTP track, 4:2:0 only: the request plain H.264 sends with the flag', async () => {
    localStorage.setItem('roomler-rc-h264-rtp', '1')
    const p = page()
    p.dial('sharper')
    p.choose('h264', 'yuv420')
    expect(p.shown()).toEqual({ codec: 'h264', chroma: 'yuv420' })
    const req = await p.request()
    expect(req).not.toHaveProperty('preferred_transport')
    expect(req).not.toHaveProperty('chroma_pref')

    const today = page()
    today.dial('sharper')
    today.choose('h264', 'auto')
    expect(req).toEqual(await today.request())

    const reloaded = page()
    await reloaded.request()
    expect(reloaded.shown()).toEqual({ codec: 'h264', chroma: 'yuv420' })
  })
})
