// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * FR-52 P5 — how a row of the external-access log reads.
 *
 * A row is the SERVER's decision about someone from outside the org, never a
 * record of the session, which rides a path the server does not see. This is
 * the one place the refusal reasons are told apart: the outsider got one coarse
 * word, on purpose, and the org's admins get the reason.
 */

/** One row of `GET /tenant/{id}/external-rc-audit` (`ExternalRcAuditView`). */
export interface ExternalAuditEntry {
  id?: string | null
  /** `approve`, `rotate_code`, `login`, `session`, `principal`, or something newer. */
  action: string
  agent_id: string
  /** The admin on `approve`, `rotate_code` and `principal`, else the outsider. */
  user_id: string
  /** A name they chose, for the outsider (spec, decision 8). */
  actor: string
  approved?: boolean | null
  max_permissions?: string | null
  expires_at?: string | null
  at: string
  /** Why an admin's change was refused. */
  denied?: string | null
  attempt_id?: string
  /** Why a login or a session was refused. Absent means it went through. */
  login_refused?: string
  login_detail?: string
  session_id?: string
  /** `principal` rows: the person the admin acted on. */
  principal?: string
}

/** One row of `GET /tenant/{id}/external-principals`. */
export interface ExternalPrincipal {
  agent_id: string
  user_id: string
  /** What the account calls itself today: a claim. Null once the account is gone. */
  name?: string | null
  first_session_at: string
  last_session_at: string
  /** While set, this person is barred from this device. */
  revoked_at?: string | null
}

export interface ExternalAuditResult {
  label: string
  color: 'success' | 'error' | 'warning' | 'info'
  /** A sentence worth reading, if the row has one. */
  why: string | null
}

const REFUSED: Record<string, string> = {
  // An admin's change.
  not_device_admin: 'not a device admin',
  cannot_grant_external: 'may not grant this',
  device_unsupported: 'agent too old',
  // A login or a session.
  unavailable: 'refused',
  throttled: 'too many attempts',
  rate_limited: 'rate limited',
  busy: 'device busy',
  malformed: 'malformed',
  rejected: 'proof rejected',
  unknown_attempt: 'login not found',
  session_limit: 'no free session slot',
  other: 'refused',
}

const ACTION: Record<string, string> = {
  approve: 'Approval',
  rotate_code: 'Connect code',
  login: 'Login',
  session: 'Session',
  principal: 'Person',
}

export function externalAuditAction(e: ExternalAuditEntry): string {
  return ACTION[e.action] ?? e.action
}

/** Rows where `user_id` and `actor` are the person from outside. */
export function isOutsiderRow(e: ExternalAuditEntry): boolean {
  return e.action === 'login' || e.action === 'session'
}

/**
 * ⚠️ A refusal comes first, whatever the action says, and an unknown reason keeps
 * its raw word. A login row read past its refusal would say "verified" about a
 * stranger who was turned away.
 */
export function externalAuditResult(e: ExternalAuditEntry): ExternalAuditResult {
  const refused = e.denied ?? e.login_refused
  if (refused) {
    return { label: REFUSED[refused] ?? refused, color: 'error', why: e.login_detail ?? null }
  }
  switch (e.action) {
    case 'login':
      return { label: 'verified', color: 'success', why: null }
    case 'session':
      return { label: 'opened', color: 'success', why: null }
    case 'approve':
      return e.approved
        ? { label: 'approved', color: 'warning', why: approvalTerms(e) }
        : { label: 'withdrawn', color: 'info', why: null }
    case 'rotate_code':
      return { label: 'rotated', color: 'info', why: null }
    case 'principal':
      return e.approved
        ? { label: 'let back', color: 'warning', why: null }
        : { label: 'barred', color: 'info', why: null }
    default:
      return { label: 'recorded', color: 'info', why: null }
  }
}

function approvalTerms(e: ExternalAuditEntry): string | null {
  const terms = [e.max_permissions, e.expires_at && `until ${fmtWhen(e.expires_at)}`].filter(Boolean)
  return terms.length ? terms.join(', ') : null
}

export function fmtWhen(at: string): string {
  const d = new Date(at)
  return Number.isNaN(d.getTime()) ? at : d.toLocaleString()
}
