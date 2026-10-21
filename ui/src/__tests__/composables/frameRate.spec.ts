// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
import { describe, expect, it } from 'vitest'
import {
  FPS_CAP,
  FRAME_RATES,
  agentOffersHighRefresh,
  describeFrameRateReason,
  fpsWireMessage,
  frameRateAnswerOf,
  frameRateOptions,
  frameRateSummary,
  toFrameRate,
} from '@/composables/frameRate'

describe('FR-119 P1 — the frame rate', () => {
  it('matches the agent capability by equality, as the agent spells it', () => {
    expect(FPS_CAP).toBe('fps-high')
    expect(agentOffersHighRefresh(['chunk-framing', 'fps-high'])).toBe(true)
    expect(agentOffersHighRefresh(['chunk-framing'])).toBe(false)
    expect(agentOffersHighRefresh(['fps-high-x'])).toBe(false)
    expect(agentOffersHighRefresh(undefined)).toBe(false)
  })

  it('offers 60 always, and above 60 only to an agent advertising fps-high', () => {
    const old = frameRateOptions(['chunk-framing'], null)
    expect(old.map((o) => [o.fps, o.enabled])).toEqual([
      [60, true],
      [120, false],
      [144, false],
    ])
    expect(old[1].why).toContain('at most 60 fps')
    const high = frameRateOptions(['chunk-framing', 'fps-high'], null)
    expect(high.every((o) => o.enabled)).toBe(true)
  })

  it('disables a rate the host screen cannot refresh at, when the host said', () => {
    const at120 = frameRateOptions(['fps-high'], 120)
    expect(at120.map((o) => [o.fps, o.enabled])).toEqual([
      [60, true],
      [120, true],
      [144, false],
    ])
    expect(at120[2].why).toBe("the host's screen refreshes at 120 Hz")
    // Unknown (null) or nonsense (0) refresh decides nothing.
    expect(frameRateOptions(['fps-high'], 0).every((o) => o.enabled)).toBe(true)
  })

  it('asks with the wire message the agent parses', () => {
    expect(fpsWireMessage(120)).toEqual({ t: 'rc:fps', fps: 120 })
    expect(FRAME_RATES).toEqual([60, 120, 144])
  })

  it('reads a stored pick only when it is a rate on offer', () => {
    expect(toFrameRate('120')).toBe(120)
    expect(toFrameRate(144)).toBe(144)
    expect(toFrameRate('90')).toBeNull()
    expect(toFrameRate(null)).toBeNull()
    expect(toFrameRate('abc')).toBeNull()
  })

  it("reads the agent's answer from rc:video-info, both-or-neither for the refusal", () => {
    expect(frameRateAnswerOf({ fps: 60, fps_asked: 120, fps_reason: 'unproven' })).toEqual({
      fps: 60,
      asked: 120,
      reason: 'unproven',
    })
    expect(frameRateAnswerOf({ fps: 60 })).toEqual({ fps: 60, asked: null, reason: null })
    // A half refusal explains nothing.
    expect(frameRateAnswerOf({ fps: 60, fps_asked: 120 })).toEqual({ fps: 60, asked: null, reason: null })
    expect(frameRateAnswerOf({ fps: 60, fps_reason: 'unproven' })).toEqual({ fps: 60, asked: null, reason: null })
    // An agent without fps-high sends no fps at all.
    expect(frameRateAnswerOf({})).toBeNull()
    expect(frameRateAnswerOf(null)).toBeNull()
  })

  it('words the answer, and keeps an unknown reason as it came', () => {
    expect(frameRateSummary(null)).toBeNull()
    expect(frameRateSummary({ fps: 60, asked: null, reason: null })).toBe('60 fps')
    expect(frameRateSummary({ fps: 60, asked: 120, reason: 'unproven' })).toBe(
      "60 fps (120 fps not granted: this host hasn't shown it can sustain that rate yet)",
    )
    expect(describeFrameRateReason('constrained')).toContain('relayed')
    expect(describeFrameRateReason('a-newer-word')).toBe('a-newer-word')
  })
})
