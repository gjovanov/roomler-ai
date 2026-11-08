// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// #3582 — the viewer, as a reload for a new build sees it. `redialing` is the
// one state a reload may take over: the session already cut, the ladder's
// timer waiting, nothing in flight. Everything else that holds a session is
// `live`, and a reload never happens in the middle of one. A session that
// ended a moment ago holds the page: its notice is what the person reads.
import { describe, expect, it } from 'vitest'
import { RC_ENDED_HOLD_MS, rcReloadState, type RcPhase } from '@/composables/useRemoteControl'

const base = { redialPending: true, hasArgs: true, external: false, work: false, endedRecently: false, asksConsent: false }

describe('rcReloadState (#3582)', () => {
  it('no session: idle, ended, failed', () => {
    for (const phase of ['idle', 'closed', 'error'] as RcPhase[]) {
      expect(rcReloadState({ ...base, phase }).session).toBe('none')
    }
  })

  it('redialing: cut, the ladder waiting, the args known', () => {
    expect(rcReloadState({ ...base, phase: 'reconnecting' }).session).toBe('redialing')
  })

  it('a request or a consent prompt in flight is live: a reload would abandon a question the host may be reading', () => {
    for (const phase of ['requesting', 'awaiting_consent', 'negotiating', 'connected'] as RcPhase[]) {
      expect(rcReloadState({ ...base, phase }).session).toBe('live')
    }
  })

  it('reconnecting with no timer waiting, or without the args to resume, is live, not redialing', () => {
    expect(rcReloadState({ ...base, phase: 'reconnecting', redialPending: false }).session).toBe('live')
    expect(rcReloadState({ ...base, phase: 'reconnecting', hasArgs: false }).session).toBe('live')
  })

  it('an external session (FR-52) is never redialing: it would need the device password again', () => {
    const s = rcReloadState({ ...base, phase: 'reconnecting', external: true })
    expect(s.session).toBe('live')
    expect(s.external).toBe(true)
  })

  it('carries the work only this page holds', () => {
    expect(rcReloadState({ ...base, phase: 'reconnecting', work: true }).work).toBe(true)
  })

  it('a session that ended a moment ago holds the page; a live one does not need to', () => {
    expect(rcReloadState({ ...base, phase: 'closed', endedRecently: true }).hold).toBe(true)
    expect(rcReloadState({ ...base, phase: 'error', endedRecently: true }).hold).toBe(true)
    expect(rcReloadState({ ...base, phase: 'closed', endedRecently: false }).hold).toBe(false)
    // A live session is held by being live; `hold` is about what an END leaves on the page.
    expect(rcReloadState({ ...base, phase: 'connected', endedRecently: true }).hold).toBe(false)
    expect(RC_ENDED_HOLD_MS).toBe(5 * 60_000)
  })

  it('says whether the device asked someone to allow the session', () => {
    expect(rcReloadState({ ...base, phase: 'connected', asksConsent: true }).asksConsent).toBe(true)
    expect(rcReloadState({ ...base, phase: 'connected' }).asksConsent).toBe(false)
  })
})
