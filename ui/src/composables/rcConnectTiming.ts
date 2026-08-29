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
  /** The signalling socket was open (or the pre-flight gave up waiting).
   *  ⚠️ This runs BEFORE the request is sent and can legitimately take
   *  seconds — the socket is re-keyed and redialled when the device's org
   *  differs from the page's. Leaving it unmeasured was a real defect:
   *  a connect that spent its whole wait here reported a small TTFF and
   *  looked healthy. */
  | 'ws_ready'
  /** TURN credentials fetched (an HTTP round-trip to the API). */
  | 'turn_ready'
  /** Local-relay probe + the browser's codec-capability probes finished. */
  | 'probes_ready'
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
  'ws_ready',
  'turn_ready',
  'probes_ready',
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
  /** True when an EARLIER attempt in this connect cycle already painted a
   *  frame — i.e. the session worked and then dropped, rather than the
   *  request never being answered.
   *
   *  ⚠️ Field-found. Without this, `attempt > 1` was reported as "the
   *  first request went unanswered", which is one cause among several and
   *  was FALSE in the first session this was tested on: attempt 1 painted
   *  at 1734 ms, the agent's DERP control WS then dropped, and the ladder
   *  reconnected on attempt 6. The message named a cause the data did not
   *  establish — the same "confidently wrong rather than absent" failure
   *  the consent rule below exists to prevent. */
  afterDrop: boolean
  /** ms since the attempt began, per mark. Missing = never reached. */
  marks: Partial<Record<RcConnectMark, number>>
}

export interface RcConnectRecorder {
  attempt: number
  afterDrop: boolean
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

export function beginAttempt(attempt: number, afterDrop = false): RcConnectRecorder {
  const t0 = now()
  const marks: Partial<Record<RcConnectMark, number>> = {}
  return {
    attempt,
    afterDrop,
    mark(name) {
      // First writer wins. A duplicate `rc:ready` or a renegotiation must
      // not restate a wait that already completed.
      if (marks[name] === undefined) marks[name] = Math.round(now() - t0)
    },
    elapsed() {
      return Math.round(now() - t0)
    },
    snapshot() {
      return { attempt, afterDrop, marks: { ...marks } }
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

/**
 * What each wait means in words an operator can act on.
 *
 * ⚠️ These are deliberately NOT the mark names. The console line is for
 * whoever is debugging the transport; this table is for the person
 * looking at a blank stage wondering whether to click Connect again, and
 * "ICE" or "SCTP" tells them nothing. The short technical name still
 * rides along in the message tail so a reported snackbar is traceable
 * back to a mark without asking the operator to open devtools.
 */
const WAIT_LABEL: Record<RcConnectMark, string> = {
  ws_ready: 'connecting to the signalling server',
  turn_ready: 'fetching relay credentials',
  probes_ready: 'checking what this browser can decode',
  request_sent: 'sending the request',
  session_created: 'the server accepting the session',
  ready: 'someone at the device approving the session',
  offer_sent: 'preparing the video offer',
  answer: 'the device answering',
  pc_connected: 'opening a network path to the device',
  dc_open: 'opening the video channel',
  first_frame: 'the first video frame arriving',
}

/**
 * A wait whose length is a HUMAN's choice, not a fault. Consent is
 * human-paced by design — the server owns its timeout precisely because
 * a person may take as long as they like — so it must never be reported
 * as "what was slow". Blaming an operator's own approval for a slow
 * connect would be both wrong and actively misleading.
 */
const HUMAN_PACED: ReadonlySet<RcConnectMark> = new Set<RcConnectMark>(['ready'])

/** At or under this, a connect is normal and needs no explanation. The
 *  measured healthy path is 2.5-4.7 s agent-side plus decode, so this
 *  sits above the healthy band and below the 10-15 s being complained
 *  about — a threshold inside the healthy band would train people to
 *  ignore the message, which is worse than not showing it. */
export const CONNECT_SLOW_MS = 7000

/** Minimum gap between two stall warnings. A flapping path abandons
 *  an attempt every few seconds; without this the operator gets a
 *  snackbar queue instead of a message. Long enough that repeats are
 *  rare, short enough that a genuinely new stall still reports. */
export const STALL_SNACK_MIN_GAP_MS = 20000

export interface RcConnectVerdict {
  /** One sentence for the operator. */
  text: string
  /** Vuetify snackbar colour. */
  color: 'info' | 'warning'
  /** False when this connect is unremarkable and the operator should be
   *  left alone. A notification for every successful connect is noise,
   *  and noise is how a real signal gets ignored. */
  notable: boolean
}

function secs(ms: number): string {
  return `${(ms / 1000).toFixed(1)} s`
}

/**
 * Turn one attempt's marks into something worth showing a person.
 *
 * The question this answers is the operator's, not the debugger's:
 * *why did that take so long, and is it my fault or the system's?*
 * So the verdict always names WHICH wait dominated, in plain words —
 * "opening a network path" and "the device answering" have completely
 * different fixes, and until now both looked identical from the outside.
 */
export function describeConnectTiming(t: RcConnectTiming): RcConnectVerdict {
  const total = t.marks.first_frame

  // Incomplete: the attempt was abandoned or cancelled. The step that
  // never completed IS the finding, so lead with it.
  if (total === undefined) {
    const stalled = MARK_ORDER.find((m) => t.marks[m] === undefined)
    const what = stalled ? WAIT_LABEL[stalled] : 'connecting'
    return {
      text: `Reconnecting - the session stalled while waiting for ${what}. `
        + `Retrying automatically. (stalled at ${stalled ?? 'unknown'})`,
      color: 'warning',
      notable: true,
    }
  }

  // Find the dominant wait, ignoring human-paced ones.
  let worst: RcConnectMark | null = null
  let worstMs = 0
  let prev = 0
  for (const name of MARK_ORDER) {
    const at = t.marks[name]
    if (at === undefined) continue
    const step = at - prev
    prev = at
    if (HUMAN_PACED.has(name)) continue
    if (step > worstMs) {
      worstMs = step
      worst = name
    }
  }

  // A retry happened. Worth saying even when the total looks fine: the
  // operator waited through a lost attempt, and knowing an attempt was
  // lost is exactly the evidence FR-22's root cause needs. Without this
  // the two cases - one slow connect, and a lost one plus a fast retry -
  // are indistinguishable to everyone except someone reading devtools.
  if (t.attempt > 1) {
    const n = t.attempt - 1
    const tries = `${n} failed attempt${n > 1 ? 's' : ''}`
    // ⚠️ State only what the marks establish. A retry has (at least) two
    // causes that look identical in the attempt counter: a request that
    // was never answered, and a session that WORKED and then dropped.
    // `afterDrop` is the only thing that can tell them apart, and naming
    // the wrong one sends the reader after the wrong bug.
    return {
      text: t.afterDrop
        ? `Reconnected in ${secs(total)} after ${tries}. The session `
          + `dropped and was restored automatically.`
        : `Connected in ${secs(total)} after ${tries}.`,
      color: 'warning',
      notable: true,
    }
  }

  if (total <= CONNECT_SLOW_MS) {
    return { text: `Connected in ${secs(total)}.`, color: 'info', notable: false }
  }

  if (!worst) {
    return {
      text: `Connected in ${secs(total)} - slower than usual.`,
      color: 'warning',
      notable: true,
    }
  }

  return {
    text: `Connected in ${secs(total)} - slower than usual. Most of the wait `
      + `was ${WAIT_LABEL[worst]} (${secs(worstMs)}).`,
    color: 'warning',
    notable: true,
  }
}
