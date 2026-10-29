// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// #3029 — the pure parts of account activation: the two parameters read
// from the email's link, the request (`/api/auth/activate`, snake_case keys),
// its three outcomes, and the one-POST-per-link guards (shared in-flight
// requests, a success remembered for the tab, a storage that throws).
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import {
  activateAccount,
  activationParams,
  rememberActivated,
  wasActivated,
  type StorageLike,
} from '@/utils/activation'

const PARAMS = { userId: 'bbbbbbbbbbbbbbbbbbbbbbbb', token: 'abc1234' }

describe('activationParams', () => {
  it('reads both parameters, trimmed', () => {
    expect(activationParams({ userId: ' u1 ', token: ' t1 ' })).toEqual({ userId: 'u1', token: 't1' })
  })

  it('is null when either is missing, empty or blank — no request for a half link', () => {
    expect(activationParams({})).toBeNull()
    expect(activationParams({ userId: 'u1' })).toBeNull()
    expect(activationParams({ token: 't1' })).toBeNull()
    expect(activationParams({ userId: '', token: 't1' })).toBeNull()
    expect(activationParams({ userId: 'u1', token: '   ' })).toBeNull()
    expect(activationParams({ userId: null, token: 't1' })).toBeNull()
  })

  it('takes the first value of a repeated parameter', () => {
    expect(activationParams({ userId: ['u1', 'u2'], token: ['t1'] })).toEqual({ userId: 'u1', token: 't1' })
    expect(activationParams({ userId: [null], token: 't1' })).toBeNull()
  })
})

describe('the tab’s memory of a success', () => {
  function memory(): StorageLike & { map: Map<string, string> } {
    const map = new Map<string, string>()
    return { map, getItem: (k) => map.get(k) ?? null, setItem: (k, v) => void map.set(k, v) }
  }

  it('remembers per user id', () => {
    const s = memory()
    expect(wasActivated('u1', s)).toBe(false)
    rememberActivated('u1', s)
    expect(wasActivated('u1', s)).toBe(true)
    expect(wasActivated('u2', s)).toBe(false)
  })

  it('never throws when storage refuses, and then simply does not remember', () => {
    const refuse = () => {
      throw new DOMException('SecurityError')
    }
    const s: StorageLike = { getItem: refuse, setItem: refuse }
    expect(() => rememberActivated('u1', s)).not.toThrow()
    expect(wasActivated('u1', s)).toBe(false)
    expect(wasActivated('u1', null)).toBe(false)
    expect(() => rememberActivated('u1', null)).not.toThrow()
  })
})

describe('activateAccount', () => {
  beforeEach(() => {
    sessionStorage.clear()
  })
  afterEach(() => {
    sessionStorage.clear()
  })

  function respond(status: number) {
    return vi.fn(async () => ({ ok: status >= 200 && status < 300, status }))
  }

  it('POSTs the server’s shape — `user_id` and `token`, snake_case — and is `ok` on 200', async () => {
    const f = respond(200)
    await expect(activateAccount(PARAMS, f)).resolves.toBe('ok')
    expect(f).toHaveBeenCalledTimes(1)
    const [url, init] = f.mock.calls[0] as unknown as [string, RequestInit]
    expect(url).toBe('/api/auth/activate')
    expect(init.method).toBe('POST')
    expect(JSON.parse(String(init.body))).toEqual({ user_id: PARAMS.userId, token: PARAMS.token })
    expect((init.headers as Record<string, string>)['Content-Type']).toBe('application/json')
  })

  it('remembers the success for the tab', async () => {
    await activateAccount(PARAMS, respond(200))
    expect(wasActivated(PARAMS.userId)).toBe(true)
  })

  it('is `invalid` on the server’s 400, and remembers nothing', async () => {
    await expect(activateAccount(PARAMS, respond(400))).resolves.toBe('invalid')
    expect(wasActivated(PARAMS.userId)).toBe(false)
  })

  it('is `error` on anything else — a 5xx, a 429, the server unreachable', async () => {
    await expect(activateAccount(PARAMS, respond(500))).resolves.toBe('error')
    await expect(activateAccount(PARAMS, respond(429))).resolves.toBe('error')
    await expect(activateAccount(PARAMS, vi.fn(async () => Promise.reject(new TypeError('Failed to fetch'))))).resolves.toBe('error')
    expect(wasActivated(PARAMS.userId)).toBe(false)
  })

  it('shares one request between concurrent callers for the same link (a double-fired effect)', async () => {
    let release!: (v: { ok: boolean; status: number }) => void
    const f = vi.fn(() => new Promise<{ ok: boolean; status: number }>((r) => (release = r)))
    const a = activateAccount(PARAMS, f)
    const b = activateAccount(PARAMS, f)
    expect(f).toHaveBeenCalledTimes(1)
    release({ ok: true, status: 200 })
    await expect(Promise.all([a, b])).resolves.toEqual(['ok', 'ok'])
  })

  it('asks again after a settled request — "Try again" after an error really tries', async () => {
    const f = respond(500)
    await activateAccount(PARAMS, f)
    await activateAccount(PARAMS, f)
    expect(f).toHaveBeenCalledTimes(2)
  })

  it('does not share requests between different links', async () => {
    const f = respond(200)
    await Promise.all([activateAccount(PARAMS, f), activateAccount({ ...PARAMS, token: 'other12' }, f)])
    expect(f).toHaveBeenCalledTimes(2)
  })
})
