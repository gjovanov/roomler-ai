// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * FR-130 P2 — the viewer tells its decoders the colour space the agent
 * converted with.
 *
 * Every agent converts the captured BGRA to YUV with BT.601 at limited range,
 * on every path (the CPU converters, the D3D11 VideoProcessor, VideoToolbox's
 * attached matrix). Before FR-130 P1 no encoder writes a colour description,
 * so Chrome decodes an HD picture as BT.709 and every saturated colour shifts:
 * magenta 191/0/191 arrives as 205/30/197 (#3049). `VideoDecoderConfig.colorSpace`
 * overrides what the bitstream says, so passing it fixes a session from every
 * agent version, the fleet's older ones included. For VP9 it is the ONLY fix:
 * Chrome's WebCodecs ignores VP9's in-band colour space (FR-130 §2.5, measured
 * in Chrome 156 and Chromium 149, hardware and software decode, 2D and WebGL).
 *
 * One definition (`RC_DECODER_COLOR_SPACE`, set S) and one config builder
 * (`rcDecoderConfig`) serve the three decode workers: `rc-vp9-444-worker` (VP9,
 * AV1 and H.264 over the data channel), `rc-hevc-worker` and
 * `rc-webcodecs-worker` (the RTP transform). Each worker is told per session
 * whether to pass it; the composable decides (`sessionColorSpace`, with the
 * per-browser kill switch `roomler-rc-colorspace=off`).
 *
 * ⚠️ Never cost the picture. A browser that rejects the member, or a value in
 * it, must still decode. `createDecoderColorSpace` retries a `configure()` that
 * threw without the colour space, and tells a worker to rebuild its decoder
 * without it, once, when the decoder errors before its first frame.
 * `isConfigSupported` probes stay codec-only: the colour space joins only the
 * config handed to `configure()`.
 */

/** FR-130 set S (§3a; open decision 1, decided by P0's measurement): the matrix
 *  every converter uses (SMPTE 170M, which is BT.601), limited range, and the
 *  primaries and transfer of the sRGB desktop the agent captured. In Chrome the
 *  sets A, B and S decode identically; S is the one that describes the source
 *  truthfully, which matters to a player that colour-manages by primaries and
 *  transfer. */
export const RC_DECODER_COLOR_SPACE: Readonly<VideoColorSpaceInit> = Object.freeze({
  matrix: 'smpte170m',
  primaries: 'bt709',
  transfer: 'iec61966-2-1',
  fullRange: false,
})

/** The `VideoDecoderConfig` a decode worker passes to `configure()`: the
 *  worker's own `base` (codec, latency, hardware preference, unchanged) with
 *  set S added when `colorSpace` is true. When it is false the result carries
 *  no `colorSpace` member at all, so the decoder reads the bitstream's own
 *  description, exactly as before FR-130. The colour space is a fresh copy:
 *  nothing a caller does to the config can reach `RC_DECODER_COLOR_SPACE`. */
export function rcDecoderConfig(base: VideoDecoderConfig, colorSpace: boolean): VideoDecoderConfig {
  const config: VideoDecoderConfig = { ...base }
  delete config.colorSpace
  if (colorSpace) config.colorSpace = { ...RC_DECODER_COLOR_SPACE }
  return config
}

/** What `configure()` needs of a decoder (a `VideoDecoder`, or a test's fake). */
export interface ConfigurableDecoder {
  configure(config: VideoDecoderConfig): void
}

/** Where the colour space was given up: `configure()` threw, or the decoder
 *  errored before its first frame. */
export type ColorSpaceDropStage = 'configure' | 'decode'

/** A decode worker's colour-space state for its session. */
export interface DecoderColorSpace {
  /** The session's flag from the main thread. Sticky: a message that omits it
   *  (the idempotent visible-canvas re-init) leaves it as it was. */
  setWanted(on: boolean): void
  /** `configure()` the decoder with `build(colorSpace)`: set S while it is
   *  wanted and has not been dropped. When that throws and the config carried
   *  the colour space, it is dropped for good and the same config goes again
   *  without it. WebCodecs throws from `configure()` (a `TypeError`, for a
   *  value this browser's enums lack) before the decoder changes state, so
   *  the same decoder takes the retry. Returns the config that took; throws
   *  the retry's error, or the first one when there was nothing to drop, for
   *  the worker's own `decoder-configure-error`. */
  configure(decoder: ConfigurableDecoder, build: (colorSpace: boolean) => VideoDecoderConfig): VideoDecoderConfig
  /** The decoder errored (which closes it). True, once a session, when the
   *  failed decoder was configured with the colour space and has produced no
   *  frame yet: the worker should then build a new decoder (its next
   *  `configure()` goes without the colour space) and wait for a key frame,
   *  instead of reporting the error. False otherwise: report it as before. */
  retryAfterError(error: unknown, framesDecoded: number): boolean
  /** Whether the decoder configured last was told the colour space. */
  readonly applied: boolean
}

/** `report` is called once, when the colour space is given up, so the worker
 *  can tell the main thread (one console line there). */
export function createDecoderColorSpace(
  report: (stage: ColorSpaceDropStage, error: unknown) => void,
): DecoderColorSpace {
  let wanted = false
  let dropped = false
  let applied = false
  return {
    setWanted(on: boolean): void {
      wanted = on
    },
    configure(decoder, build) {
      const config = build(wanted && !dropped)
      try {
        decoder.configure(config)
        applied = config.colorSpace !== undefined
        return config
      } catch (err) {
        if (config.colorSpace === undefined) throw err
        dropped = true
        applied = false
        report('configure', err)
        const plain = build(false)
        decoder.configure(plain)
        return plain
      }
    },
    retryAfterError(error: unknown, framesDecoded: number): boolean {
      if (!applied || dropped || framesDecoded > 0) return false
      dropped = true
      applied = false
      report('decode', error)
      return true
    },
    get applied() {
      return applied
    },
  }
}

/** A decoded frame's colour space, for the `first-frame` diagnostics: what the
 *  decoder says it produced, `null` where the browser does not say. Read it
 *  before the frame is painted, which closes it. */
export function frameColorSpace(frame: unknown): VideoColorSpaceInit | null {
  try {
    const cs = (frame as { colorSpace?: { toJSON?: () => VideoColorSpaceInit } } | null)?.colorSpace
    if (!cs || typeof cs.toJSON !== 'function') return null
    const json = cs.toJSON()
    return json && typeof json === 'object' ? json : null
  } catch {
    return null
  }
}

/** One console token for a colour space from a worker message, e.g.
 *  `matrix=smpte170m primaries=bt709 transfer=iec61966-2-1 range=limited`;
 *  `none` when there is none (the decoder was not told one, or the browser
 *  reports none). */
export function colorSpaceLabel(cs: unknown): string {
  if (!cs || typeof cs !== 'object') return 'none'
  const c = cs as VideoColorSpaceInit
  const range = c.fullRange === true ? 'full' : c.fullRange === false ? 'limited' : 'unspecified'
  return `matrix=${c.matrix ?? 'unspecified'} primaries=${c.primaries ?? 'unspecified'}`
    + ` transfer=${c.transfer ?? 'unspecified'} range=${range}`
}
