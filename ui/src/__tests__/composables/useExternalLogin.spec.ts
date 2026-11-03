// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
// @vitest-environment node
import { describe, it, expect, beforeAll, beforeEach, afterEach, vi } from 'vitest'
import * as opaque from '@serenity-kit/opaque'
import { friendlyExtauthRefusal, externalLogin } from '@/composables/useExternalLogin'
import { deriveAppKey, fromBase64Url, toBase64Url } from '@/utils/extauth'

type Frame = Record<string, unknown> & { t: string }

/** The socket's two ends: what the page sent, and who it is listening for. */
const wire = vi.hoisted(() => ({
  sent: [] as string[],
  handlers: new Map<string, Set<(f: never) => void>>(),
  device: null as null | ((f: Frame) => void),
}))

vi.mock('@/stores/ws', () => ({
  useWsStore: () => ({
    // JSON.stringify is what the real `sendRaw` puts on the socket.
    sendRaw: (msg: Frame) => {
      wire.sent.push(JSON.stringify(msg))
      wire.device?.(msg)
    },
    onRcMessage: (t: string, handler: (f: never) => void) => {
      const set = wire.handlers.get(t) ?? new Set()
      wire.handlers.set(t, set)
      set.add(handler)
      return () => set.delete(handler)
    },
  }),
}))

function deliver(frame: Frame): void {
  for (const h of wire.handlers.get(frame.t) ?? []) (h as (f: Frame) => void)(frame)
}

/** Built, never written: a secret scanner cannot tell a KDF input from a leak. */
const pw = (tag: string) => `fr52-not-a-credential-${tag}`

const ATTEMPT = 'a'.repeat(32)
const PRINCIPAL = 'b'.repeat(24)

/**
 * The device's half, played by the library's own server calls: a record made
 * the way `rc password set` makes one, and the two answers a login needs. What
 * the two ends derive is compared at the end of a login, so a frame that was
 * not the real exchange cannot pass for one.
 */
function deviceHolding(password: string) {
  const serverSetup = opaque.server.createSetup()
  const { clientRegistrationState, registrationRequest } = opaque.client.startRegistration({ password })
  const { registrationResponse } = opaque.server.createRegistrationResponse({
    serverSetup,
    userIdentifier: 'device',
    registrationRequest,
  })
  const { registrationRecord } = opaque.client.finishRegistration({
    password,
    registrationResponse,
    clientRegistrationState,
  })
  const state = { login: '', sessionKey: '' }
  wire.device = (frame) => {
    if (frame.t === 'rc:extauth.start') {
      const { serverLoginState, loginResponse } = opaque.server.startLogin({
        serverSetup,
        registrationRecord,
        startLoginRequest: String(frame.ke1),
        userIdentifier: 'device',
      })
      state.login = serverLoginState
      deliver({ t: 'rc:extauth.challenge', attempt_id: ATTEMPT, ke2: loginResponse })
    } else if (frame.t === 'rc:extauth.finish') {
      const { sessionKey } = opaque.server.finishLogin({
        serverLoginState: state.login,
        finishLoginRequest: String(frame.ke3),
      })
      state.sessionKey = sessionKey
      deliver({ t: 'rc:extauth.result', attempt_id: frame.attempt_id })
    }
  }
  return state
}

const binary = (bytes: Uint8Array) => Array.from(bytes, (b) => String.fromCharCode(b)).join('')

/**
 * The spellings of a secret a frame or a console line could hold: the same five
 * the server's capture searches (`extauth_tests.rs::spellings`).
 */
function spellings(secret: string): string[] {
  const utf8 = new TextEncoder().encode(secret)
  const utf16le = new Uint8Array(new Uint16Array(Array.from(secret, (c) => c.charCodeAt(0))).buffer)
  return [
    binary(utf8),
    btoa(binary(utf8)),
    toBase64Url(utf8),
    Array.from(utf8, (b) => b.toString(16).padStart(2, '0')).join(''),
    binary(utf16le),
  ]
}

/**
 * Searched as bytes, the way the frame leaves on the socket. JSON escapes the
 * NULs of the UTF-16 spelling, so each is searched as a frame would carry it too.
 */
function holds(captured: string, secret: string): boolean {
  const bytes = binary(new TextEncoder().encode(captured))
  return spellings(secret).some((s) => bytes.includes(s) || bytes.includes(JSON.stringify(s).slice(1, -1)))
}

function bytesOf(frame: string, field: string): number {
  const value = (JSON.parse(frame) as Record<string, unknown>)[field]
  return fromBase64Url(String(value))?.length ?? -1
}

describe('FR-52 AC: the password leaves the page in no frame and no console line', () => {
  let printed: string[]

  beforeAll(async () => {
    await opaque.ready
  })

  beforeEach(() => {
    wire.sent = []
    wire.handlers.clear()
    wire.device = null
    printed = []
    for (const level of ['log', 'info', 'warn', 'error', 'debug'] as const) {
      vi.spyOn(console, level).mockImplementation((...args: unknown[]) => {
        printed.push(args.map(String).join(' '))
      })
    }
  })

  afterEach(() => {
    vi.restoreAllMocks()
  })

  const captured = () => [...wire.sent, ...printed].join('\n')

  it('a full login: the two frames are all that is sent, and neither holds it', async () => {
    const password = pw('captured-on-the-page-5a2c')
    const device = deviceHolding(password)

    const result = await externalLogin('ABCD-EFGH-JKMN', password, PRINCIPAL)

    // The exchange really happened: the page's key is the one the device holds.
    expect(result.ok).toBe(true)
    if (!result.ok) return
    const sessionKey = fromBase64Url(device.sessionKey)
    expect(sessionKey).not.toBeNull()
    expect(result.appKey).toEqual(await deriveAppKey(sessionKey!, ATTEMPT, PRINCIPAL))

    expect(wire.sent).toHaveLength(2)
    const [start, finish] = wire.sent as [string, string]
    // The whole frame, so a field added later has to be defended here.
    expect(Object.keys(JSON.parse(start)).sort()).toEqual(['connect_code', 'ke1', 't'])
    expect(Object.keys(JSON.parse(finish)).sort()).toEqual(['attempt_id', 'connect_code', 'ke3', 't'])
    // RFC 9807's fixed sizes: a nonce, a blinded element and a key share; a MAC.
    // A password of any length has nowhere to sit in either.
    expect(bytesOf(start, 'ke1')).toBe(96)
    expect(bytesOf(finish, 'ke3')).toBe(64)

    expect(holds(captured(), password)).toBe(false)
  })

  it('a wrong guess: one frame leaves, and it does not hold the guess', async () => {
    deviceHolding(pw('the-right-one'))
    const guess = pw('a-wrong-guess-1f4b')

    const result = await externalLogin('ABCD-EFGH-JKMN', guess, PRINCIPAL)

    expect(result).toEqual({ ok: false, reason: 'wrong_password' })
    expect(wire.sent).toHaveLength(1)
    expect(JSON.parse(wire.sent[0]!).t).toBe('rc:extauth.start')
    expect(holds(captured(), guess)).toBe(false)
  })

  it('a refused start: nothing more leaves, and it does not hold the password', async () => {
    wire.device = () => deliver({ t: 'rc:extauth.result', refused: 'unavailable' })
    const password = pw('refused-at-the-door-c07e')

    const result = await externalLogin('ABCD-EFGH-JKMN', password, PRINCIPAL)

    expect(result).toMatchObject({ ok: false, reason: 'unavailable' })
    expect(wire.sent).toHaveLength(1)
    expect(holds(captured(), password)).toBe(false)
  })

  it('the same password twice is two different first frames', async () => {
    const password = pw('blinded-each-time')
    deviceHolding(password)
    await externalLogin('ABCD-EFGH-JKMN', password, PRINCIPAL)
    await externalLogin('ABCD-EFGH-JKMN', password, PRINCIPAL)

    const starts = wire.sent.filter((f) => JSON.parse(f).t === 'rc:extauth.start')
    expect(starts).toHaveLength(2)
    // A KE1 the password alone decided would be a dictionary oracle for the relay.
    expect(JSON.parse(starts[0]!).ke1).not.toBe(JSON.parse(starts[1]!).ke1)
  })

  // A capture that finds nothing proves nothing unless it could have found it.
  it('the capture finds a canary in every spelling, in a frame and in a console line', async () => {
    const canary = 'fr52-page-canary-7d3a'
    deviceHolding(pw('whatever'))
    await externalLogin(canary, pw('whatever'), PRINCIPAL)
    expect(holds(wire.sent.join('\n'), canary)).toBe(true)

    for (const spelling of spellings(canary)) {
      expect(holds(JSON.stringify({ t: 'x', field: spelling }), canary)).toBe(true)
      console.warn('a log line that prints', spelling)
      expect(holds(printed.join('\n'), canary)).toBe(true)
      printed = []
    }
  })
}, 60_000)

describe('FR-115 P3b: an external session refused by the slot count', () => {
  it('says the org is at its plan, and how past it', () => {
    const text = friendlyExtauthRefusal('session_limit')
    expect(text).toContain('as many remote sessions as its plan allows')
    expect(text).toContain('connect for another organization')
  })

  it('keeps the coarse answers it had', () => {
    expect(friendlyExtauthRefusal('unavailable')).toContain('not available for outside access')
    expect(friendlyExtauthRefusal('something-newer')).toBe('The device refused the request.')
  })
})
