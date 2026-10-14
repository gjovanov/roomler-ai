// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
import { describe, expect, it } from 'vitest'
import { AGENT_ACCOUNT_TITLE, servesSshAsSystem, sshAsChip } from '@/utils/sshIdentity'

describe('sshAsChip (FR-105)', () => {
  it("names SYSTEM only when the device's service served the session", () => {
    expect(sshAsChip('daemon', 'system')).toMatchObject({ label: 'SYSTEM', color: 'warning' })
  })

  it('names the signed-in user when the service served it as that user', () => {
    const chip = sshAsChip('console_user', 'console_user')
    expect(chip?.label).toBe('signed-in user')
    expect(chip?.title).toContain('SYSTEM service')
  })

  it("never calls the worker's daemon identity SYSTEM: on an attended install it is the user", () => {
    for (const ranAs of ['worker', null, undefined]) {
      const chip = sshAsChip('daemon', ranAs)
      expect(chip?.label).toBe("agent's account")
      expect(chip?.label).not.toContain('SYSTEM')
      expect(chip?.title).toBe(AGENT_ACCOUNT_TITLE)
      expect(chip?.title).toContain('signed-in user on an attended Windows install')
    }
  })

  it('keeps the plain account labels on the worker path', () => {
    expect(sshAsChip('console_user', 'worker')?.label).toBe('signed-in user')
    expect(sshAsChip('named', 'worker')?.label).toBe('named account')
  })

  it('shows nothing for a refusal, which names no account', () => {
    expect(sshAsChip(null, null)).toBeNull()
    expect(sshAsChip(undefined, undefined)).toBeNull()
  })

  it('shows a mode newer than this UI as sent, never blank', () => {
    expect(sshAsChip('something_new', 'worker')?.label).toBe('something_new')
  })
})

describe('servesSshAsSystem (FR-105)', () => {
  it('reads the ssh-system word', () => {
    expect(servesSshAsSystem(['exec', 'ssh', 'ssh-system'])).toBe(true)
  })

  it('matches by equality: ssh and ssh-consent are not ssh-system', () => {
    expect(servesSshAsSystem(['ssh', 'ssh-consent', 'ssh-grant-ack', 'exec-system'])).toBe(false)
  })

  it('is false without caps', () => {
    expect(servesSshAsSystem(undefined)).toBe(false)
    expect(servesSshAsSystem(null)).toBe(false)
    expect(servesSshAsSystem([])).toBe(false)
  })
})
