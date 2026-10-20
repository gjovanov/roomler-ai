// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// #1631 — the two composable-side guards behind "the remote view follows the
// device you pick": what an inbound `rc:session.created` means to the
// composable that receives it, and the module-level terminator for a create
// that answers a request whose composable is already gone. Both are pure
// (the registry takes its socket as a parameter), so they lock the wire
// behaviour without the WS store.
import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest'
import {
  sessionCreatedAction,
  createAbandonedRequestRegistry,
  RC_LATE_CREATE_WINDOW_MS,
  RC_REQUEST_TIMEOUT_MS,
  type AbandonedRequestIo,
} from '@/composables/useRemoteControl'

describe('sessionCreatedAction (#1631)', () => {
  const created = (session_id: string, agent_id?: string) =>
    agent_id === undefined ? { session_id } : { session_id, agent_id }

  it("while requesting, a create for ANOTHER agent is terminated — the old inline logic adopted it (A's late create landing in B's viewer)", () => {
    expect(sessionCreatedAction('requesting', created('s1', 'agentA'), null, 'agentB', false)).toBe('terminate')
  })

  it('while requesting, a create for the requested agent is adopted', () => {
    expect(sessionCreatedAction('requesting', created('s1', 'agentB'), null, 'agentB', false)).toBe('adopt')
  })

  it('while requesting, a create with no agent_id is adopted (pre-field servers; refuse only what is provably foreign)', () => {
    expect(sessionCreatedAction('requesting', created('s1'), null, 'agentB', false)).toBe('adopt')
    expect(sessionCreatedAction('requesting', { session_id: 's1', agent_id: '' }, null, 'agentB', false)).toBe('adopt')
  })

  it('while requesting with no known requested agent, a create is adopted rather than terminated (fail-open, as before)', () => {
    expect(sessionCreatedAction('requesting', created('s1', 'agentA'), null, null, false)).toBe('adopt')
  })

  it('outside requesting, the #1045 coalesce echo for the tracked session is ignored, never terminated', () => {
    expect(sessionCreatedAction('awaiting_consent', created('s1', 'agentB'), 's1', 'agentB', false)).toBe('ignore')
    expect(sessionCreatedAction('connected', created('s1', 'agentB'), 's1', 'agentB', false)).toBe('ignore')
  })

  it('outside requesting, any other create is a ghost and is terminated', () => {
    expect(sessionCreatedAction('awaiting_consent', created('s2', 'agentB'), 's1', 'agentB', false)).toBe('terminate')
    expect(sessionCreatedAction('idle', created('s1', 'agentA'), null, null, false)).toBe('terminate')
    expect(sessionCreatedAction('closed', created('s1', 'agentA'), null, null, false)).toBe('terminate')
    expect(sessionCreatedAction('error', created('s1', 'agentA'), null, null, false)).toBe('terminate')
    expect(sessionCreatedAction('reconnecting', created('s1', 'agentA'), null, 'agentA', false)).toBe('terminate')
  })

  it('a create with no session id outside requesting is not mistaken for the echo of an untracked session', () => {
    expect(sessionCreatedAction('idle', {}, null, null, false)).toBe('terminate')
  })
})

// FR-52 P4f field run (2026-10-20, prod, connect code 21ZC-RE79-BYTM): a real
// outsider's external session was torn down by this composable on its OWN
// create, every single time (3/3), logged as "releasing the ghost session" —
// then looped forever retrying. An external session's agent_id is always the
// all-zero placeholder (crates/modules/fleet/src/hub.rs — an outsider never
// learns the device's real id), while the client's own requestedAgentId for
// an external request is `ext:<connectCode>` (connectExternal(), this file).
// Those two strings can never be equal, so the pre-fix foreign-agent guard
// terminated every legitimate external session unconditionally.
describe('sessionCreatedAction — external sessions (FR-52 field bug)', () => {
  const ZERO_AGENT_ID = '000000000000000000000000'

  it('an external create (zero-placeholder agent_id) while requesting is adopted, not terminated', () => {
    expect(
      sessionCreatedAction(
        'requesting',
        { session_id: 's1', agent_id: ZERO_AGENT_ID },
        null,
        'ext:21ZC-RE79-BYTM',
        true,
      ),
    ).toBe('adopt')
  })

  it('negative control: the identical create is terminated when isExternal is false — proving the fix, not the zero id alone, is what adopts it', () => {
    expect(
      sessionCreatedAction(
        'requesting',
        { session_id: 's1', agent_id: ZERO_AGENT_ID },
        null,
        'ext:21ZC-RE79-BYTM',
        false,
      ),
    ).toBe('terminate')
  })

  it('outside requesting, external sessions keep the #1045 coalesce-echo/ghost rules unchanged (keyed by session_id, never agent_id)', () => {
    expect(
      sessionCreatedAction(
        'awaiting_consent',
        { session_id: 's1', agent_id: ZERO_AGENT_ID },
        's1',
        'ext:21ZC-RE79-BYTM',
        true,
      ),
    ).toBe('ignore')
    expect(
      sessionCreatedAction(
        'awaiting_consent',
        { session_id: 's2', agent_id: ZERO_AGENT_ID },
        's1',
        'ext:21ZC-RE79-BYTM',
        true,
      ),
    ).toBe('terminate')
  })
})

/** A fake of the WS-store slice the registry uses: records subscriptions,
 *  lets a test deliver a message, and records what was sent. */
function fakeIo() {
  const handlers = new Map<string, Set<(msg: any) => void>>()
  const sent: Array<Record<string, unknown>> = []
  let unsubCalls = 0
  const io: AbandonedRequestIo = {
    onRcMessage(t, handler) {
      let set = handlers.get(t)
      if (!set) {
        set = new Set()
        handlers.set(t, set)
      }
      set.add(handler)
      return () => {
        unsubCalls += 1
        set!.delete(handler)
      }
    },
    sendRaw(msg) {
      sent.push(msg as Record<string, unknown>)
    },
  }
  return {
    io,
    sent,
    get unsubCalls() {
      return unsubCalls
    },
    handlerCount(t: string) {
      return handlers.get(t)?.size ?? 0
    },
    deliver(t: string, msg: unknown) {
      for (const h of [...(handlers.get(t) ?? [])]) h(msg)
    },
  }
}

describe('createAbandonedRequestRegistry (#1631)', () => {
  beforeEach(() => {
    vi.useFakeTimers()
    vi.spyOn(console, 'warn').mockImplementation(() => {})
  })
  afterEach(() => {
    vi.useRealTimers()
    vi.restoreAllMocks()
  })

  it('the window is twice the request timeout', () => {
    expect(RC_LATE_CREATE_WINDOW_MS).toBe(2 * RC_REQUEST_TIMEOUT_MS)
  })

  it('arm → the late create for that agent is terminated with controller_hangup, then the entry disarms itself', () => {
    const f = fakeIo()
    const reg = createAbandonedRequestRegistry(f.io)
    reg.arm('agentA')
    expect(reg.size).toBe(1)
    expect(f.handlerCount('rc:session.created')).toBe(1)

    f.deliver('rc:session.created', { session_id: 'sA', agent_id: 'agentA', permissions: 'VIEW' })
    expect(f.sent).toEqual([{ t: 'rc:terminate', session_id: 'sA', reason: 'controller_hangup' }])
    expect(reg.size).toBe(0)
    expect(f.unsubCalls).toBe(1)
    expect(f.handlerCount('rc:session.created')).toBe(0)

    // A second create (the hub re-affirming, or a retry) finds nobody home.
    f.deliver('rc:session.created', { session_id: 'sA2', agent_id: 'agentA' })
    expect(f.sent).toHaveLength(1)
  })

  it('a create for a different agent is not the one it waits for', () => {
    const f = fakeIo()
    const reg = createAbandonedRequestRegistry(f.io)
    reg.arm('agentA')
    f.deliver('rc:session.created', { session_id: 'sB', agent_id: 'agentB' })
    expect(f.sent).toEqual([])
    expect(reg.size).toBe(1)
  })

  it('arm → connect() disarms before it re-requests → the create is NOT terminated (A→B→A keeps the coalesced session)', () => {
    const f = fakeIo()
    const reg = createAbandonedRequestRegistry(f.io)
    reg.arm('agentA')
    reg.disarm('agentA')
    expect(reg.size).toBe(0)
    expect(f.unsubCalls).toBe(1)
    f.deliver('rc:session.created', { session_id: 'sA', agent_id: 'agentA' })
    expect(f.sent).toEqual([])
  })

  it('disarm of an agent that is not armed is a no-op', () => {
    const f = fakeIo()
    const reg = createAbandonedRequestRegistry(f.io)
    reg.disarm('agentZ')
    expect(reg.size).toBe(0)
    expect(f.unsubCalls).toBe(0)
  })

  it('an entry expires after the window: unsubscribed, and a create after that is left alone', () => {
    const f = fakeIo()
    const reg = createAbandonedRequestRegistry(f.io, RC_LATE_CREATE_WINDOW_MS)
    reg.arm('agentA')
    vi.advanceTimersByTime(RC_LATE_CREATE_WINDOW_MS - 1)
    expect(reg.size).toBe(1)
    vi.advanceTimersByTime(1)
    expect(reg.size).toBe(0)
    expect(f.unsubCalls).toBe(1)
    f.deliver('rc:session.created', { session_id: 'sA', agent_id: 'agentA' })
    expect(f.sent).toEqual([])
  })

  it('re-arming the same agent replaces the entry (one subscription, a fresh window)', () => {
    const f = fakeIo()
    const reg = createAbandonedRequestRegistry(f.io, 1000)
    reg.arm('agentA')
    vi.advanceTimersByTime(800)
    reg.arm('agentA')
    expect(reg.size).toBe(1)
    expect(f.unsubCalls).toBe(1)
    expect(f.handlerCount('rc:session.created')).toBe(1)
    vi.advanceTimersByTime(800)
    expect(reg.size).toBe(1) // the second arm's window is still open
    vi.advanceTimersByTime(200)
    expect(reg.size).toBe(0)
  })

  it('a create without a session id is dropped without a terminate, and still consumes the entry', () => {
    const f = fakeIo()
    const reg = createAbandonedRequestRegistry(f.io)
    reg.arm('agentA')
    f.deliver('rc:session.created', { agent_id: 'agentA' })
    expect(f.sent).toEqual([])
    expect(reg.size).toBe(0)
  })

  it('two registries over one entries map see each other (the composable that armed is gone when the next one disarms)', () => {
    const entries = new Map()
    const f = fakeIo()
    const first = createAbandonedRequestRegistry(f.io, RC_LATE_CREATE_WINDOW_MS, entries)
    const second = createAbandonedRequestRegistry(f.io, RC_LATE_CREATE_WINDOW_MS, entries)
    first.arm('agentA')
    expect(second.size).toBe(1)
    second.disarm('agentA')
    expect(first.size).toBe(0)
    f.deliver('rc:session.created', { session_id: 'sA', agent_id: 'agentA' })
    expect(f.sent).toEqual([])
  })
})
