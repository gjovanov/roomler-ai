// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// FR-124 — the call page's view of which streams show a black picture.
//
// Alone in a call with a floating self-view you are on TWO tiles at once (the
// spotlight's fallback and the floating one). When someone joins, the
// spotlight tile unmounts and reports `false` while the floating one still
// shows black: a flag would drop the warning there, a count keeps it.
import { describe, it, expect } from 'vitest'
import { computed } from 'vue'
import { useBlackPictureTiles } from '@/composables/useBlackPictureTiles'

describe('useBlackPictureTiles', () => {
  it('one tile: black while it says so', () => {
    const t = useBlackPictureTiles()
    expect(t.isBlack('local')).toBe(false)
    t.report('local', true)
    expect(t.isBlack('local')).toBe(true)
    t.report('local', false)
    expect(t.isBlack('local')).toBe(false)
  })

  it('two tiles of one stream: black until the LAST of them recovers or unmounts', () => {
    const t = useBlackPictureTiles()
    t.report('local', true) // the spotlight tile
    t.report('local', true) // the floating self-view
    t.report('local', false) // the spotlight tile unmounts when someone joins
    expect(t.isBlack('local')).toBe(true)
    t.report('local', false)
    expect(t.isBlack('local')).toBe(false)
  })

  it('streams are counted apart', () => {
    const t = useBlackPictureTiles()
    t.report('peer-a', true)
    expect(t.isBlack('local')).toBe(false)
    expect(t.isBlack('peer-a')).toBe(true)
  })

  it('a stray false never leaves a debt that swallows the next true', () => {
    const t = useBlackPictureTiles()
    t.report('local', false)
    t.report('local', true)
    expect(t.isBlack('local')).toBe(true)
  })

  it('is reactive inside a computed', () => {
    const t = useBlackPictureTiles()
    const black = computed(() => t.isBlack('local'))
    expect(black.value).toBe(false)
    t.report('local', true)
    expect(black.value).toBe(true)
  })
})
