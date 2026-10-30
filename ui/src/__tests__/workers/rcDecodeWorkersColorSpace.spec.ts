// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * FR-130 P2 — every decode worker tells its decoder the colour space the agent
 * converted with, and never costs the picture for it.
 *
 * Black-box: each real worker module is driven through the messages the
 * composable sends it (`init-canvas`, `chunk`, the RTP transform event), with
 * WebCodecs replaced by a recording fake. What is asserted is the argument of
 * every `configure()` and what the worker tells the main thread.
 */
import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest'
import { RC_DECODER_COLOR_SPACE } from '@/workers/rc-decoder-config'

const SET_S = { ...RC_DECODER_COLOR_SPACE }

type DecoderInit = { output: (frame: unknown) => void; error: (err: unknown) => void }

/** A `VideoDecoder` that records what it is asked. `throwOnColorSpace` makes
 *  configure() throw on a config carrying one, like a browser whose enums
 *  lack a value; `fail()` is a browser refusing after configure() returned
 *  (the error callback, which closes the decoder). */
class FakeDecoder {
  static instances: FakeDecoder[] = []
  static throwOnColorSpace = false
  static isConfigSupported = vi.fn(async (config: VideoDecoderConfig) => ({ supported: true, config }))
  readonly configs: VideoDecoderConfig[] = []
  readonly chunks: { type: string; timestamp: number }[] = []
  state: 'unconfigured' | 'configured' | 'closed' = 'unconfigured'
  decodeQueueSize = 0
  constructor(readonly init: DecoderInit) {
    FakeDecoder.instances.push(this)
  }
  configure(config: VideoDecoderConfig): void {
    if (this.state === 'closed') throw new DOMException('closed', 'InvalidStateError')
    this.configs.push(JSON.parse(JSON.stringify(config)))
    if (FakeDecoder.throwOnColorSpace && config.colorSpace) {
      throw new TypeError("The provided value 'iec61966-2-1' is not a valid enum value of type VideoTransferCharacteristics.")
    }
    this.state = 'configured'
  }
  decode(chunk: { type: string; timestamp: number }): void {
    this.chunks.push(chunk)
  }
  close(): void {
    this.state = 'closed'
  }
  fail(err: unknown): void {
    this.state = 'closed'
    this.init.error(err)
  }
}

class FakeChunk {
  type: string
  timestamp: number
  constructor(init: { type: string; timestamp: number }) {
    this.type = init.type
    this.timestamp = init.timestamp
  }
}

/** A decoded frame, reporting the colour space the decoder gave it. */
function fakeFrame(timestamp: number, colorSpace: VideoColorSpaceInit = SET_S) {
  return {
    timestamp,
    displayWidth: 1920,
    displayHeight: 1080,
    codedWidth: 1920,
    codedHeight: 1080,
    visibleRect: { x: 0, y: 0, width: 1920, height: 1080 },
    colorSpace: { toJSON: () => ({ ...colorSpace }) },
    close: () => {},
  }
}

function fakeCanvas() {
  return { width: 0, height: 0, getContext: () => ({ drawImage: () => {} }) }
}

/** One frame on the data channel, framed as the agent frames it: u32 size,
 *  u8 flags (bit 0: key), u64 timestamp, payload. */
function dcFrame(key: boolean, timestampUs: number): ArrayBuffer {
  const payload = [0, 0, 0, 1, 0x65]
  const buf = new ArrayBuffer(13 + payload.length)
  const view = new DataView(buf)
  view.setUint32(0, payload.length, true)
  view.setUint8(4, key ? 1 : 0)
  view.setUint32(5, timestampUs, true)
  view.setUint32(9, 0, true)
  new Uint8Array(buf, 13).set(payload)
  return buf
}

type Scope = {
  postMessage: (msg: unknown) => void
  onmessage: ((ev: { data: unknown }) => void) | null
  onrtctransform: ((ev: unknown) => Promise<void>) | null
}
const scope = self as unknown as Scope
let originalPost: Scope['postMessage']
let posted: Record<string, unknown>[] = []
const ofType = (t: string) => posted.filter((m) => m.type === t)

beforeEach(() => {
  vi.resetModules()
  FakeDecoder.instances = []
  FakeDecoder.throwOnColorSpace = false
  FakeDecoder.isConfigSupported.mockClear()
  vi.stubGlobal('VideoDecoder', FakeDecoder)
  vi.stubGlobal('EncodedVideoChunk', FakeChunk)
  // The workers debounce their key-frame requests on performance.now(); a
  // clock that moves keeps every request visible.
  let t = 100_000
  vi.spyOn(performance, 'now').mockImplementation(() => (t += 1_000))
  posted = []
  originalPost = scope.postMessage
  scope.postMessage = (msg) => {
    posted.push(msg as Record<string, unknown>)
  }
})

afterEach(() => {
  scope.postMessage = originalPost
  scope.onmessage = null
  scope.onrtctransform = null
  vi.unstubAllGlobals()
  vi.restoreAllMocks()
})

const DC_WORKERS = [
  {
    name: 'rc-vp9-444-worker (VP9, AV1, H.264 over the data channel)',
    load: () => import('@/workers/rc-vp9-444-worker'),
    codec: 'vp09.01.10.08',
  },
  {
    name: 'rc-hevc-worker (HEVC over the data channel)',
    load: () => import('@/workers/rc-hevc-worker'),
    codec: 'hev1.1.6.L153.B0',
  },
] as const

describe.each(DC_WORKERS)('FR-130 P2 — $name', ({ load, codec }) => {
  /** The worker's config as it was before FR-130, field for field. */
  const base = { codec, optimizeForLatency: true, hardwareAcceleration: 'no-preference' }

  async function start(init: Record<string, unknown>) {
    await load()
    const onmessage = scope.onmessage!
    onmessage({ data: { type: 'init-canvas', canvas: fakeCanvas(), ...init } })
    return (msg: Record<string, unknown>) => onmessage({ data: msg })
  }

  it('tells the decoder set S when the session asks, and keeps every other field', async () => {
    await start({ colorSpace: true })
    expect(FakeDecoder.instances).toHaveLength(1)
    expect(FakeDecoder.instances[0]!.configs).toEqual([{ ...base, colorSpace: SET_S }])
    expect(ofType('decoder-configured')).toEqual([
      { type: 'decoder-configured', codec, pref: 'no-preference', colorSpace: SET_S },
    ])
  })

  it('configures exactly as before FR-130 when the session says no, or nothing', async () => {
    const send = await start({ colorSpace: false, decodePref: 'prefer-software' })
    expect(FakeDecoder.instances[0]!.configs).toEqual([
      { codec, optimizeForLatency: true, hardwareAcceleration: 'prefer-software' },
    ])
    expect(ofType('decoder-configured')[0]!.colorSpace).toBeNull()
    // A failure before the first frame is reported at once: the colour space
    // was not in play, so there is nothing to retry.
    send({ type: 'chunk', bytes: dcFrame(true, 1) })
    FakeDecoder.instances[0]!.fail(new DOMException('Unsupported configuration', 'NotSupportedError'))
    expect(ofType('decoder-error')).toHaveLength(1)
    expect(FakeDecoder.instances).toHaveLength(1)
    expect(ofType('colorspace-dropped')).toEqual([])
  })

  it('treats an absent flag as no', async () => {
    await start({})
    expect('colorSpace' in FakeDecoder.instances[0]!.configs[0]!).toBe(false)
  })

  it('retries a configure() that threw on the colour space without it', async () => {
    FakeDecoder.throwOnColorSpace = true
    await start({ colorSpace: true })
    expect(FakeDecoder.instances).toHaveLength(1)
    expect(FakeDecoder.instances[0]!.configs).toEqual([{ ...base, colorSpace: SET_S }, base])
    expect(ofType('colorspace-dropped')).toHaveLength(1)
    expect(ofType('colorspace-dropped')[0]).toMatchObject({ stage: 'configure', codec })
    expect(ofType('decoder-configure-error')).toEqual([])
    expect(ofType('decoder-configured')[0]!.colorSpace).toBeNull()
  })

  it('rebuilds once without it when the decoder fails before its first frame, and gates on a key frame', async () => {
    const send = await start({ colorSpace: true })
    send({ type: 'chunk', bytes: dcFrame(true, 1) })
    expect(FakeDecoder.instances[0]!.chunks).toHaveLength(1)
    FakeDecoder.instances[0]!.fail(new DOMException('Unsupported configuration', 'NotSupportedError'))

    // No error reaches the main thread (it would ban the transport); a new
    // decoder takes the same config without the colour space instead.
    expect(ofType('decoder-error')).toEqual([])
    expect(FakeDecoder.instances).toHaveLength(2)
    expect(FakeDecoder.instances[1]!.configs).toEqual([base])
    expect(ofType('colorspace-dropped')).toHaveLength(1)
    expect(ofType('colorspace-dropped')[0]).toMatchObject({ stage: 'decode', codec })
    expect(ofType('request-keyframe').length).toBeGreaterThan(0)

    // The new decoder takes a key frame first.
    send({ type: 'chunk', bytes: dcFrame(false, 2) })
    expect(FakeDecoder.instances[1]!.chunks).toEqual([])
    send({ type: 'chunk', bytes: dcFrame(true, 3) })
    expect(FakeDecoder.instances[1]!.chunks.map((c) => c.type)).toEqual(['key'])

    // One retry a session: the rebuilt decoder's own failure is reported.
    FakeDecoder.instances[1]!.fail(new DOMException('Unsupported configuration', 'NotSupportedError'))
    expect(ofType('decoder-error')).toHaveLength(1)
    expect(FakeDecoder.instances).toHaveLength(2)
  })

  it("asks for the key frame at once after the rebuild, even in the worker's first 250 ms", async () => {
    // The resync debounce's clock starts when the worker is created, and the
    // rebuild's one request must not wait on it: a real-Chrome smoke of this
    // path stalled until it went past the debounce.
    vi.mocked(performance.now).mockReturnValue(10)
    const send = await start({ colorSpace: true })
    send({ type: 'chunk', bytes: dcFrame(true, 1) })
    FakeDecoder.instances[0]!.fail(new DOMException('Unsupported configuration', 'NotSupportedError'))
    expect(ofType('request-keyframe')).toHaveLength(1)
  })

  it('reports an error after the first frame as before, and the first frame carries its colour space', async () => {
    const send = await start({ colorSpace: true })
    send({ type: 'chunk', bytes: dcFrame(true, 1) })
    FakeDecoder.instances[0]!.init.output(fakeFrame(1))
    expect(ofType('first-frame')).toHaveLength(1)
    expect(ofType('first-frame')[0]).toMatchObject({ width: 1920, height: 1080, colorSpace: SET_S })
    FakeDecoder.instances[0]!.fail(new Error('driver hiccup'))
    expect(ofType('decoder-error')).toHaveLength(1)
    expect(FakeDecoder.instances).toHaveLength(1)
  })

  it('keeps the flag across the idempotent visible-canvas re-init', async () => {
    const send = await start({ colorSpace: true })
    send({ type: 'init-canvas', canvas: fakeCanvas() })
    // Still one decoder, still told set S.
    expect(FakeDecoder.instances).toHaveLength(1)
    expect(FakeDecoder.instances[0]!.configs[0]!.colorSpace).toEqual(SET_S)
  })
})

describe('FR-130 P2 — rc-webcodecs-worker (the RTP transform)', () => {
  const codecString = 'avc1.42E01F'

  /** Start the transform the composable installs, with a readable the test
   *  feeds and a writable that collects what goes on to the default path. */
  async function start(options: Record<string, unknown>) {
    await import('@/workers/rc-webcodecs-worker')
    let ctl!: ReadableStreamDefaultController<unknown>
    const readable = new ReadableStream<unknown>({
      start(c) {
        ctl = c
      },
    })
    const passedOn: unknown[] = []
    const writable = new WritableStream<unknown>({
      write(v) {
        passedOn.push(v)
      },
    })
    await scope.onrtctransform!({ transformer: { readable, writable, options } })
    const feed = async (type: 'key' | 'delta', timestamp: number) => {
      ctl.enqueue({ type, timestamp, data: new ArrayBuffer(8) })
      for (let i = 0; i < 5; i++) await new Promise((r) => setTimeout(r, 0))
    }
    return { feed, passedOn }
  }

  it('probes codec-only, then configures with set S', async () => {
    const { feed } = await start({ codec: 'h264', colorSpace: true })
    for (const [cfg] of FakeDecoder.isConfigSupported.mock.calls) {
      expect('colorSpace' in cfg).toBe(false)
    }
    expect(FakeDecoder.instances[0]!.configs).toEqual([
      { codec: codecString, optimizeForLatency: true, colorSpace: SET_S },
    ])
    expect(ofType('transform-active')[0]).toMatchObject({ configured: true, colorSpace: SET_S })
    await feed('key', 1)
    expect(FakeDecoder.instances[0]!.chunks).toHaveLength(1)
  })

  it('configures as before FR-130 without the flag', async () => {
    const { feed } = await start({ codec: 'h264' })
    expect(FakeDecoder.instances[0]!.configs).toEqual([{ codec: codecString, optimizeForLatency: true }])
    expect(ofType('transform-active')[0]!.colorSpace).toBeNull()
    await feed('key', 1)
  })

  it('retries a configure() that threw on the colour space without it', async () => {
    FakeDecoder.throwOnColorSpace = true
    const { feed } = await start({ codec: 'h264', colorSpace: true })
    expect(FakeDecoder.instances[0]!.configs).toEqual([
      { codec: codecString, optimizeForLatency: true, colorSpace: SET_S },
      { codec: codecString, optimizeForLatency: true },
    ])
    expect(ofType('colorspace-dropped')[0]).toMatchObject({ stage: 'configure', codec: codecString })
    expect(ofType('decoder-configure-error')).toEqual([])
    expect(ofType('transform-active')[0]).toMatchObject({ configured: true, colorSpace: null })
    await feed('key', 1)
    expect(FakeDecoder.instances[0]!.chunks).toHaveLength(1)
  })

  it('rebuilds once without it when the decoder fails before its first frame, and waits for a key frame', async () => {
    const { feed, passedOn } = await start({ codec: 'h264', colorSpace: true })
    await feed('key', 1)
    FakeDecoder.instances[0]!.fail(new DOMException('Unsupported configuration', 'NotSupportedError'))

    // No error reaches the main thread (it would drop to <video>).
    expect(ofType('decoder-error')).toEqual([])
    expect(FakeDecoder.instances).toHaveLength(2)
    expect(FakeDecoder.instances[1]!.configs).toEqual([{ codec: codecString, optimizeForLatency: true }])
    expect(ofType('colorspace-dropped')[0]).toMatchObject({ stage: 'decode' })
    expect(ofType('request-keyframe').length).toBeGreaterThan(0)

    // A delta still reaches the default path, but not the new decoder.
    await feed('delta', 2)
    expect(FakeDecoder.instances[1]!.chunks).toEqual([])
    expect(passedOn).toHaveLength(2)
    await feed('key', 3)
    expect(FakeDecoder.instances[1]!.chunks.map((c) => c.type)).toEqual(['key'])

    // The first frame says what the decoder produced.
    FakeDecoder.instances[1]!.init.output(fakeFrame(3, { matrix: 'bt709', primaries: 'bt709', transfer: 'bt709', fullRange: false }))
    expect(ofType('first-frame')[0]).toMatchObject({
      colorSpace: { matrix: 'bt709', primaries: 'bt709', transfer: 'bt709', fullRange: false },
    })

    // One retry a session.
    FakeDecoder.instances[1]!.fail(new Error('again'))
    expect(ofType('decoder-error')).toHaveLength(1)
    expect(FakeDecoder.instances).toHaveLength(2)
  })

  it("asks for the key frame at once after the rebuild, even in the worker's first 250 ms", async () => {
    vi.mocked(performance.now).mockReturnValue(10)
    const { feed } = await start({ codec: 'h264', colorSpace: true })
    await feed('key', 1)
    FakeDecoder.instances[0]!.fail(new DOMException('Unsupported configuration', 'NotSupportedError'))
    expect(ofType('request-keyframe')).toHaveLength(1)
  })
})
