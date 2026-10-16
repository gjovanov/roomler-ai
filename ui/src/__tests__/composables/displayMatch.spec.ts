// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * FR-108 — the viewer's half of display match: the device's answer, read and
 * worded.
 */
import { describe, expect, it } from 'vitest'
import {
  describeDisplayMatchRefusal,
  displayMatchSummary,
  parseDisplayMatchState,
} from '@/composables/displayMatch'
import { parseControlInbound } from '@/composables/useRemoteControl'

describe('the device answer', () => {
  it('reads a switch, a refusal and an off', () => {
    expect(parseDisplayMatchState({ t: 'rc:display-match.state', on: true, mode: { w: 1600, h: 900 } })).toEqual({
      on: true,
      mode: { w: 1600, h: 900 },
      refused: null,
    })
    expect(parseDisplayMatchState({ t: 'rc:display-match.state', on: false, refused: 'attended' })).toEqual({
      on: false,
      mode: null,
      refused: 'attended',
    })
    expect(parseDisplayMatchState({ t: 'rc:display-match.state', on: false })).toEqual({
      on: false,
      mode: null,
      refused: null,
    })
  })

  it('never reads a refusal as a switch, whatever else the frame says', () => {
    const s = parseDisplayMatchState({
      t: 'rc:display-match.state',
      on: true,
      mode: { w: 640, h: 480 },
      refused: 'attended',
    })
    expect(s?.on).toBe(false)
    expect(s?.refused).toBe('attended')
  })

  it('ignores what is not its frame', () => {
    expect(parseDisplayMatchState({ t: 'rc:keep-busy.state', on: true })).toBeNull()
    expect(parseDisplayMatchState({ t: 'rc:display-match.state' })).toBeNull()
    expect(parseDisplayMatchState({ t: 'rc:display-match.state', on: 'yes' })).toBeNull()
  })

  it('reaches the session through the control channel parser', () => {
    expect(parseControlInbound(JSON.stringify({ t: 'rc:display-match.state', on: false, refused: 'locked' }))).toEqual({
      kind: 'display_match',
      state: { on: false, mode: null, refused: 'locked' },
    })
  })
})

describe('the words', () => {
  it('says why the device did not switch', () => {
    expect(describeDisplayMatchRefusal('attended')).toMatch(/someone is using/)
    expect(describeDisplayMatchRefusal('locked')).toMatch(/locked/)
    expect(describeDisplayMatchRefusal('disabled_on_device')).toMatch(/owner/)
    expect(describeDisplayMatchRefusal('no_fitting_mode')).toMatch(/no display mode/)
    expect(describeDisplayMatchRefusal('switch_failed')).toMatch(/could not/)
    expect(describeDisplayMatchRefusal('unsupported')).toMatch(/can't/)
  })

  it('shows a word from a newer agent as it came, never as a success', () => {
    expect(describeDisplayMatchRefusal('a_word_from_the_future')).toBe('a_word_from_the_future')
    expect(displayMatchSummary({ on: false, mode: null, refused: 'a_word_from_the_future' })).toBe(
      'not now: a_word_from_the_future',
    )
  })

  it('has nothing to say before the device answers', () => {
    expect(displayMatchSummary(null)).toBeNull()
  })

  it('says the mode the host runs at', () => {
    expect(displayMatchSummary({ on: true, mode: { w: 1600, h: 900 }, refused: null })).toBe(
      'the host runs at 1600×900 for this session',
    )
    expect(displayMatchSummary({ on: false, mode: null, refused: null })).toBe('off for this session')
  })
})
