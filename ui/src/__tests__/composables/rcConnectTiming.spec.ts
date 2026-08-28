import { describe, it, expect } from 'vitest'
import {
  beginAttempt,
  formatConnectTiming,
  type RcConnectTiming,
} from '@/composables/rcConnectTiming'
import { signalingTimeoutFor, RC_REQUEST_TIMEOUT_MS, RC_SIGNALING_TIMEOUT_MS }
  from '@/composables/useRemoteControl'

describe('FR-22 signalling timeout table', () => {
  it('bounds the request hop far below the ICE hop', () => {
    // The whole defect: `requesting` waits on ONE server round-trip
    // (measured sub-second) but was guarded by the ICE-sized number, so
    // a lost request cost 15 s before a 250 ms retry succeeded normally.
    expect(signalingTimeoutFor('requesting')).toBe(RC_REQUEST_TIMEOUT_MS)
    expect(signalingTimeoutFor('negotiating')).toBe(RC_SIGNALING_TIMEOUT_MS)
    expect(RC_REQUEST_TIMEOUT_MS).toBeLessThan(RC_SIGNALING_TIMEOUT_MS)
  })

  it('keeps the bad case under the 8 s recovery budget', () => {
    // Acceptance criterion: a stalled attempt recovers in < 8 s.
    // request bound + first ladder step (250 ms) + a normal ~3 s connect.
    expect(RC_REQUEST_TIMEOUT_MS + 250 + 3000).toBeLessThan(8000)
  })

  it('leaves ICE generous enough for a relayed corp-VPN path', () => {
    // Measured healthy: request -> DC open in 0.95-2.25 s. The bound must
    // stay several times that, or a slow-but-working relay gets killed.
    expect(RC_SIGNALING_TIMEOUT_MS).toBeGreaterThanOrEqual(10000)
  })

  it('never arms on consent or the terminal phases', () => {
    // `awaiting_consent` is server-owned and human-paced: a client-side
    // number here would abandon a session a human is about to approve.
    expect(signalingTimeoutFor('awaiting_consent')).toBeNull()
    expect(signalingTimeoutFor('idle')).toBeNull()
    expect(signalingTimeoutFor('connected')).toBeNull()
    expect(signalingTimeoutFor('reconnecting')).toBeNull()
    expect(signalingTimeoutFor('closed')).toBeNull()
    expect(signalingTimeoutFor('error')).toBeNull()
  })
})

describe('FR-22 connect timing recorder', () => {
  it('records each mark once — a repeat must not restate a finished wait', () => {
    const r = beginAttempt(1)
    r.mark('request_sent')
    r.mark('session_created')
    const first = r.snapshot().marks.session_created
    r.mark('session_created')
    expect(r.snapshot().marks.session_created).toBe(first)
  })

  it('reports done() only after a frame actually painted', () => {
    const r = beginAttempt(1)
    r.mark('pc_connected')
    r.mark('dc_open')
    // Reaching `connected` is NOT the same as seeing the screen — the
    // whole report this FR exists for is about the wait after that.
    expect(r.done()).toBe(false)
    r.mark('first_frame')
    expect(r.done()).toBe(true)
  })

  it('formats a complete attempt as per-step deltas, not absolute offsets', () => {
    const t: RcConnectTiming = {
      attempt: 1,
      marks: {
        request_sent: 0,
        session_created: 40,
        ready: 60,
        offer_sent: 70,
        answer: 300,
        pc_connected: 1200,
        dc_open: 1400,
        first_frame: 3000,
      },
    }
    const line = formatConnectTiming(t)
    expect(line).toContain('attempt 1 ttff 3000ms')
    // The ICE wait is the long one here; a reader must see 900, not 1200.
    expect(line).toContain('pc_connected:+900')
    expect(line).toContain('answer:+230')
    expect(line).toContain('first_frame:+1600')
  })

  it('names the step an incomplete attempt was waiting on', () => {
    // This is the diagnostically valuable line: a MISSING mark is what
    // distinguishes a half-open agent WS from a cross-pod split.
    const t: RcConnectTiming = {
      attempt: 2,
      marks: { request_sent: 0, session_created: 35, ready: 50, offer_sent: 55 },
    }
    const line = formatConnectTiming(t)
    expect(line).toContain('attempt 2 INCOMPLETE (stalled waiting for answer)')
    // Unreached steps are shown, not omitted — the gap IS the finding.
    expect(line).toContain('answer:—')
    expect(line).toContain('first_frame:—')
  })

  it('distinguishes a slow single attempt from a lost one plus a fast retry', () => {
    // A fresh recorder per attempt is what makes this possible; sharing
    // one across the ladder would make an 18 s connect report as 3 s.
    const a = beginAttempt(1)
    a.mark('request_sent')
    const b = beginAttempt(2)
    b.mark('request_sent')
    b.mark('first_frame')
    expect(formatConnectTiming(a.snapshot())).toContain('attempt 1 INCOMPLETE')
    expect(formatConnectTiming(b.snapshot())).toContain('attempt 2 ttff')
  })
})
