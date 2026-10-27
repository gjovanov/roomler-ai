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
 *
 * P2 reads the size: the server gives each consumer the simulcast layer its
 * tile's height needs, and a spotlit tile's consumer a higher priority.
 */

/** What one tile reports: on screen or not, and its size in device pixels. */
export interface TileVisibility {
  visible: boolean
  w: number
  h: number
  /**
   * FR-127 P2 — the big tile of a spotlight or sidebar layout, or a pinned
   * one: its consumer gets a higher priority. The LAYOUT sets it when it
   * forwards the report; a tile does not know where it is rendered.
   */
  spotlit?: boolean
  /**
   * FR-127 P2 — a tile already on screen changed size (a window resize, a
   * layout step, a tile spotlit): the size is updated, the count of
   * reporters is not.
   */
  resized?: boolean
}

/** One entry of a `media:view` report. */
export interface ViewTile {
  producer_id: string
  w: number
  h: number
  /** FR-127 P2 — present only when true, so an older server reads past it. */
  spotlit?: boolean
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
    out.push({ producer_id, w: tile.w, h: tile.h, ...(tile.spotlit ? { spotlit: true } : {}) })
  }
  return out
}
