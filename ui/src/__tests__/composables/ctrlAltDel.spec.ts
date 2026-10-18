// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
import { describe, it, expect } from 'vitest'
import { agentSendsSas, ctrlAltDelFrames, SAS_CAP } from '@/composables/ctrlAltDel'

describe('FR-97 P4a — the Ctrl+Alt+Del button', () => {
  it('sends the real sequence only to an agent that advertises it', () => {
    expect(SAS_CAP).toBe('sas')
    expect(agentSendsSas(['arbiter', 'sas'])).toBe(true)
    // An older agent cannot parse `{t: 'sas'}`: absent, empty or unknown is no.
    expect(agentSendsSas(undefined)).toBe(false)
    expect(agentSendsSas(null)).toBe(false)
    expect(agentSendsSas([])).toBe(false)
    // Equality, never a prefix.
    expect(agentSendsSas(['sas-v2', 'xsas'])).toBe(false)
  })

  it('is one sas frame where the host sends it', () => {
    expect(ctrlAltDelFrames(['arbiter', 'sas'])).toEqual([{ t: 'sas' }])
  })

  it('is the three keys anywhere else, downs in order and releases in reverse', () => {
    const frames = ctrlAltDelFrames(['arbiter', 'mouse-rel'])
    expect(frames).toEqual([
      { t: 'key', code: 0xe0, down: true, mods: 1 },
      { t: 'key', code: 0xe2, down: true, mods: 5 },
      { t: 'key', code: 0x4c, down: true, mods: 5 },
      { t: 'key', code: 0x4c, down: false, mods: 5 },
      { t: 'key', code: 0xe2, down: false, mods: 1 },
      { t: 'key', code: 0xe0, down: false, mods: 0 },
    ])
    expect(ctrlAltDelFrames(undefined)).toEqual(frames)
  })
})
