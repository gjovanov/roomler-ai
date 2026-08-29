// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
import { describe, it, expect } from 'vitest'
import { qualifiedCarrier, edgeSides, participatingCarriers } from '@/utils/mesh'

describe('qualifiedCarrier', () => {
  it('renders the CLI format for qualified relays', () => {
    expect(qualifiedCarrier('relay', 'turn/udp')).toBe('relay:turn/udp')
    expect(qualifiedCarrier('relay', 'turn/tcp')).toBe('relay:turn/tcp')
    expect(qualifiedCarrier('derp', 'derp/tcp')).toBe('relay:derp/tcp')
  })

  it('degrades to the bare carrier when the agent predates the field', () => {
    expect(qualifiedCarrier('relay', null)).toBe('relay')
    expect(qualifiedCarrier('relay', undefined)).toBe('relay')
    expect(qualifiedCarrier('derp', null)).toBe('derp')
  })

  it('never decorates a non-relay carrier, even with a stale qualifier', () => {
    expect(qualifiedCarrier('direct', 'turn/udp')).toBe('direct')
    expect(qualifiedCarrier('offline', 'derp/tcp')).toBe('offline')
  })
})

describe('edgeSides', () => {
  const ends = [
    { node: 'a', carrier: 'direct', rtt_ms: 39 },
    { node: 'b', carrier: 'relay', relay: 'turn/udp', rtt_ms: 87 },
  ]

  it('resolves each direction to its own report and flags the asymmetry', () => {
    const s = edgeSides(ends, 'a', 'b')
    expect(s.from?.carrier).toBe('direct')
    expect(s.to?.relay).toBe('turn/udp')
    expect(s.asymmetric).toBe(true)
  })

  it('an agreeing pair is not asymmetric', () => {
    const both = [
      { node: 'a', carrier: 'direct' },
      { node: 'b', carrier: 'direct' },
    ]
    expect(edgeSides(both, 'a', 'b').asymmetric).toBe(false)
  })

  it('a one-sided edge cannot claim asymmetry', () => {
    const s = edgeSides([{ node: 'a', carrier: 'relay' }], 'a', 'b')
    expect(s.from?.carrier).toBe('relay')
    expect(s.to).toBeUndefined()
    expect(s.asymmetric).toBe(false)
  })

  it('missing ends (old server payload) resolves to nothing', () => {
    expect(edgeSides(undefined, 'a', 'b')).toEqual({
      from: undefined,
      to: undefined,
      asymmetric: false,
    })
  })
})

describe('participatingCarriers', () => {
  it('an asymmetric edge belongs to BOTH classes — hiding relay must not hide its direct half', () => {
    const ends = [
      { node: 'a', carrier: 'direct' },
      { node: 'b', carrier: 'relay' },
    ]
    expect(participatingCarriers('relay', ends).sort()).toEqual(['direct', 'relay'])
  })

  it('falls back to the merged carrier when ends are absent', () => {
    expect(participatingCarriers('derp', undefined)).toEqual(['derp'])
    expect(participatingCarriers('derp', [])).toEqual(['derp'])
  })

  it('a symmetric pair collapses to one class', () => {
    const ends = [
      { node: 'a', carrier: 'derp' },
      { node: 'b', carrier: 'derp' },
    ]
    expect(participatingCarriers('derp', ends)).toEqual(['derp'])
  })
})
