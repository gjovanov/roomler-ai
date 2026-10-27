// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD

/**
 * FR-127 P2 — simulcast, the client's half: the ladder the server hands us
 * in `media:transport_created.meeting.simulcast`, turned into the
 * `encodings` the camera producer publishes.
 *
 * The server owns the ladder (`crates/modules/conference/src/media/simulcast.rs`),
 * so it changes without a client release; this file only reads it, in the
 * wire's snake_case, and refuses anything it cannot read whole — one
 * encoding then, never a half-parsed ladder.
 */

/** One rung as the wire carries it. */
export interface WireRung {
  scale_resolution_down_by: number
  max_bitrate: number
  scalability_mode?: string
}

/** One `encodings[]` entry as mediasoup-client (RTCRtpEncodingParameters) takes it. */
export interface CameraEncoding {
  scaleResolutionDownBy: number
  maxBitrate: number
  scalabilityMode: string
}

/**
 * The encodings to publish from `meeting.simulcast`, or `null` for one
 * encoding: the field absent or null (the lever off, or an older server),
 * fewer than two rungs, or any rung that is not a pair of finite positive
 * numbers. Rungs are kept in the order sent — lowest first, the order
 * mediasoup numbers spatial layers in and mediasoup-client assigns `rid`s in.
 */
export function cameraEncodings(simulcast: unknown): CameraEncoding[] | null {
  if (!simulcast || typeof simulcast !== 'object') return null
  const rungs = (simulcast as { encodings?: unknown }).encodings
  if (!Array.isArray(rungs) || rungs.length < 2) return null
  const out: CameraEncoding[] = []
  for (const r of rungs) {
    if (!r || typeof r !== 'object') return null
    const { scale_resolution_down_by: scale, max_bitrate: max, scalability_mode: mode } = r as WireRung
    if (!Number.isFinite(scale) || scale < 1 || !Number.isFinite(max) || max <= 0) return null
    out.push({
      scaleResolutionDownBy: scale,
      maxBitrate: Math.round(max),
      scalabilityMode: typeof mode === 'string' && mode ? mode : 'L1T3',
    })
  }
  return out
}

/**
 * The camera's capture size from its track, for `media:produce`'s `capture`
 * — what the server's layer map is relative to. `null` when the track
 * cannot say; the server then assumes 640×480.
 */
export function captureSize(track: MediaStreamTrack): { w: number; h: number } | null {
  try {
    const s = track.getSettings?.()
    const w = s?.width
    const h = s?.height
    if (typeof w === 'number' && typeof h === 'number' && w > 0 && h > 0 && Number.isFinite(w) && Number.isFinite(h)) {
      return { w: Math.round(w), h: Math.round(h) }
    }
  } catch {
    /* a track that has no settings */
  }
  return null
}
