// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
import { describe, expect, it } from 'vitest'
import {
  crashChip,
  installChip,
  installSentence,
  type CrashHealth,
  type LastInstallWord,
} from '@/utils/deviceHealth'
import { presenceColor, presenceIcon, presenceLabel, type PresenceRow } from '@/utils/devicePresence'

const NOW_S = 1_795_000_000
const NOW = NOW_S * 1000

const word = (status: string, more: Partial<LastInstallWord> = {}): LastInstallWord => ({
  status,
  expected_version: 'agent-v0.4.240',
  new_binary_version: '0.4.240',
  started_unix: NOW_S - 7200,
  finished_unix: NOW_S - 7180,
  ...more,
})

describe('installChip and installSentence (FR-67 P5)', () => {
  it('says nothing for an agent that predates the word', () => {
    expect(installChip(undefined, NOW)).toBeNull()
    expect(installChip(null, NOW)).toBeNull()
    expect(installSentence(undefined, NOW)).toBe('')
  })

  it('keeps no record, a watcher at work and an install that never reported apart', () => {
    const absent = { status: 'Absent' }
    expect(installChip(absent, NOW)).toBeNull()
    expect(installSentence(absent, NOW)).toContain('none recorded')

    const atWork = word('InProgress', { started_unix: NOW_S - 30, finished_unix: undefined })
    expect(installChip(atWork, NOW)?.color).toBe('info')
    expect(installChip(atWork, NOW)?.text).toBe('updating')
    expect(installSentence(atWork, NOW)).toContain('in progress')

    const frozen = { ...atWork, never_reported: true }
    expect(installChip(frozen, NOW)?.color).toBe('warning')
    expect(installChip(frozen, NOW)?.text).toBe('update never reported')
    expect(installSentence(frozen, NOW)).toContain('NEVER REPORTED')
  })

  it('renders each terminal verdict, and is quiet for a verified one', () => {
    const verified = word('SucceededVerified')
    expect(installChip(verified, NOW)).toBeNull()
    expect(installSentence(verified, NOW)).toContain('0.4.240')
    expect(installSentence(verified, NOW)).toContain('verified')

    const cells: Array<[string, string, string]> = [
      ['SucceededUnverified', 'warning', 'update unverified'],
      ['InstallerFailed', 'error', 'update failed'],
      ['Timeout', 'error', 'update timed out'],
      ['WatcherStopped', 'warning', 'update not confirmed'],
      ['Unreadable', 'warning', 'update record unreadable'],
    ]
    for (const [status, color, text] of cells) {
      const chip = installChip(word(status), NOW)
      expect(chip?.color, status).toBe(color)
      expect(chip?.text, status).toBe(text)
      expect(installSentence(word(status), NOW), status).not.toBe('')
    }
  })

  it('ignores a word this page does not know, as an unknown update_mode is ignored', () => {
    expect(installChip(word('SomethingNew'), NOW)).toBeNull()
  })
})

const crash = (state: string, more: Partial<CrashHealth> = {}): CrashHealth => ({
  state,
  crashes: 0,
  window_secs: 600,
  ...more,
})

describe('crashChip (#3332)', () => {
  it('says nothing for an agent that predates the word, nothing to report, or an unknown state', () => {
    expect(crashChip(undefined, NOW)).toBeNull()
    expect(crashChip(null, NOW)).toBeNull()
    expect(crashChip(crash('ok', { last_known_good: '0.4.240' }), NOW)).toBeNull()
    expect(crashChip(crash('something-new', { crashes: 9 }), NOW)).toBeNull()
  })

  it('gives the count and the window while the device crashes', () => {
    const chip = crashChip(crash('crashing', { crashes: 2, last_crash_unix: NOW_S - 60 }), NOW)
    expect(chip?.color).toBe('warning')
    expect(chip?.text).toBe('2 crashes in 10 min')
    expect(chip?.tooltip).toContain('crash reports')
  })

  it('says what the ladder did: rolled back, or could not roll back with the accused version', () => {
    const back = crashChip(
      crash('rolled_back', { crashes: 3, last_crash_unix: NOW_S - 60, last_known_good: '0.4.239' }),
      NOW,
    )
    expect(back?.color).toBe('warning')
    expect(back?.text).toBe('rolled back to 0.4.239')

    const failed = crashChip(
      crash('rollback_failed', {
        crashes: 3,
        last_crash_unix: NOW_S - 60,
        accused_version: '0.4.240',
        last_known_good: '0.4.239',
      }),
      NOW,
    )
    expect(failed?.color).toBe('error')
    expect(failed?.text).toBe('crash loop: 0.4.240')
    expect(failed?.tooltip).toContain('rollback_failed')
    expect(failed?.tooltip).toContain('0.4.240')
  })

  it('leaves presence as it is in all three states: absent, nothing and something', () => {
    type Row = PresenceRow & { capabilities?: { crash_health?: CrashHealth } }
    const base: Row = { kind: 'agent', presence: 'online', is_online: true, os: 'windows' }
    const rows: Row[] = [
      base,
      { ...base, capabilities: { crash_health: crash('ok') } },
      {
        ...base,
        capabilities: {
          crash_health: crash('rollback_failed', { crashes: 3, accused_version: '0.4.240' }),
        },
      },
    ]
    for (const r of rows) {
      expect(presenceLabel(r)).toBe(presenceLabel(base))
      expect(presenceColor(r)).toBe(presenceColor(base))
      expect(presenceIcon(r)).toBe(presenceIcon(base))
    }
    expect(crashChip(rows[0].capabilities?.crash_health, NOW)).toBeNull()
    expect(crashChip(rows[1].capabilities?.crash_health, NOW)).toBeNull()
    expect(crashChip(rows[2].capabilities?.crash_health, NOW)?.color).toBe('error')
  })
})
