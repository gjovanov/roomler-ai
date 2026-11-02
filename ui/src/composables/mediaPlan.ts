/**
 * FR-135 R2 — `rc:media-plan`: what the device promised this session, read and
 * put into words. Pure, so the wire and the wording are unit-tested.
 *
 * A device with a plan says it when the session starts and on every change. So
 * a black page can name its reason: the device said it stopped and why, or the
 * page knows what it was promised and that nothing came. An older agent sends
 * no plan, and its sessions read as they did.
 */

export interface RcMediaStep {
  from: string
  to: string
  why: string
  at_ms: number
}

export interface RcMediaPlan {
  epoch: number
  /** `webrtc`, or a `data-channel-*`. */
  transport: string
  codec: string
  chroma: string | null
  /** `name:chroma`, once an encoder opened. */
  cell: string | null
  input: { form: string; reason: string } | null
  fps: { asked: number | null; granted: number; reason: string | null } | null
  capture: { arm: string; form: string } | null
  ladder: RcMediaStep[]
  stopped: { why: string; detail: string } | null
}

const text = (v: unknown): string => (typeof v === 'string' ? v : '')
const finite = (v: unknown): number | null => (typeof v === 'number' && Number.isFinite(v) ? v : null)
const record = (v: unknown): Record<string, unknown> | null =>
  v && typeof v === 'object' && !Array.isArray(v) ? (v as Record<string, unknown>) : null

/** The plan in a parsed control message, or `null` when it is not one. Every
 *  field but the first three may be absent, and a newer agent may add more. */
export function parseMediaPlan(obj: Record<string, unknown>): RcMediaPlan | null {
  const epoch = finite(obj.epoch)
  if (epoch === null || typeof obj.transport !== 'string' || typeof obj.codec !== 'string') return null
  const input = record(obj.input)
  const fps = record(obj.fps)
  const granted = fps ? finite(fps.granted) : null
  const capture = record(obj.capture)
  const stopped = record(obj.stopped)
  const ladder = Array.isArray(obj.ladder) ? obj.ladder : []
  return {
    epoch,
    transport: obj.transport,
    codec: obj.codec,
    chroma: typeof obj.chroma === 'string' ? obj.chroma : null,
    cell: typeof obj.cell === 'string' ? obj.cell : null,
    input: input ? { form: text(input.form), reason: text(input.reason) } : null,
    fps: fps && granted !== null
      ? { asked: finite(fps.asked), granted, reason: typeof fps.reason === 'string' ? fps.reason : null }
      : null,
    capture: capture ? { arm: text(capture.arm), form: text(capture.form) } : null,
    ladder: ladder.flatMap((s) => {
      const step = record(s)
      return step && typeof step.from === 'string' && typeof step.to === 'string'
        ? [{ from: step.from, to: step.to, why: text(step.why), at_ms: finite(step.at_ms) ?? 0 }]
        : []
    }),
    stopped: stopped && typeof stopped.why === 'string' ? { why: stopped.why, detail: text(stopped.detail) } : null,
  }
}

/** Does this agent say its plan, and take `rc:viewer-hz`? Equality on the verb,
 *  as every verb is matched. */
export function agentSaysMediaPlan(rpc: readonly string[] | null | undefined): boolean {
  return rpc?.includes('media-plan') === true
}

/** Seconds with no picture before a page with a plan says what it was promised:
 *  the watchdog's probe, so the words come with the first thing it does about it. */
export const MEDIA_SILENT_TICKS = 6

/** Is this a session that never painted, past [`MEDIA_SILENT_TICKS`]? The first
 *  picture ends it, and nothing else does: it outlives the watchdog's re-create,
 *  or the words would blink every 10 s. A session that painted and then stalled
 *  keeps its frame on the screen and the watchdog's path. */
export function nextMediaSilent(
  prev: boolean,
  advanced: boolean,
  sessionPainted: boolean,
  planKnown: boolean,
  stallTicks: number,
): boolean {
  if (advanced) return false
  return prev || (!sessionPainted && planKnown && stallTicks >= MEDIA_SILENT_TICKS)
}

function codecName(codec: string): string {
  switch (codec) {
    case 'h264':
      return 'H.264'
    case 'h265':
    case 'hevc':
      return 'H.265'
    case 'av1':
      return 'AV1'
    case 'vp9':
      return 'VP9'
    default:
      return codec
  }
}

/** What the session was promised, in a phrase: `H.265 over the data channel`. */
export function describeMediaPromise(plan: Pick<RcMediaPlan, 'codec' | 'transport'>): string {
  const over = plan.transport === 'webrtc' ? 'WebRTC' : 'the data channel'
  return `${codecName(plan.codec)} over ${over}`
}

export interface MediaStopCopy {
  title: string
  body: string
  /** The device's word, shown as it came: an unknown one still says something. */
  code: string
  detail: string
}

/** A list in words: `a`, `a and b`, `a, b and c`. */
function listed(items: string[]): string {
  if (items.length < 2) return items.join('')
  return `${items.slice(0, -1).join(', ')} and ${items[items.length - 1]}`
}

/** The words for a device that said it stopped. */
export function describeMediaStop(plan: Pick<RcMediaPlan, 'stopped' | 'ladder'>): MediaStopCopy | null {
  const s = plan.stopped
  if (!s) return null
  // What fell to `none` and the arrows before it: every cell this open tried.
  const tried = [...new Set(plan.ladder.filter((l) => !l.from.endsWith(' fps') && l.from !== 'gpu' && l.from !== 'cpu').map((l) => l.from))]
  switch (s.why) {
    case 'no_encoder':
      return {
        title: 'This device could not start its video.',
        body: tried.length
          ? `It tried ${listed(tried)}, and none of them opened.`
          : 'It has no encoder it can open for this stream.',
        code: s.why,
        detail: s.detail,
      }
    case 'encoder_fault':
      return {
        title: "This device's video encoder stopped.",
        body: tried.length ? `${tried[tried.length - 1]} failed and would not restart.` : 'It failed and would not restart.',
        code: s.why,
        detail: s.detail,
      }
    default:
      return {
        title: 'This device stopped its video.',
        body: '',
        code: s.why,
        detail: s.detail,
      }
  }
}

export interface MediaPlanAction {
  /** The page's words: new ones, `null` to clear them, or `undefined` to leave
   *  them. A re-created session's first plan knows nothing yet about why the last
   *  one stopped, and clearing on it would blink the words every 10 s. */
  stop: MediaStopCopy | null | undefined
  /** The device is on the RTP track, while this page decodes a data channel
   *  nothing will arrive on: the black session. It has to show the track. */
  showTrack: boolean
}

/** What a page does with a plan it just read. `null` for one older than the
 *  newest this session has seen, which is dropped. */
export function mediaPlanAction(plan: RcMediaPlan, newestEpoch: number, dataChannelActive: boolean): MediaPlanAction | null {
  if (plan.epoch < newestEpoch) return null
  return {
    stop: plan.stopped ? describeMediaStop(plan) : plan.cell ? null : undefined,
    showTrack: plan.transport === 'webrtc' && dataChannelActive,
  }
}

/** One arrow, for the stats pill's title: `h264_nvenc → h264_qsv: no NVIDIA GPU`. */
export function describeMediaStep(step: RcMediaStep): string {
  return step.why ? `${step.from} → ${step.to}: ${step.why}` : `${step.from} → ${step.to}`
}

/** The stats pill: the last arrow, and how many there were. `''` with none. */
export function mediaLadderLabel(ladder: readonly RcMediaStep[]): string {
  const last = ladder[ladder.length - 1]
  if (!last) return ''
  const arrow = `${last.from} → ${last.to}`
  return ladder.length > 1 ? `${arrow} (${ladder.length} steps)` : arrow
}

export function viewerHzWireMessage(hz: number): { t: 'rc:viewer-hz'; hz: number } {
  return { t: 'rc:viewer-hz', hz }
}
