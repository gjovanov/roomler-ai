// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
import { describe, it, expect, vi } from 'vitest'
import {
  RC_DECODER_COLOR_SPACE,
  colorSpaceLabel,
  createDecoderColorSpace,
  frameColorSpace,
  rcDecoderConfig,
  type ConfigurableDecoder,
} from '@/workers/rc-decoder-config'

/** FR-130 set S, spelled out once more on purpose: a change to the shared
 *  constant has to change this test too. */
const SET_S = { matrix: 'smpte170m', primaries: 'bt709', transfer: 'iec61966-2-1', fullRange: false }

const BASE: VideoDecoderConfig = {
  codec: 'vp09.01.10.08',
  optimizeForLatency: true,
  hardwareAcceleration: 'no-preference',
}

/** A decoder that records each configure() and can be told to throw on the
 *  colour space, the way a browser whose enums lack a value does. */
function decoder(opts: { throwOnColorSpace?: boolean; throwAlways?: boolean } = {}) {
  const configs: VideoDecoderConfig[] = []
  const d: ConfigurableDecoder & { configs: VideoDecoderConfig[] } = {
    configs,
    configure(config) {
      configs.push(config)
      if (opts.throwAlways) throw new Error('unsupported codec')
      if (opts.throwOnColorSpace && config.colorSpace) {
        throw new TypeError("The provided value 'iec61966-2-1' is not a valid enum value")
      }
    },
  }
  return d
}

const build = (colorSpace: boolean) => rcDecoderConfig(BASE, colorSpace)

describe('RC_DECODER_COLOR_SPACE (FR-130 set S)', () => {
  it('is BT.601 limited with the sRGB primaries and transfer, and cannot be changed', () => {
    expect(RC_DECODER_COLOR_SPACE).toEqual(SET_S)
    expect(Object.isFrozen(RC_DECODER_COLOR_SPACE)).toBe(true)
  })
})

describe('rcDecoderConfig', () => {
  it('adds set S to the worker config and keeps every other field', () => {
    expect(rcDecoderConfig(BASE, true)).toEqual({ ...BASE, colorSpace: SET_S })
  })

  it('carries no colorSpace member at all when off, so the bitstream decides', () => {
    const off = rcDecoderConfig(BASE, false)
    expect(off).toEqual(BASE)
    expect('colorSpace' in off).toBe(false)
    // Off strips one the base already had, too.
    const stale = rcDecoderConfig({ ...BASE, colorSpace: { matrix: 'bt709' } }, false)
    expect('colorSpace' in stale).toBe(false)
  })

  it('never shares the constant or mutates the base', () => {
    const base = { ...BASE }
    const on = rcDecoderConfig(base, true)
    expect(on.colorSpace).not.toBe(RC_DECODER_COLOR_SPACE)
    expect(base).toEqual(BASE)
    on.colorSpace!.matrix = 'bt709'
    expect(RC_DECODER_COLOR_SPACE.matrix).toBe('smpte170m')
  })
})

describe('createDecoderColorSpace — the session flag and its fallback', () => {
  it('configures without the colour space until the session asks for it', () => {
    const report = vi.fn()
    const cs = createDecoderColorSpace(report)
    const d = decoder()
    expect(cs.configure(d, build)).toEqual(BASE)
    expect(cs.applied).toBe(false)
    // Nothing to give up: an error is reported as before.
    expect(cs.retryAfterError(new Error('x'), 0)).toBe(false)
    expect(report).not.toHaveBeenCalled()
  })

  it('configures with set S once wanted, and the flag sticks', () => {
    const cs = createDecoderColorSpace(vi.fn())
    cs.setWanted(true)
    const d = decoder()
    expect(cs.configure(d, build)).toEqual({ ...BASE, colorSpace: SET_S })
    expect(cs.applied).toBe(true)
    expect(cs.configure(d, build).colorSpace).toEqual(SET_S)
    cs.setWanted(false)
    expect('colorSpace' in cs.configure(d, build)).toBe(false)
  })

  it('retries a configure() that threw on the colour space without it, on the same decoder', () => {
    const report = vi.fn()
    const cs = createDecoderColorSpace(report)
    cs.setWanted(true)
    const d = decoder({ throwOnColorSpace: true })
    const applied = cs.configure(d, build)
    expect(applied).toEqual(BASE)
    expect(d.configs).toEqual([{ ...BASE, colorSpace: SET_S }, BASE])
    expect(cs.applied).toBe(false)
    expect(report).toHaveBeenCalledTimes(1)
    expect(report.mock.calls[0]![0]).toBe('configure')
    expect(report.mock.calls[0]![1]).toBeInstanceOf(TypeError)
    // Dropped for good: the next configure goes plain, with no second report.
    expect(cs.configure(d, build)).toEqual(BASE)
    expect(report).toHaveBeenCalledTimes(1)
  })

  it('throws the retry error when the config fails without the colour space too', () => {
    const report = vi.fn()
    const cs = createDecoderColorSpace(report)
    cs.setWanted(true)
    const d = decoder({ throwAlways: true })
    expect(() => cs.configure(d, build)).toThrow('unsupported codec')
    expect(d.configs).toHaveLength(2)
    expect(report).toHaveBeenCalledTimes(1)
  })

  it('rethrows at once when there was no colour space to drop', () => {
    const report = vi.fn()
    const cs = createDecoderColorSpace(report)
    const d = decoder({ throwAlways: true })
    expect(() => cs.configure(d, build)).toThrow('unsupported codec')
    expect(d.configs).toHaveLength(1)
    expect(report).not.toHaveBeenCalled()
  })

  it('asks for one rebuild when the decoder errors before its first frame with set S', () => {
    const report = vi.fn()
    const cs = createDecoderColorSpace(report)
    cs.setWanted(true)
    cs.configure(decoder(), build)
    const err = new Error('Decoder initialization failed')
    expect(cs.retryAfterError(err, 0)).toBe(true)
    expect(report).toHaveBeenCalledWith('decode', err)
    expect(cs.applied).toBe(false)
    // The rebuilt decoder is configured without it…
    expect('colorSpace' in cs.configure(decoder(), build)).toBe(false)
    // …and its own failure is reported as before: one retry a session.
    expect(cs.retryAfterError(err, 0)).toBe(false)
    expect(report).toHaveBeenCalledTimes(1)
  })

  it('never retries once a frame has been decoded with it', () => {
    const report = vi.fn()
    const cs = createDecoderColorSpace(report)
    cs.setWanted(true)
    cs.configure(decoder(), build)
    expect(cs.retryAfterError(new Error('driver hiccup'), 1)).toBe(false)
    expect(cs.applied).toBe(true)
    expect(report).not.toHaveBeenCalled()
  })
})

describe('frameColorSpace', () => {
  it("reads the decoder's colour space off a frame", () => {
    const frame = { colorSpace: { toJSON: () => ({ ...SET_S }) } }
    expect(frameColorSpace(frame)).toEqual(SET_S)
  })

  it('is null where the browser says nothing, or throws', () => {
    expect(frameColorSpace(null)).toBeNull()
    expect(frameColorSpace({})).toBeNull()
    expect(frameColorSpace({ colorSpace: {} })).toBeNull()
    expect(frameColorSpace({ colorSpace: { toJSON: () => { throw new Error('closed') } } })).toBeNull()
  })
})

describe('colorSpaceLabel', () => {
  it('names each member, and none for none', () => {
    expect(colorSpaceLabel(SET_S)).toBe(
      'matrix=smpte170m primaries=bt709 transfer=iec61966-2-1 range=limited',
    )
    expect(colorSpaceLabel({ matrix: 'bt709', primaries: null, transfer: null, fullRange: null })).toBe(
      'matrix=bt709 primaries=unspecified transfer=unspecified range=unspecified',
    )
    expect(colorSpaceLabel(null)).toBe('none')
    expect(colorSpaceLabel(undefined)).toBe('none')
    expect(colorSpaceLabel('bt709')).toBe('none')
  })
})
