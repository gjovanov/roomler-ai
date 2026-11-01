// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
import { describe, it, expect } from 'vitest'
import {
  externalAuditAction,
  externalAuditResult,
  isOutsiderRow,
  type ExternalAuditEntry,
} from '@/utils/externalAudit'

const row = (over: Partial<ExternalAuditEntry>): ExternalAuditEntry => ({
  action: 'login',
  agent_id: 'a1',
  user_id: 'u1',
  actor: 'Someone',
  at: '2026-10-31T10:00:00Z',
  ...over,
})

describe('FR-52 P5: how a row of the external-access log reads', () => {
  it('a refused login or session never reads as one that went through', () => {
    for (const action of ['login', 'session']) {
      for (const reason of [
        'unavailable',
        'throttled',
        'rate_limited',
        'busy',
        'malformed',
        'rejected',
        'unknown_attempt',
        'session_limit',
        'other',
        'a-reason-from-a-newer-server',
      ]) {
        const r = externalAuditResult(row({ action, login_refused: reason }))
        expect(r.color, `${action} ${reason}`).toBe('error')
        expect(['verified', 'opened']).not.toContain(r.label)
      }
    }
  })

  it('a refusal names its reason, and the org reads what the outsider never did', () => {
    const r = externalAuditResult(
      row({
        login_refused: 'unavailable',
        login_detail: 'gate 1: the organization has external access switched off',
      }),
    )
    expect(r.why).toContain('gate 1')
    expect(externalAuditResult(row({ login_refused: 'throttled' })).label).toBe('too many attempts')
  })

  it('a reason this build does not know keeps its word', () => {
    expect(externalAuditResult(row({ login_refused: 'newer' })).label).toBe('newer')
  })

  it('an admin change that was refused reads as refused', () => {
    const r = externalAuditResult(row({ action: 'approve', approved: true, denied: 'not_device_admin' }))
    expect(r.color).toBe('error')
    expect(r.label).toBe('not a device admin')
  })

  it('what went through', () => {
    expect(externalAuditResult(row({})).label).toBe('verified')
    expect(externalAuditResult(row({ action: 'session', session_id: 's1' })).label).toBe('opened')
    expect(externalAuditResult(row({ action: 'rotate_code' })).label).toBe('rotated')
    expect(externalAuditResult(row({ action: 'approve', approved: false })).label).toBe('withdrawn')
  })

  it('an approval says on what terms, because it is the row that opens a device', () => {
    const r = externalAuditResult(
      row({ action: 'approve', approved: true, max_permissions: 'VIEW | INPUT' }),
    )
    expect(r.label).toBe('approved')
    expect(r.why).toBe('VIEW | INPUT')
    expect(externalAuditResult(row({ action: 'approve', approved: true })).why).toBeNull()
  })

  it('only a login or a session is someone from outside', () => {
    expect(isOutsiderRow(row({ action: 'login' }))).toBe(true)
    expect(isOutsiderRow(row({ action: 'session' }))).toBe(true)
    expect(isOutsiderRow(row({ action: 'approve' }))).toBe(false)
    expect(isOutsiderRow(row({ action: 'rotate_code' }))).toBe(false)
    expect(isOutsiderRow(row({ action: 'principal' }))).toBe(false)
  })

  it('a bar and a let-back read as what they did, and a refused one as refused', () => {
    const acted = (approved: boolean) =>
      externalAuditResult(row({ action: 'principal', approved, principal: 'p1' }))
    expect(acted(false).label).toBe('barred')
    expect(acted(true).label).toBe('let back')
    expect(externalAuditAction(row({ action: 'principal' }))).toBe('Person')
    const refused = externalAuditResult(
      row({ action: 'principal', approved: true, denied: 'cannot_grant_external' }),
    )
    expect(refused.color).toBe('error')
    expect(refused.label).toBe('may not grant this')
  })

  it('an action this build does not know keeps its word', () => {
    expect(externalAuditAction(row({ action: 'login' }))).toBe('Login')
    expect(externalAuditAction(row({ action: 'newer' }))).toBe('newer')
    expect(externalAuditResult(row({ action: 'newer' })).label).toBe('recorded')
  })
})
