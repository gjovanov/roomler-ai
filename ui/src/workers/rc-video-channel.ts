// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * FR-135 R3 — the `video-bytes` channel, wherever it is read: on the page,
 * which relays each message to the data-channel decode worker (the old path,
 * and the fallback), or in the worker itself, which the page moved it into
 * (`roomler-rc-dc-worker`, `docs/remote-control.md` §11.14.1).
 *
 * Pure, and imported by both sides, so the rule is one rule: the page cannot
 * import the worker module (it would run the worker's code on the page), and
 * the worker must not import the composable.
 */

/** 2026-07-24 freeze triangulation, the network half: a silence on the video
 *  channel this long, logged once when bytes resume, means bytes were NOT
 *  arriving (network or SCTP), not that the page was too busy to forward them
 *  (the main-thread stall detector) or the decoder stalled (`decode-stall`). */
export const VIDEO_DC_GAP_MS = 1500

/** The delivery-gap clock of one channel. `note` takes each message's arrival
 *  and returns the silence before it when that was over {@link VIDEO_DC_GAP_MS},
 *  else null. The first message after `reset` (a fresh path) has none, because
 *  the setup's silence is not a gap. */
export function createDeliveryGapClock(): { note(nowMs: number): number | null; reset(): void } {
  let lastMs = 0
  return {
    note(nowMs) {
      const gap = lastMs > 0 && nowMs - lastMs > VIDEO_DC_GAP_MS ? nowMs - lastMs : null
      lastMs = nowMs
      return gap
    },
    reset() {
      lastMs = 0
    },
  }
}

/** The page hands a channel to the worker in this message. */
export type VideoChannelMessage = { type: 'video-channel'; channel: RTCDataChannel }

/** FR-135 R3 — move the `video-bytes` channel into the decode worker by
 *  posting it in the transfer list. This must run in the task that created the
 *  channel: the WebRTC spec stops a channel being transferable at the end of
 *  that task, so no message can arrive before it moves, and at its first
 *  `send()`, which the page never calls on this channel. A browser that cannot
 *  transfer an `RTCDataChannel` throws `DataCloneError` from `postMessage`
 *  before anything is detached, so the channel is still the page's, open and
 *  whole, and the page relays it as before.
 *
 *  Returns null when the worker has the channel, or why it stayed on the page.
 *  Pure for the tests: `worker` is anything with `postMessage`. */
export function moveVideoChannelToWorker(
  worker: { postMessage(message: unknown, transfer: Transferable[]): void },
  channel: RTCDataChannel,
): string | null {
  const message: VideoChannelMessage = { type: 'video-channel', channel }
  try {
    worker.postMessage(message, [channel as unknown as Transferable])
    return null
  } catch (err) {
    if (err instanceof Error) return `${err.name}: ${err.message}`
    return String(err)
  }
}
