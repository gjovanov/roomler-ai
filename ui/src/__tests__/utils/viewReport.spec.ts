// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// FR-127 P1 — the `media:view` report built from the tiles on screen.
import { describe, it, expect } from 'vitest'
import { buildViewReport, type TileVisibility } from '@/utils/viewReport'

const on = (w = 320, h = 240): TileVisibility => ({ visible: true, w, h })
const off = (): TileVisibility => ({ visible: false, w: 0, h: 0 })

describe('buildViewReport (FR-127 P1)', () => {
  it('names the video producer of every tile on screen, in the order they came on screen', () => {
    const visible = new Map([
      ['c2', on()],
      ['c1', on()],
    ])
    const producers = new Map([
      ['c1', 'vp-1'],
      ['c2', 'vp-2'],
    ])
    expect(buildViewReport(visible, producers).map((t) => t.producer_id)).toEqual(['vp-2', 'vp-1'])
  })

  it('leaves out a tile that is off screen', () => {
    const visible = new Map([
      ['c1', on()],
      ['c2', off()],
    ])
    const producers = new Map([
      ['c1', 'vp-1'],
      ['c2', 'vp-2'],
    ])
    expect(buildViewReport(visible, producers).map((t) => t.producer_id)).toEqual(['vp-1'])
  })

  it('leaves out a tile whose peer has no video producer yet (audio arrived first)', () => {
    const visible = new Map([
      ['c1', on()],
      ['c2', on()],
    ])
    const producers = new Map([['c1', 'vp-1']])
    expect(buildViewReport(visible, producers)).toEqual([{ producer_id: 'vp-1', w: 320, h: 240 }])
  })

  it('carries the tile size, for the layer map of P2', () => {
    const visible = new Map([['c1:screen', on(1920, 1080)]])
    const producers = new Map([['c1:screen', 'sp-1']])
    expect(buildViewReport(visible, producers)).toEqual([{ producer_id: 'sp-1', w: 1920, h: 1080 }])
  })

  it('is empty when nothing is on screen', () => {
    expect(buildViewReport(new Map(), new Map([['c1', 'vp-1']]))).toEqual([])
  })
})
