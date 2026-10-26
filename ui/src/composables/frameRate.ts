// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * FR-119 P1 — the frame rate: what the viewer may ask for, and what the agent
 * answered.
 *
 * The viewer asks with `rc:fps {fps}` on the control channel, and only of an
 * agent that advertises `fps-high` in `AgentCaps.video`. The agent answers in
 * `rc:video-info`: `fps` is what the stream runs at, and when it did not grant
 * the ask, `fps_asked` and `fps_reason` say what was asked and why not. An agent
 * without `fps-high` answers nothing new, and the picker shows why above-60 is
 * unavailable rather than offering something that will never be honoured.
 *
 * Pure on purpose (no Vue, no channel), so all of it is unit-tested.
 */

/** The capability verb in `AgentCaps.video`. ⚠️ A wire spelling, matched by
 *  equality on both ends. */
export const FPS_CAP = 'fps-high'

/** FR-119 P2b-17b — the agent acts on `rc:fps {retry: true}`: it forgets its
 *  verdict for the rate asked for and trials it again. ⚠️ A wire spelling. */
export const FPS_RETRY_CAP = 'fps-retry'

/** The rates the picker offers. 60 is today's rate on every session. */
export const FRAME_RATES = [60, 120, 144] as const
export type FrameRate = (typeof FRAME_RATES)[number]

/** Where the viewer's pick is remembered, per browser. */
export const FRAME_RATE_STORAGE_KEY = 'roomler-rc-fps'

export function agentOffersHighRefresh(video: readonly string[] | undefined | null): boolean {
  return (video ?? []).includes(FPS_CAP)
}

/** FR-119 P2b-17b — whether this agent can be asked to trial a refused rate
 *  again. An older one ignores the field, so it is never offered there. */
export function agentOffersFrameRateRetry(video: readonly string[] | undefined | null): boolean {
  return (video ?? []).includes(FPS_RETRY_CAP)
}

export interface FrameRateOption {
  fps: FrameRate
  enabled: boolean
  /** Why it is disabled, in words; `null` when it is not. */
  why: string | null
}

/** The picker's options. 60 is always available. Above 60 needs the agent's
 *  `fps-high`, and, when the host reported its screen's refresh rate, a screen
 *  at least that fast: a screen cannot produce frames faster than it refreshes. */
export function frameRateOptions(
  video: readonly string[] | undefined | null,
  hostRefreshHz: number | null,
): FrameRateOption[] {
  const high = agentOffersHighRefresh(video)
  return FRAME_RATES.map((fps) => {
    if (fps === 60) return { fps, enabled: true, why: null }
    if (!high) return { fps, enabled: false, why: "this device's agent streams at most 60 fps" }
    if (hostRefreshHz !== null && hostRefreshHz > 0 && hostRefreshHz < fps) {
      return { fps, enabled: false, why: `the host's screen refreshes at ${hostRefreshHz} Hz` }
    }
    return { fps, enabled: true, why: null }
  })
}

/** The control-channel ask; `retry` asks the agent to trial the rate again
 *  (FR-119 P2b-17b), and is sent only when true. */
export function fpsWireMessage(
  fps: FrameRate,
  retry = false,
): { t: 'rc:fps'; fps: FrameRate; retry?: true } {
  return retry ? { t: 'rc:fps', fps, retry: true } : { t: 'rc:fps', fps }
}

/** A stored or typed value, as a rate the picker offers; `null` otherwise. */
export function toFrameRate(raw: unknown): FrameRate | null {
  const n = typeof raw === 'string' ? Number(raw) : raw
  return (FRAME_RATES as readonly unknown[]).includes(n) ? (n as FrameRate) : null
}

/** The agent's answer, from the fields `rc:video-info` carries. */
export interface FrameRateAnswer {
  /** The rate the stream runs at. */
  fps: number
  /** The viewer's ask, present only when it was not granted. */
  asked: number | null
  /** Why not, in the agent's word (`unproven`, `constrained`, …). */
  reason: string | null
  /** FR-119 P2b — the rate runs as a trial: the host has not shown it can
   *  sustain it yet, and goes back to its usual rate if it cannot. */
  trial: boolean
}

/** Read the frame-rate half of a parsed `rc:video-info`; `null` when it has
 *  none (an agent without `fps-high`). */
export function frameRateAnswerOf(info: {
  fps?: number
  fps_asked?: number
  fps_reason?: string
  fps_trial?: boolean
} | null | undefined): FrameRateAnswer | null {
  if (!info || typeof info.fps !== 'number' || !Number.isFinite(info.fps)) return null
  const asked = typeof info.fps_asked === 'number' && Number.isFinite(info.fps_asked) ? info.fps_asked : null
  const reason = typeof info.fps_reason === 'string' && info.fps_reason.length > 0 ? info.fps_reason : null
  // Both or neither: an ask without its reason, or a reason without the ask,
  // explains nothing, so neither is shown.
  const refused = asked !== null && reason !== null
  return {
    fps: info.fps,
    asked: refused ? asked : null,
    reason: refused ? reason : null,
    trial: info.fps_trial === true,
  }
}

/** FR-119 P2b-3 — the host screen's refresh rate from `rc:video-info`, when the
 *  agent could read it; `null` otherwise (an older agent, or a screen that
 *  does not say). */
export function hostRefreshHzOf(info: { refresh_hz?: number } | null | undefined): number | null {
  const hz = info?.refresh_hz
  return typeof hz === 'number' && Number.isFinite(hz) && hz > 0 ? hz : null
}

/** Why the agent did not grant the ask, in words. A word this viewer does not
 *  know (from a newer agent) is shown as it came, never read as a grant. */
export function describeFrameRateReason(word: string): string {
  switch (word) {
    case 'unproven':
      return "this host hasn't shown it can sustain that rate yet"
    case 'constrained':
      return 'the connection is relayed, and a relay stays at its current rate'
    case 'refresh':
      return "the host's screen refreshes slower than that"
    case 'denied':
      return "this device's owner has turned that rate off for its encoder"
    case 'pipe':
      return "the connection hasn't shown it can carry what that rate needs"
    default:
      return word
  }
}

/** One line for the picker, from the agent's answer; `null` when there is
 *  none. */
export function frameRateSummary(answer: FrameRateAnswer | null): string | null {
  if (!answer) return null
  if (answer.asked !== null && answer.reason !== null) {
    return `${answer.fps} fps (${answer.asked} fps not granted: ${describeFrameRateReason(answer.reason)})`
  }
  if (answer.trial) {
    return `${answer.fps} fps, on trial until this host has shown it can keep up`
  }
  return `${answer.fps} fps`
}

/** FR-119 P2b-17b — offer "try again" for an ask the agent refused as unproven,
 *  to an agent that can act on it. No other reason is a verdict a retry could
 *  change: a relay, the screen's refresh, a denial and the pipe all stand. */
export function frameRateCanRetry(answer: FrameRateAnswer | null, offersRetry: boolean): boolean {
  return offersRetry && answer?.reason === 'unproven' && answer.asked !== null
}
