// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
import { describe, it, expect, vi } from 'vitest'
import { beginAttempt, type RcConnectTiming } from '@/composables/rcConnectTiming'
import {
  buildConnectTimingUpload,
  createConnectTimingUploader,
  RC_CONNECT_LOG_TARGET,
  RC_CONNECT_UPLOAD_MAX_PER_MIN,
  RC_CONNECT_UPLOAD_PATH,
  type RcConnectUploadContext,
} from '@/composables/rcConnectTimingUpload'

const TENANT = '69f0000000000000000000a1'
const SESSION = '69f0000000000000000000b2'
const AGENT = '69f0000000000000000000c3'
const OTHER_ORG = '69f0000000000000000000d4'

const painted: RcConnectTiming = {
  attempt: 1,
  afterDrop: false,
  hidden: false,
  marks: {
    ws_ready: 12,
    turn_ready: 76,
    probes_ready: 104,
    request_sent: 107,
    session_created: 150,
    ready: 203,
    offer_sent: 212,
    answer: 426,
    pc_connected: 1576,
    dc_open: 2364,
    first_frame: 2736,
  },
}

/** A `requesting` stall: the request went out and nothing answered. */
const stalledRequesting: RcConnectTiming = {
  attempt: 1,
  afterDrop: false,
  hidden: true,
  marks: { ws_ready: 12, turn_ready: 76, probes_ready: 104, request_sent: 107 },
}

function ctx(over: Partial<RcConnectUploadContext> = {}): RcConnectUploadContext {
  return {
    tenantId: TENANT,
    agentOrgId: TENANT,
    agentId: AGENT,
    sessionId: SESSION,
    outcome: 'first_frame',
    ...over,
  }
}

describe('FR-22 part 4 — the connect record', () => {
  it('is one browser-source line the ingest route can file: tenant, session, target, level', () => {
    const body = buildConnectTimingUpload(painted, ctx(), 1_758_826_800_000)
    expect(body).not.toBeNull()
    expect(body!.tenant_id).toBe(TENANT)
    expect(body!.source).toBe('browser')
    expect(body!.session_id).toBe(SESSION)
    expect(body!.lines).toHaveLength(1)
    const line = body!.lines[0]
    expect(line.target).toBe(RC_CONNECT_LOG_TARGET)
    expect(line.level).toBe('INFO')
    // Canonical extended JSON — the one shape bson::DateTime is locked to
    // accept on this route (agent_log.rs, browser_connect_timing_record_parses).
    expect(line.ts).toEqual({ $date: { $numberLong: '1758826800000' } })
    expect(line.msg).toBe(
      'attempt 1 ttff 2736ms — ws_ready:+12 turn_ready:+64 probes_ready:+28 request_sent:+3 '
      + 'session_created:+43 ready:+53 offer_sent:+9 answer:+214 pc_connected:+1150 dc_open:+788 first_frame:+372',
    )
  })

  it('carries the attempt, afterDrop, the outcome, the hidden flag, the ttff and every mark reached', () => {
    const f = buildConnectTimingUpload(painted, ctx(), 0)!.lines[0].fields
    expect(f.v).toBe(1)
    expect(f.outcome).toBe('first_frame')
    expect(f.attempt).toBe(1)
    expect(f.after_drop).toBe(false)
    expect(f.hidden).toBe(false)
    expect(f.stalled_at).toBeNull()
    expect(f.ttff_ms).toBe(2736)
    expect(f.agent_id).toBe(AGENT)
    expect(f.marks).toEqual(painted.marks)
    // Same org as the page: no cross-org field to mislead a miner.
    expect(f.agent_org_id).toBeUndefined()
  })

  it('names the phase a stall died in, and omits the session it never got', () => {
    // A request that no hub ever answered has no `rc:session.created`, so
    // no session id — which is exactly why such a stall leaves no server
    // row, and why this record is the only place its phase can be read.
    const body = buildConnectTimingUpload(stalledRequesting, ctx({ sessionId: null, outcome: 'abandoned' }), 0)!
    expect(body.session_id).toBeUndefined()
    expect('session_id' in body).toBe(false)
    const line = body.lines[0]
    expect(line.level).toBe('WARN')
    expect(line.msg).toContain('INCOMPLETE (stalled waiting for session_created)')
    expect(line.fields.outcome).toBe('abandoned')
    expect(line.fields.stalled_at).toBe('session_created')
    expect(line.fields.ttff_ms).toBeNull()
    // An unreached mark is ABSENT, never zero: its absence is the finding.
    expect(line.fields.marks).toEqual(stalledRequesting.marks)
    expect('session_created' in line.fields.marks).toBe(false)
  })

  it('flags a hidden tab so paint timing from it can be excluded', () => {
    // Standing rule (2026-09-07): no rAF in a hidden tab, so first_frame
    // there is not a paint anyone saw. The 2026-09-25 field read was
    // blocked by exactly such a tab; the record must make them filterable.
    const f = buildConnectTimingUpload({ ...painted, hidden: true }, ctx(), 0)!.lines[0].fields
    expect(f.hidden).toBe(true)
  })

  it('records the device org only when it differs from the page tenant, and the rc:error code when one ended the attempt', () => {
    const f = buildConnectTimingUpload(
      stalledRequesting,
      ctx({ agentOrgId: OTHER_ORG, sessionId: null, outcome: 'retried', errorCode: 'agent_offline' }),
      0,
    )!.lines[0].fields
    expect(f.agent_org_id).toBe(OTHER_ORG)
    expect(f.error_code).toBe('agent_offline')
    expect(f.outcome).toBe('retried')
  })

  it('files nothing without a tenant, and drops ids that are not ObjectId hex', () => {
    expect(buildConnectTimingUpload(painted, ctx({ tenantId: null }), 0)).toBeNull()
    expect(buildConnectTimingUpload(painted, ctx({ tenantId: 'not-a-tenant' }), 0)).toBeNull()
    const body = buildConnectTimingUpload(
      painted,
      ctx({ sessionId: 'sess_abc', agentId: 'https://evil.example/x' }),
      0,
    )!
    expect(body.session_id).toBeUndefined()
    expect(body.lines[0].fields.agent_id).toBeNull()
  })

  it('is timing metadata only — no URL, no token, no message text, by shape', () => {
    // The error code is the one free-text slot in the record. It is
    // constrained to the server's snake_case code vocabulary, so a field
    // that arrived carrying anything else is recorded as `other` rather
    // than forwarded — the record never trusts the sender for this.
    const body = buildConnectTimingUpload(painted, ctx({ errorCode: 'Bearer eyJhbGciOiJIUzI1NiJ9.x.y' }), 0)!
    const json = JSON.stringify(body)
    expect(json).not.toMatch(/https?:\/\//)
    expect(json).not.toContain('Bearer ')
    expect(json).not.toContain('eyJ')
    expect(body.lines[0].fields.error_code).toBe('other')
    expect(buildConnectTimingUpload(painted, ctx({ errorCode: 'agent_on_other_pod' }), 0)!.lines[0].fields.error_code)
      .toBe('agent_on_other_pod')
    expect(json.length).toBeLessThan(1200)
  })

  it('uses the recorder\'s own hidden flag, which starts from the tab state and is sticky', () => {
    const r = beginAttempt(1, false, false)
    expect(r.snapshot().hidden).toBe(false)
    r.noteHidden()
    expect(r.snapshot().hidden).toBe(true)
    const started = beginAttempt(2, false, true)
    expect(started.snapshot().hidden).toBe(true)
  })
})

describe('FR-22 part 4 — the uploader is best effort by construction', () => {
  type FetchLike = (input: string, init: RequestInit) => Promise<unknown>
  function makeFetch(impl?: () => Promise<unknown>) {
    const f: FetchLike = impl ?? (() => Promise.resolve({ ok: true, status: 201 }))
    return vi.fn<FetchLike>(f)
  }

  it('POSTs one record per attempt to the browser log route, keepalive, same-origin, no auth header', () => {
    const fetch = makeFetch()
    const up = createConnectTimingUploader({ fetch, now: () => 1_758_826_800_000 })
    up.send(painted, ctx())
    expect(fetch).toHaveBeenCalledTimes(1)
    const [url, init] = fetch.mock.calls[0]
    expect(url).toBe(RC_CONNECT_UPLOAD_PATH)
    expect(init.method).toBe('POST')
    expect(init.keepalive).toBe(true)
    expect(init.credentials).toBe('same-origin')
    expect(init.headers).toEqual({ 'Content-Type': 'application/json' })
    const body = JSON.parse(init.body as string)
    expect(body.source).toBe('browser')
    expect(body.lines[0].target).toBe(RC_CONNECT_LOG_TARGET)
    expect(up.sentCount()).toBe(1)
  })

  it('never throws and never surfaces a rejected POST', async () => {
    const errors: unknown[] = []
    const onRejection = (e: PromiseRejectionEvent) => { errors.push(e.reason); e.preventDefault() }
    globalThis.addEventListener?.('unhandledrejection', onRejection)
    try {
      const rejecting = createConnectTimingUploader({ fetch: makeFetch(() => Promise.reject(new Error('offline'))) })
      expect(() => rejecting.send(painted, ctx())).not.toThrow()
      const throwing = createConnectTimingUploader({
        fetch: vi.fn<FetchLike>(() => { throw new TypeError('Failed to fetch') }),
      })
      expect(() => throwing.send(painted, ctx())).not.toThrow()
      const refused = createConnectTimingUploader({ fetch: makeFetch(() => Promise.resolve({ ok: false, status: 403 })) })
      expect(() => refused.send(painted, ctx())).not.toThrow()
      // Let the settlements run; nothing may have escaped.
      await new Promise((r) => setTimeout(r, 0))
      expect(errors).toEqual([])
    } finally {
      globalThis.removeEventListener?.('unhandledrejection', onRejection)
    }
  })

  it('does not retry a failed POST — one attempt, one request', async () => {
    const fetch = makeFetch(() => Promise.reject(new Error('offline')))
    const up = createConnectTimingUploader({ fetch })
    up.send(painted, ctx())
    await new Promise((r) => setTimeout(r, 0))
    expect(fetch).toHaveBeenCalledTimes(1)
  })

  it('caps itself under the per-IP governor: a flapping ladder drops records rather than throttling the API', () => {
    let t = 0
    const fetch = makeFetch()
    const up = createConnectTimingUploader({ fetch, now: () => t })
    for (let i = 0; i < RC_CONNECT_UPLOAD_MAX_PER_MIN + 5; i++) {
      t += 1000
      up.send(stalledRequesting, ctx({ sessionId: null, outcome: 'abandoned' }))
    }
    expect(fetch).toHaveBeenCalledTimes(RC_CONNECT_UPLOAD_MAX_PER_MIN)
    expect(RC_CONNECT_UPLOAD_MAX_PER_MIN).toBeLessThan(60)
    // The window slides: a minute later the cap is available again.
    t += 60_000
    up.send(painted, ctx())
    expect(fetch).toHaveBeenCalledTimes(RC_CONNECT_UPLOAD_MAX_PER_MIN + 1)
  })

  it('sends nothing when there is no tenant to file under, and does not count it against the cap', () => {
    const fetch = makeFetch()
    const up = createConnectTimingUploader({ fetch })
    up.send(painted, ctx({ tenantId: null }))
    expect(fetch).not.toHaveBeenCalled()
    expect(up.sentCount()).toBe(0)
  })

  it('does nothing where there is no fetch at all', () => {
    const saved = globalThis.fetch
    // @ts-expect-error — simulating an environment with no fetch
    globalThis.fetch = undefined
    try {
      const up = createConnectTimingUploader()
      expect(() => up.send(painted, ctx())).not.toThrow()
      expect(up.sentCount()).toBe(0)
    } finally {
      globalThis.fetch = saved
    }
  })
})
