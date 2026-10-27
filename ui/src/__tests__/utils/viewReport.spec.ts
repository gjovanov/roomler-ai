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

  // FR-127 P2 — the spotlit flag rides the report only when true, so an
  // older server reads past it and a plain tile's entry is P1's exactly.
  it('marks a spotlit tile, and only a spotlit one', () => {
    const visible = new Map<string, TileVisibility>([
      ['c1', { ...on(1280, 720), spotlit: true }],
      ['c2', { ...on(160, 90), spotlit: false }],
      ['c3', on(160, 90)],
    ])
    const producers = new Map([
      ['c1', 'vp-1'],
      ['c2', 'vp-2'],
      ['c3', 'vp-3'],
    ])
    expect(buildViewReport(visible, producers)).toEqual([
      { producer_id: 'vp-1', w: 1280, h: 720, spotlit: true },
      { producer_id: 'vp-2', w: 160, h: 90 },
      { producer_id: 'vp-3', w: 160, h: 90 },
    ])
  })
})
