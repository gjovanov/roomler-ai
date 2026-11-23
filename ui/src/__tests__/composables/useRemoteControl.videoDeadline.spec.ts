// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// FR-148 P4b-2 (D55) — the page opens a second video channel, `video-deadline`,
// unordered with no retransmits, beside `video-bytes` whenever chunk framing is
// negotiated, and hands both to the one data-channel worker. `video-bytes` is
// always reliable and ordered: the per-browser unordered opt-in is retired.
//
// Through the real composable: the peer is a fake `RTCPeerConnection` that
// records every channel it is asked for, the decode worker a fake `Worker` that
// records every message, the socket the WS store with its I/O stubbed. Nothing
// here talks to an agent: what is under test is what the page opens and what
// it hands the worker.
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest'
import { readdirSync, readFileSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'
import { ref } from 'vue'
import { createPinia, setActivePinia } from 'pinia'

vi.mock('@/api/client', () => ({ api: { get: vi.fn(async () => ({ ice_servers: [] })) } }))

import { useRemoteControl } from '@/composables/useRemoteControl'
import type { Agent, AgentCapabilities } from '@/stores/agents'
import { useWsStore } from '@/stores/ws'

const AGENT = '69f0000000000000000000c3'
const TENANT = '69f0000000000000000000a1'
const PERMS = 'VIEW | INPUT | CLIPBOARD | FILES | RECORD'
/** The opt-in P4b-2 retires, spelled out once. */
const RETIRED_KEY = 'roomler-rc-unordered-video'

/** A device that sends VP9 over the data channel: the auto rank's pick on
 *  the default dial. `video` is what it advertised; `chunk-framing` is FR-17's. */
function caps(video: string[]): AgentCapabilities {
  return {
    codecs: ['h264', 'vp9'],
    has_input_permission: true,
    supports_clipboard: true,
    supports_file_transfer: true,
    max_simultaneous_sessions: 2,
    hw_encoders: ['openh264-sw', 'libvpx-vp9-444-sw'],
    transports: ['webrtc', 'data-channel-vp9-444'],
    video_cells: [
      { codec: 'h264', backend: 'openh264', chroma: ['yuv420'], hw: false },
      { codec: 'vp9', backend: 'libvpx', chroma: ['yuv420', 'yuv444'], hw: false },
    ],
    video,
  } as unknown as AgentCapabilities
}

class FakeChannel {
  readyState: 'connecting' | 'open' | 'closed' = 'connecting'
  binaryType = 'blob'
  onopen: (() => void) | null = null
  onclose: (() => void) | null = null
  onmessage: ((ev: { data: unknown }) => void) | null = null
  constructor(
    readonly label: string,
    readonly init: RTCDataChannelInit | undefined,
  ) {}
  send() {}
  close() {
    this.readyState = 'closed'
  }
  addEventListener() {}
  removeEventListener() {}
}

class FakePc {
  static last: FakePc | null = null
  readonly made: FakeChannel[] = []
  connectionState = 'new'
  onconnectionstatechange: (() => void) | null = null
  ontrack: unknown = null
  onicecandidate: unknown = null
  constructor(public config: unknown) {
    FakePc.last = this
  }
  createDataChannel(label: string, init?: RTCDataChannelInit) {
    const ch = new FakeChannel(label, init)
    this.made.push(ch)
    return ch
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

/** The decode worker: every message the page posts it, with its transfer list. */
class FakeWorker {
  static last: FakeWorker | null = null
  readonly posts: { msg: Record<string, unknown>; transfer: unknown[] }[] = []
  onmessage: ((ev: MessageEvent) => void) | null = null
  constructor() {
    FakeWorker.last = this
  }
  postMessage(msg: Record<string, unknown>, transfer: unknown[] = []) {
    this.posts.push({ msg, transfer })
  }
  terminate() {}
  of(type: string) {
    return this.posts.filter((p) => p.msg.type === type)
  }
}

class FakeOffscreenCanvas {
  constructor(
    public width: number,
    public height: number,
  ) {}
}

/** Connect to a device that advertised `video`, and return the channels the
 *  page opened, by label, and the worker it started. */
async function connect(video: string[]) {
  setActivePinia(createPinia())
  const ws = useWsStore()
  ws.sendRaw = () => {}
  ws.onRcMessage = () => () => {}
  ws.forceRedial = vi.fn()
  ws.setTenantAffinity = vi.fn()
  ws.getDialedTid = () => TENANT
  ws.status = 'connected'
  globalThis.fetch = vi.fn(async () => ({})) as unknown as typeof fetch
  const agent = ref({ id: AGENT, capabilities: caps(video) } as unknown as Agent)
  const rc = useRemoteControl(agent, { userId: () => null })
  FakePc.last = null
  FakeWorker.last = null
  await rc.connect(AGENT, PERMS, false, TENANT)
  await vi.advanceTimersByTimeAsync(0)
  const pc = FakePc.last as FakePc | null
  const worker = FakeWorker.last as FakeWorker | null
  expect(pc).not.toBeNull()
  expect(worker, 'the data-channel path started').not.toBeNull()
  const channel = (label: string) => pc!.made.find((c) => c.label === label)
  return { rc, pc: pc!, worker: worker!, bytes: channel('video-bytes'), deadline: channel('video-deadline') }
}

beforeAll(() => {
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
  vi.stubGlobal('RTCPeerConnection', FakePc)
  vi.stubGlobal('Worker', FakeWorker)
  vi.stubGlobal('OffscreenCanvas', FakeOffscreenCanvas)
})
afterEach(() => {
  vi.useRealTimers()
  vi.unstubAllGlobals()
  localStorage.clear()
})

describe('FR-148 P4b-2 cell 5 — the channels the page opens', () => {
  it('opens video-deadline, unordered with no retransmits, beside an ordered video-bytes when framing is negotiated', async () => {
    const { bytes, deadline } = await connect(['chunk-framing'])
    expect(bytes?.init).toEqual({ ordered: true })
    expect(deadline?.init).toEqual({ ordered: false, maxRetransmits: 0 })
  })

  it('opens no second channel without framing, and video-bytes as before', async () => {
    const { pc, bytes, deadline } = await connect([])
    expect(bytes?.init).toEqual({ ordered: true })
    expect(deadline).toBeUndefined()
    expect(pc.made.filter((c) => c.label.startsWith('video'))).toHaveLength(1)
  })

  it('keeps video-bytes ordered whatever the retired opt-in says, and tells the worker nothing of it', async () => {
    localStorage.setItem(RETIRED_KEY, '1')
    const { worker, bytes, deadline } = await connect(['chunk-framing'])
    expect(bytes?.init).toEqual({ ordered: true })
    expect(deadline?.init).toEqual({ ordered: false, maxRetransmits: 0 })
    const init = worker.of('init-canvas')[0]!.msg
    expect(init.chunkFraming).toBe(true)
    expect(init).not.toHaveProperty('unorderedVideo')
  })

  it('reads the retired opt-in nowhere in the app', () => {
    // Every source file under ui/src but the tests: no code reads the key, and
    // no comment names it, so FR-135's ratchet counts it gone.
    const src = join(dirname(fileURLToPath(import.meta.url)), '..', '..')
    const naming: string[] = []
    const walk = (dir: string) => {
      for (const e of readdirSync(dir, { withFileTypes: true })) {
        const p = join(dir, e.name)
        if (e.isDirectory()) {
          if (e.name !== '__tests__') walk(p)
        } else if (/\.(ts|vue|js)$/.test(e.name) && readFileSync(p, 'utf8').includes(RETIRED_KEY)) {
          naming.push(p)
        }
      }
    }
    walk(src)
    expect(naming).toEqual([])
  })
})

describe('FR-148 P4b-2 — both channels reach the one worker', () => {
  it('relays a message off video-deadline marked as such, and one off video-bytes exactly as before', async () => {
    const { worker, bytes, deadline } = await connect(['chunk-framing'])
    expect(deadline).toBeDefined()
    expect(bytes!.binaryType).toBe('arraybuffer')
    expect(deadline!.binaryType).toBe('arraybuffer')
    const a = new ArrayBuffer(16)
    const b = new ArrayBuffer(24)
    bytes!.onmessage!({ data: a })
    deadline!.onmessage!({ data: b })
    deadline!.onmessage!({ data: 'a text message is no frame' })
    const chunks = worker.of('chunk')
    expect(chunks).toHaveLength(2)
    // video-bytes: the message an old agent's session has always relayed.
    expect(Object.keys(chunks[0]!.msg).sort()).toEqual(['bytes', 'sentAt', 'type'])
    expect(chunks[0]!.msg.bytes).toBe(a)
    expect(chunks[0]!.transfer).toEqual([a])
    // video-deadline: the same, marked, so the worker reads it as unordered.
    expect(chunks[1]!.msg).toMatchObject({ type: 'chunk', bytes: b, deadline: true })
    expect(typeof chunks[1]!.msg.sentAt).toBe('number')
    expect(chunks[1]!.transfer).toEqual([b])
  })

  it('moves both channels into the worker in one message where the session moves its channel', async () => {
    localStorage.setItem('roomler-rc-dc-worker', '1')
    const { worker, bytes, deadline } = await connect(['chunk-framing'])
    expect(deadline).toBeDefined()
    const moves = worker.of('video-channel')
    expect(moves).toHaveLength(1)
    expect(moves[0]!.msg).toEqual({ type: 'video-channel', channel: bytes, deadline })
    expect(moves[0]!.transfer).toEqual([bytes, deadline])
    // The page keeps no reader on either.
    expect(bytes!.onmessage).toBeNull()
    expect(deadline!.onmessage).toBeNull()
  })

  it('moves video-bytes alone without framing, as before', async () => {
    localStorage.setItem('roomler-rc-dc-worker', '1')
    const { worker, bytes } = await connect([])
    const moves = worker.of('video-channel')
    expect(moves).toHaveLength(1)
    expect(moves[0]!.msg).toEqual({ type: 'video-channel', channel: bytes })
    expect(moves[0]!.transfer).toEqual([bytes])
  })
})
