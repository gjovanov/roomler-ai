// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// #3582 review — the load after a reload for a new build resumes the session
// that reload took over THROUGH THE REDIAL'S LADDER. A first connect starts
// the ladder at 0, and `rc:error` retries `agent_offline` / `agent_busy` only
// past it, so a tab that reloaded while its device restarted for an update
// stopped on the first `agent_offline`, where its redial kept trying.
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { ref } from 'vue'
import { createPinia, setActivePinia } from 'pinia'
import { useRemoteControl } from '@/composables/useRemoteControl'
import { useWsStore } from '@/stores/ws'

const AGENT = '69f0000000000000000000c3'
const TENANT = '69f0000000000000000000a1'
const USER = '69f0000000000000000000b7'
const ARGS = { agentId: AGENT, permissions: 'VIEW | INPUT | CLIPBOARD | FILES | RECORD', orgId: TENANT }

beforeEach(() => {
  setActivePinia(createPinia())
  vi.useFakeTimers()
})

afterEach(() => {
  vi.useRealTimers()
})

describe('resumeAfterReload (#3582)', () => {
  it('puts the session on the redial ladder: past attempt 0, reconnecting, with the args its redial uses', () => {
    const rc = useRemoteControl(ref(null), { userId: () => USER })
    expect(rc.phase.value).toBe('idle')
    rc.resumeAfterReload(ARGS)
    expect(rc.phase.value).toBe('reconnecting')
    // Past 0: `agent_offline` / `agent_busy` are retried, not terminal.
    expect(rc.reconnectAttempt.value).toBeGreaterThanOrEqual(1)
    expect(rc.reloadSurface.resumeArgs()).toEqual(ARGS)
    expect(rc.reloadSurface.userId()).toBe(USER)
    // The socket is still down at this point of a page load: the ladder waits
    // for it, as a redial does, rather than failing the session.
    expect(rc.error.value).toBeNull()
    expect(rc.reloadSurface.state().session).toBe('redialing')
  })

  it('never for an external session, and never over one already under way', () => {
    const external = useRemoteControl(ref(null), { external: true })
    external.resumeAfterReload(ARGS)
    expect(external.phase.value).toBe('idle')
    expect(external.reloadSurface.resumeArgs()).toBeNull()

    const rc = useRemoteControl(ref(null), { userId: () => USER })
    rc.resumeAfterReload(ARGS)
    const attempt = rc.reconnectAttempt.value
    rc.resumeAfterReload({ ...ARGS, agentId: '69f0000000000000000000ff' })
    expect(rc.reconnectAttempt.value).toBe(attempt)
    expect(rc.reloadSurface.resumeArgs()).toEqual(ARGS)
  })

  it('a held redial does not advance; given back, the ladder runs again with its backoff', () => {
    const ws = useWsStore()
    const sendRaw = vi.fn()
    ws.sendRaw = sendRaw
    const rc = useRemoteControl(ref(null), { userId: () => USER })
    rc.resumeAfterReload(ARGS)
    // Unheld, the ladder's timer fires and steps the counter (the socket is down here).
    const before = rc.reconnectAttempt.value
    vi.advanceTimersByTime(60_000)
    expect(rc.reconnectAttempt.value).toBeGreaterThan(before)

    expect(rc.reloadSurface.holdRedial()).toEqual(ARGS)
    expect(rc.reloadSurface.state().session).toBe('redialing')
    const held = rc.reconnectAttempt.value
    vi.advanceTimersByTime(10 * 60_000)
    expect(rc.reconnectAttempt.value).toBe(held)
    expect(sendRaw).not.toHaveBeenCalled()

    rc.reloadSurface.releaseRedial()
    expect(rc.phase.value).toBe('reconnecting')
    expect(rc.reconnectAttempt.value).toBe(held + 1)
    expect(rc.reloadSurface.state().session).toBe('redialing')
  })
})
