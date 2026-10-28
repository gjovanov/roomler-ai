// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD

/**
 * FR-127 P1 — receive what is visible: what a tile says about itself, and
 * the `media:view` report built from the tiles on screen.
 *
 * A tile reports whether it is on screen (an `IntersectionObserver`, which
 * accounts for the scrolling strip that clips it) and its size in device
 * pixels; the store keeps the set of tiles on screen by stream key, turns it
 * into the video producer ids it consumes, and sends the WHOLE set — never a
 * delta — so a lost or reordered report heals with the next one.
 */

/** What one tile reports: on screen or not, and its size in device pixels. */
export interface TileVisibility {
  visible: boolean
  w: number
  h: number
}

/** One entry of a `media:view` report. */
export interface ViewTile {
  producer_id: string
  w: number
  h: number
}

/**
 * The report for the tiles on screen: each stream key that has a video
 * producer, in the order the tiles came on screen (the server truncates a
 * report past its cap in the order sent). A tile whose peer has no video
 * producer yet (audio arrived first, or a camera never started) is left out;
 * it joins the report when its producer is learned.
 */
export function buildViewReport(
  visible: Map<string, TileVisibility>,
  videoProducerByStreamKey: Map<string, string>,
): ViewTile[] {
  const out: ViewTile[] = []
  for (const [streamKey, tile] of visible) {
    if (!tile.visible) continue
    const producer_id = videoProducerByStreamKey.get(streamKey)
    if (!producer_id) continue
    out.push({ producer_id, w: tile.w, h: tile.h })
  }
  return out
}
