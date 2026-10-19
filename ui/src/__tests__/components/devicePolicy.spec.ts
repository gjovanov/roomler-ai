// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// The one-line summary a collapsed device card shows in place of its six
// access-policy controls: the consent mode always, then only what differs
// from the defaults.
import { describe, expect, it } from 'vitest'
import type { AccessPolicy } from '@/stores/agents'
import { idleMinuteItems, ownerPromptLabel, policySummary } from '@/components/admin/devicePolicy'

function policy(over: Partial<AccessPolicy> = {}): AccessPolicy {
  return {
    consent_mode: null,
    prompt_owner: null,
    allowed_role_ids: [],
    allowed_user_ids: [],
    auto_terminate_idle_minutes: null,
    input_mode: null,
    rc_overlay: null,
    lock_on_end: null,
    ...over,
  }
}

describe('policySummary', () => {
  it('says the consent mode, and nothing else, for a device on the defaults', () => {
    expect(policySummary(policy(), false)).toEqual(['Prompt on host'])
  })

  it('names every setting that differs from its default', () => {
    expect(
      policySummary(
        policy({
          consent_mode: 'email',
          input_mode: 'exclusive',
          auto_terminate_idle_minutes: 30,
          lock_on_end: 'always',
          rc_overlay: true,
        }),
        false,
      ),
    ).toEqual(['Email owner', 'one controller', 'idle 30 min', 'lock at end', 'over the mesh'])
    expect(policySummary(policy({ lock_on_end: 'auto' }), false)).toContain('lock if locked at start')
  })

  it('tells the owner that consent is skipped for them (FR-27), unless they asked to be asked', () => {
    expect(policySummary(policy(), true)).toEqual(['Prompt on host', 'skipped for you'])
    expect(policySummary(policy({ prompt_owner: true }), true)).toEqual(['Prompt on host'])
    // Somebody else's device: the mode applies to the viewer as it reads.
    expect(policySummary(policy(), false)).not.toContain('skipped for you')
  })

  it('does not repeat the exemption under auto-grant, where nobody is asked anyway', () => {
    expect(policySummary(policy({ consent_mode: 'auto' }), true)).toEqual(['Auto-grant'])
  })

  it('treats "never" and a zero idle limit as the defaults they are', () => {
    expect(
      policySummary(policy({ lock_on_end: 'never', auto_terminate_idle_minutes: 0 }), false),
    ).toEqual(['Prompt on host'])
  })

  it('is empty for a device with no policy yet', () => {
    expect(policySummary(undefined, true)).toEqual([])
  })
})

describe('idleMinuteItems', () => {
  it('keeps a stored value that is no preset, so the select is never blank', () => {
    expect(idleMinuteItems(45).map((i) => i.value)).toEqual([0, 5, 15, 30, 45, 60, 120])
    expect(idleMinuteItems(30).map((i) => i.value)).toEqual([0, 5, 15, 30, 60, 120])
  })
})

describe('ownerPromptLabel', () => {
  it('says what is true now, in both states', () => {
    expect(ownerPromptLabel(policy())).toBe('Ask me too (you own this device, so consent is skipped for you)')
    expect(ownerPromptLabel(policy({ prompt_owner: true, consent_mode: 'auto' }))).toBe(
      'Ask me too (you own this device; "Auto-grant (unattended)" applies to you)',
    )
  })
})
