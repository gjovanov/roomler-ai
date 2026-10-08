// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// FR-90 P1g — whether agent sessions serve an organization: asked once, the
// server's answer kept; no answer is not an answer.
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { createPinia, setActivePinia } from 'pinia'

const hoisted = vi.hoisted(() => {
  class ApiError extends Error {
    constructor(
      public status: number,
      public data: unknown,
    ) {
      super(`API error ${status}`)
    }
  }
  return { get: vi.fn(), ApiError }
})
vi.mock('@/api/client', () => ({ api: { get: hoisted.get }, ApiError: hoisted.ApiError }))

import { useHiveStore } from '@/stores/hive'

beforeEach(() => {
  setActivePinia(createPinia())
  vi.spyOn(console, 'warn').mockImplementation(() => {})
})

afterEach(() => {
  vi.clearAllMocks()
  vi.restoreAllMocks()
})

describe('hive store — the organizations agent sessions serve (FR-90 P1g)', () => {
  it('asks once per organization and keeps a yes', async () => {
    hoisted.get.mockResolvedValue({ enabled: true })
    const hive = useHiveStore()
    const [a, b] = await Promise.all([hive.checkServed('t1'), hive.checkServed('t1')])
    expect([a, b]).toEqual([true, true])
    expect(await hive.checkServed('t1')).toBe(true)
    expect(hoisted.get).toHaveBeenCalledTimes(1)
    expect(hoisted.get).toHaveBeenCalledWith('/tenant/t1/hive')
    expect(hive.served).toEqual({ t1: true })
  })

  it('keeps a 404 as not served', async () => {
    hoisted.get.mockRejectedValue(new hoisted.ApiError(404, { error: 'not_found' }))
    const hive = useHiveStore()
    expect(await hive.checkServed('t2')).toBe(false)
    expect(await hive.checkServed('t2')).toBe(false)
    expect(hoisted.get).toHaveBeenCalledTimes(1)
    expect(hive.served).toEqual({ t2: false })
  })

  it('treats no answer as not served for now, and asks again', async () => {
    hoisted.get.mockRejectedValueOnce(new hoisted.ApiError(500, null)).mockResolvedValueOnce({ enabled: true })
    const hive = useHiveStore()
    expect(await hive.checkServed('t3')).toBe(false)
    expect(hive.served).toEqual({})
    expect(await hive.checkServed('t3')).toBe(true)
    expect(hoisted.get).toHaveBeenCalledTimes(2)
  })
})
