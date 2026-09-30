// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// FR-88 (#1790) §3b — the SPA's purestat goals. The load-bearing property is
// the no-op: purestat may be blocked, not loaded yet, or turned off, and a
// goal must then do nothing and throw nothing, because it sits on the success
// path of a sign-up.
import { afterEach, describe, expect, it, vi } from 'vitest'
import { GOALS, isOAuthSignup, trackGoal } from '@/utils/goals'

type W = Window & { purestat?: unknown }

afterEach(() => {
  delete (window as W).purestat
})

describe('trackGoal', () => {
  it('names the four goals configured in purestat', () => {
    expect([...GOALS]).toEqual(['signup', 'subscribe', 'install-copy', 'github-outbound'])
  })

  it('is a no-op when window.purestat is absent', () => {
    expect((window as W).purestat).toBeUndefined()
    expect(() => trackGoal('signup')).not.toThrow()
  })

  it('is a no-op when window.purestat is not a function', () => {
    ;(window as W).purestat = 'loading'
    expect(() => trackGoal('signup')).not.toThrow()
  })

  it('calls purestat with the goal name once', () => {
    const spy = vi.fn()
    ;(window as W).purestat = spy
    trackGoal('install-copy')
    expect(spy).toHaveBeenCalledTimes(1)
    expect(spy).toHaveBeenCalledWith('install-copy')
  })

  it('swallows a throwing purestat: analytics must not break the flow it measures', () => {
    ;(window as W).purestat = () => {
      throw new Error('beacon failed')
    }
    expect(() => trackGoal('subscribe')).not.toThrow()
  })
})

describe('isOAuthSignup', () => {
  it('is true only for the server’s marker', () => {
    expect(isOAuthSignup('#token=abc&signup=1', '')).toBe(true)
    expect(isOAuthSignup('#signup=1', '')).toBe(true)
    expect(isOAuthSignup('', '?signup=1')).toBe(true)
  })

  it('is false for a sign-in to an existing account (no marker)', () => {
    expect(isOAuthSignup('#token=abc', '')).toBe(false)
    expect(isOAuthSignup('', '')).toBe(false)
    expect(isOAuthSignup('#signup=0', '?signup=true')).toBe(false)
  })
})
