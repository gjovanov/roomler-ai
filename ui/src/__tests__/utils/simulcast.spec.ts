// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// FR-127 P2 — the ladder the server hands the client, read whole or not at
// all, and the camera's capture size from its track.
import { describe, it, expect } from 'vitest'
import { cameraEncodings, captureSize } from '@/utils/simulcast'

const ladder = {
  encodings: [
    { scale_resolution_down_by: 2, max_bitrate: 300000, scalability_mode: 'L1T3' },
    { scale_resolution_down_by: 1, max_bitrate: 1500000, scalability_mode: 'L1T3' },
  ],
}

describe('cameraEncodings (FR-127 P2)', () => {
  it('maps the wire ladder onto RTCRtpEncodingParameters, in the order sent', () => {
    expect(cameraEncodings(ladder)).toEqual([
      { scaleResolutionDownBy: 2, maxBitrate: 300000, scalabilityMode: 'L1T3' },
      { scaleResolutionDownBy: 1, maxBitrate: 1500000, scalabilityMode: 'L1T3' },
    ])
  })

  it('reads three rungs too, and fills a missing mode with L1T3', () => {
    const three = {
      encodings: [
        { scale_resolution_down_by: 4, max_bitrate: 100000 },
        ...ladder.encodings,
      ],
    }
    expect(cameraEncodings(three)?.map((e) => e.scaleResolutionDownBy)).toEqual([4, 2, 1])
    expect(cameraEncodings(three)?.[0].scalabilityMode).toBe('L1T3')
  })

  it('is one encoding (null) when the lever is off, the server is older, or the ladder is short', () => {
    expect(cameraEncodings(null)).toBeNull()
    expect(cameraEncodings(undefined)).toBeNull()
    expect(cameraEncodings({})).toBeNull()
    expect(cameraEncodings({ encodings: [] })).toBeNull()
    expect(cameraEncodings({ encodings: [ladder.encodings[1]] })).toBeNull()
    expect(cameraEncodings('L1T3')).toBeNull()
  })

  it('refuses a ladder it cannot read whole, never a half-parsed one', () => {
    const bad = (rung: unknown) => ({ encodings: [ladder.encodings[0], rung] })
    expect(cameraEncodings(bad(null))).toBeNull()
    expect(cameraEncodings(bad({ scale_resolution_down_by: 0, max_bitrate: 1 }))).toBeNull()
    expect(cameraEncodings(bad({ scale_resolution_down_by: 1, max_bitrate: 0 }))).toBeNull()
    expect(cameraEncodings(bad({ scale_resolution_down_by: 'x', max_bitrate: 1 }))).toBeNull()
    expect(cameraEncodings(bad({ scale_resolution_down_by: Infinity, max_bitrate: 1 }))).toBeNull()
  })
})

describe('captureSize (FR-127 P2)', () => {
  const track = (settings: unknown) => ({ getSettings: () => settings }) as unknown as MediaStreamTrack

  it('reads the track settings, rounded', () => {
    expect(captureSize(track({ width: 640, height: 480 }))).toEqual({ w: 640, h: 480 })
    expect(captureSize(track({ width: 1279.6, height: 719.5 }))).toEqual({ w: 1280, h: 720 })
  })

  it('is null when the track cannot say', () => {
    expect(captureSize(track({}))).toBeNull()
    expect(captureSize(track({ width: 0, height: 480 }))).toBeNull()
    expect(captureSize({} as unknown as MediaStreamTrack)).toBeNull()
    expect(
      captureSize({
        getSettings: () => {
          throw new Error('no settings')
        },
      } as unknown as MediaStreamTrack),
    ).toBeNull()
  })
})
