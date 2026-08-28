import { describe, it, expect } from 'vitest'
import {
  beginAttempt,
  CONNECT_SLOW_MS,
  describeConnectTiming,
  formatConnectTiming,
  STALL_SNACK_MIN_GAP_MS,
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

describe('FR-22 operator-facing connect verdict', () => {
  const marks = (o: Partial<Record<string, number>>) =>
    ({ attempt: 1, marks: o } as RcConnectTiming)

  it('says nothing about a normal connect', () => {
    // A notification on every successful connect is noise, and noise is
    // how the real signal gets ignored.
    const v = describeConnectTiming(marks({
      request_sent: 0, session_created: 40, ready: 60, offer_sent: 70,
      answer: 300, pc_connected: 1200, dc_open: 1400, first_frame: 3200,
    }))
    expect(v.notable).toBe(false)
  })

  it('names the dominant wait in plain words when a connect is slow', () => {
    const v = describeConnectTiming(marks({
      request_sent: 0, session_created: 40, ready: 60, offer_sent: 70,
      answer: 300, pc_connected: 9000, dc_open: 9200, first_frame: 11000,
    }))
    expect(v.notable).toBe(true)
    expect(v.color).toBe('warning')
    expect(v.text).toContain('11.0 s')
    expect(v.text).toContain('opening a network path to the device')
    // The operator gets meaning, not jargon.
    expect(v.text).not.toContain('pc_connected')
    expect(v.text).not.toContain('ICE')
  })

  it('never blames a human for taking their time over the consent prompt', () => {
    // `ready` is human-paced by design — the SERVER owns its timeout for
    // exactly that reason. Reporting "most of the wait was someone
    // approving" would be both true and useless, and would point the
    // operator at themselves instead of at the slow step.
    const v = describeConnectTiming(marks({
      request_sent: 0, session_created: 40,
      ready: 20000,          // a person took 20 s to click Allow
      offer_sent: 20010, answer: 20200, pc_connected: 21000,
      dc_open: 21200, first_frame: 26000,
    }))
    expect(v.text).not.toContain('approving')
    // The real dominant non-human wait is the last leg to first frame.
    expect(v.text).toContain('the first video frame arriving')
  })

  it('reports a retry even when the total looks acceptable', () => {
    // This is the FR-22 signature. One slow connect and a lost attempt
    // plus a fast retry are indistinguishable to everyone not reading
    // devtools — and telling them apart is the whole point.
    const v = describeConnectTiming({
      attempt: 2,
      marks: {
        request_sent: 0, session_created: 30, ready: 45, offer_sent: 50,
        answer: 200, pc_connected: 900, dc_open: 1100, first_frame: 2600,
      },
    })
    expect(v.notable).toBe(true)
    expect(v.text).toContain('after 1 failed attempt')
    expect(v.text).not.toContain('attempts')
  })

  it('pluralises multiple failed attempts', () => {
    const v = describeConnectTiming({
      attempt: 3,
      marks: { request_sent: 0, first_frame: 2600 },
    })
    expect(v.text).toContain('after 2 failed attempts')
  })

  it('leads with the stalled step on an abandoned attempt', () => {
    const v = describeConnectTiming(marks({
      request_sent: 0, session_created: 35, ready: 50, offer_sent: 55,
    }))
    expect(v.notable).toBe(true)
    expect(v.text).toContain('the device answering')
    expect(v.text).toContain('Retrying automatically')
    // The short technical name rides along so a reported snackbar is
    // traceable back to a mark without asking for devtools.
    expect(v.text).toContain('stalled at answer')
  })

  it('puts the slow threshold above the measured healthy band', () => {
    // Measured: request -> first pump heartbeat 2.5-4.7 s, plus decode.
    // A threshold inside that band would fire on healthy sessions and
    // train people to dismiss the message.
    expect(CONNECT_SLOW_MS).toBeGreaterThan(5000)
    // ...and below the 10-15 s actually being complained about.
    expect(CONNECT_SLOW_MS).toBeLessThan(10000)
  })
})

describe('FR-22 stall-warning throttle', () => {
  it('is long enough that a flapping path cannot bury its own message', () => {
    // The ladder abandons an attempt as fast as the `requesting` bound
    // allows, so the gap must exceed that or a flap storm produces one
    // snackbar per abandonment.
    expect(STALL_SNACK_MIN_GAP_MS).toBeGreaterThan(RC_REQUEST_TIMEOUT_MS)
  })
})
