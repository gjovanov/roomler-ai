// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
import { describe, expect, it } from 'vitest'
import {
  isAsleep,
  presenceColor,
  presenceIcon,
  presenceLabel,
  presenceTitle,
  type PresenceRow,
  type SleepView,
} from '@/utils/devicePresence'

const SINCE = '2026-10-30T12:00:00Z'
const fmt = (iso: string) => `<${iso}>`

function row(overrides: Partial<PresenceRow> = {}): PresenceRow {
  return { kind: 'agent', presence: 'offline', is_online: false, os: 'windows', ...overrides }
}

function sleep(policy: string, held: boolean, on_ac?: boolean): SleepView {
  return { since: SINCE, power: { policy, held, ...(on_ac === undefined ? {} : { on_ac }) } }
}

describe('device presence (FR-55 P4)', () => {
  it('reads an offline device that gave no reason as offline, unchanged', () => {
    const r = row()
    expect(isAsleep(r)).toBe(false)
    expect(presenceLabel(r)).toBe('offline')
    expect(presenceIcon(r)).toBe('mdi-circle-outline')
    expect(presenceColor(r)).toBe('grey')
    expect(presenceTitle(r, fmt)).toBe('Offline')
  })

  it('reads a device that announced its sleep as asleep, with its own icon and colour', () => {
    const r = row({ sleep: sleep('never', false, false) })
    expect(isAsleep(r)).toBe(true)
    expect(presenceLabel(r)).toBe('asleep')
    expect(presenceIcon(r)).toBe('mdi-sleep')
    expect(presenceColor(r)).toBe('blue-grey')
  })

  it('says a sleep with nothing held was by policy, and how to keep the device reachable', () => {
    const never = presenceTitle(row({ sleep: sleep('never', false, false) }), fmt)
    expect(never).toContain(`Asleep since <${SINCE}>, as its power policy allows`)
    expect(never).toContain('power policy “never”, on battery')
    expect(never).toContain('set power_policy to on-ac or always')
    expect(never).toContain('reconnects by itself when it wakes')

    const onAcOnBattery = presenceTitle(row({ sleep: sleep('on-ac', false, false) }), fmt)
    expect(onAcOnBattery).toContain('as its power policy allows')
    expect(onAcOnBattery).toContain('on-ac holds only on mains: plug it in')
  })

  it('says a sleep past the hold was not the policy, and names the clamshell limit on a Mac', () => {
    const pc = presenceTitle(row({ sleep: sleep('on-ac', true, true) }), fmt)
    expect(pc).toContain(`Asleep since <${SINCE}>, past its hold`)
    expect(pc).toContain('power policy “on-ac”, on mains')
    expect(pc).not.toContain('as its power policy allows')
    expect(pc).not.toContain('lid closed')

    const mac = presenceTitle(row({ os: 'macos', sleep: sleep('always', true) }), fmt)
    expect(mac).toContain('past its hold (power policy “always”)')
    expect(mac).toContain('A Mac with its lid closed and no external display sleeps')
  })

  it('never calls an online or stale device asleep, whatever its row still says', () => {
    const online = row({ presence: 'online', is_online: true, sleep: sleep('never', false) })
    expect(isAsleep(online)).toBe(false)
    expect(presenceLabel(online)).toBe('online')
    expect(presenceTitle(online, fmt)).toBe('Online — a control socket is registered')
    const stale = row({ presence: 'stale', sleep: sleep('never', false) })
    expect(presenceLabel(stale)).toBe('stale')
    expect(presenceIcon(stale)).toBe('mdi-circle-half-full')
  })

  it('keeps a tunnel client on its stored status', () => {
    const t = row({ kind: 'tunnel_client', sleep: sleep('never', false) })
    expect(isAsleep(t)).toBe(false)
    expect(presenceTitle(t, fmt)).toBe('Offline (stored status)')
  })
})
