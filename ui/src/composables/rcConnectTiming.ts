/**
 * FR-22 — time-to-first-frame instrumentation.
 *
 * The report this exists for is *"in some occasions it can take up to over
 * 10 or even 15 secs to see the remote screen"*. The agent side was measured
 * across ten consecutive sessions and is consistent (request → first pump
 * heartbeat: 2.5–4.7 s, no outliers), so the variance lives on the path
 * between the browser's request and its first painted frame — and there was
 * no browser-side timing at all. "Sometimes 10–15 s" cannot be turned into a
 * distribution, which means no fix here can be shown to work.
 *
 * ⚠️ The point of splitting the connect into marks rather than timing it
 * end-to-end: a single TTFF number tells you a connect was slow but not
 * WHICH wait was slow, and the three candidate causes (a half-open agent
 * control WS, a cross-pod split, a lost SDP frame on a reconnected WS) fail
 * in *different* phases. A total would have been cheaper to build and would
 * not have answered the question the FR is actually asking.
 *
 * ⚠️ Marks are recorded once per attempt and never overwritten. A retry
 * calls `beginAttempt()` and gets a fresh recorder, so the ladder's second
 * attempt cannot silently overwrite the first one's marks and make a
 * two-attempt connect look like one fast one — which is exactly the
 * measurement error that would hide the defect being hunted.
 */

/** The ordered waits a connect passes through. Each name says what is
 *  being WAITED ON, not which code assigns it, because the diagnostic
 *  question is always "who didn't answer". */
export type RcConnectMark =
  /** `rc:session.request` handed to the WS. */
  | 'request_sent'
  /** Server answered `rc:session.created` — it reached a live hub. */
  | 'session_created'
  /** `rc:ready` — consent settled and the agent is negotiating. */
  | 'ready'
  /** Our offer was sent. */
  | 'offer_sent'
  /** The agent's SDP answer arrived. */
  | 'answer'
  /** `RTCPeerConnection` reached `connected` (ICE + DTLS done). */
  | 'pc_connected'
  /** The `video-bytes` DataChannel opened (SCTP up). */
  | 'dc_open'
  /** First decoded frame painted. */
  | 'first_frame'

const MARK_ORDER: RcConnectMark[] = [
  'request_sent',
  'session_created',
  'ready',
  'offer_sent',
  'answer',
  'pc_connected',
  'dc_open',
  'first_frame',
]

export interface RcConnectTiming {
  /** 1-based attempt number within one user-initiated connect. */
  attempt: number
  /** ms since the attempt began, per mark. Missing = never reached. */
  marks: Partial<Record<RcConnectMark, number>>
}

export interface RcConnectRecorder {
  attempt: number
  mark(name: RcConnectMark): void
  /** Elapsed ms since the attempt began. */
  elapsed(): number
  snapshot(): RcConnectTiming
  /** True once `first_frame` has been marked — the attempt succeeded and
   *  further marks are noise from a later session on the same page. */
  done(): boolean
}

function now(): number {
  return typeof performance !== 'undefined' ? performance.now() : Date.now()
}

export function beginAttempt(attempt: number): RcConnectRecorder {
  const t0 = now()
  const marks: Partial<Record<RcConnectMark, number>> = {}
  return {
    attempt,
    mark(name) {
      // First writer wins. A duplicate `rc:ready` or a renegotiation must
      // not restate a wait that already completed.
      if (marks[name] === undefined) marks[name] = Math.round(now() - t0)
    },
    elapsed() {
      return Math.round(now() - t0)
    },
    snapshot() {
      return { attempt, marks: { ...marks } }
    },
    done() {
      return marks.first_frame !== undefined
    },
  }
}

/**
 * Render one attempt as a single log line: total, then the per-STEP cost
 * of each wait that was reached.
 *
 * Deltas rather than absolute offsets, because the actionable quantity is
 * "which wait was long", and absolute offsets make the reader subtract in
 * their head — the step after a 9 s stall looks equally late in absolute
 * terms even when it answered instantly.
 *
 * A wait that was never reached is reported as `<name>:—` rather than
 * omitted, since a MISSING mark is the most informative outcome there is:
 * it names the exact step that never completed.
 */
export function formatConnectTiming(t: RcConnectTiming): string {
  const parts: string[] = []
  let prev = 0
  let stalledAt: RcConnectMark | null = null
  for (const name of MARK_ORDER) {
    const at = t.marks[name]
    if (at === undefined) {
      if (stalledAt === null) stalledAt = name
      parts.push(`${name}:—`)
      continue
    }
    parts.push(`${name}:+${at - prev}`)
    prev = at
  }
  const total = t.marks.first_frame
  const head = total === undefined
    ? `attempt ${t.attempt} INCOMPLETE (stalled waiting for ${stalledAt})`
    : `attempt ${t.attempt} ttff ${total}ms`
  return `${head} — ${parts.join(' ')}`
}
