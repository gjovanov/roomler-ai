// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
// @vitest-environment node
//
// FR-52 — the page's PINNED key stretching is the library default the device
// was proven against in P3a. Registered with the default (what a device's
// `rc password set` produces, P3a), logged in with the pin: the login opens
// only if the two stretch the password identically. The negative arm shows the
// check can fail — one pass fewer and the same password is refused.
import { describe, it, expect, beforeAll } from 'vitest'
import * as opaque from '@serenity-kit/opaque'
import { KEY_STRETCHING } from '@/utils/extauth'

/** Built, never written: a secret scanner cannot tell a KDF input from a leak. */
const pw = (tag: string) => `fr52-not-a-credential-${tag}`

function register(password: string) {
  const serverSetup = opaque.server.createSetup()
  const { clientRegistrationState, registrationRequest } = opaque.client.startRegistration({ password })
  const { registrationResponse } = opaque.server.createRegistrationResponse({
    serverSetup,
    userIdentifier: 'device',
    registrationRequest,
  })
  // The library DEFAULT stretching, as the device's record was made against it.
  const { registrationRecord } = opaque.client.finishRegistration({
    password,
    registrationResponse,
    clientRegistrationState,
  })
  return { serverSetup, registrationRecord }
}

function login(
  password: string,
  device: ReturnType<typeof register>,
  keyStretching: Parameters<typeof opaque.client.finishLogin>[0]['keyStretching'],
) {
  const { clientLoginState, startLoginRequest } = opaque.client.startLogin({ password })
  const { loginResponse } = opaque.server.startLogin({
    serverSetup: device.serverSetup,
    registrationRecord: device.registrationRecord,
    startLoginRequest,
    userIdentifier: 'device',
  })
  return opaque.client.finishLogin({ clientLoginState, loginResponse, password, keyStretching })
}

describe('FR-52 — the page pins the password stretching the device uses', () => {
  beforeAll(async () => {
    await opaque.ready
  })

  it('the pin opens a record made with the library default', () => {
    const device = register(pw('right'))
    expect(login(pw('right'), device, KEY_STRETCHING)).toBeDefined()
  })

  it('…and one pass fewer does not, so the check above can fail', () => {
    const device = register(pw('right'))
    const weaker = { 'argon2id-custom': { ...KEY_STRETCHING['argon2id-custom'], iterations: 2 } }
    expect(login(pw('right'), device, weaker)).toBeUndefined()
  })
}, 60_000)
