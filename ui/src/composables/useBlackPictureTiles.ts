// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
import { reactive } from 'vue'

/**
 * FR-124 — which streams are showing a black picture, from the tiles' own
 * `picture-black` reports.
 *
 * A count per stream key, not a flag: alone in a call with a floating
 * self-view, you are on two tiles at once, and the one that unmounts when
 * someone joins must not clear a warning the other still earns. Each tile
 * reports changes only, and `false` on unmount if it was black, so the count
 * stays honest.
 */
export function useBlackPictureTiles() {
  const counts = reactive(new Map<string, number>())

  function report(streamKey: string, black: boolean) {
    const n = (counts.get(streamKey) ?? 0) + (black ? 1 : -1)
    if (n > 0) counts.set(streamKey, n)
    else counts.delete(streamKey)
  }

  /** Reactive: re-read inside a computed. */
  function isBlack(streamKey: string): boolean {
    return (counts.get(streamKey) ?? 0) > 0
  }

  return { report, isBlack }
}
