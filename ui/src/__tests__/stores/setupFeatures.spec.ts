// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
import { describe, it, expect, vi, beforeEach } from 'vitest'
import { setActivePinia, createPinia } from 'pinia'

vi.mock('@/api/client', () => ({
  api: {
    get: vi.fn(),
    post: vi.fn(),
    put: vi.fn(),
    delete: vi.fn(),
  },
}))

import { useSetupFeaturesStore } from '@/stores/setupFeatures'
import { installCards } from '@/utils/enrollCommands'
import { DOWNLOAD, downloadLead } from '@/utils/landing'
import { api } from '@/api/client'

const mockApi = vi.mocked(api)
const ORIGIN = 'https://example.roomler.test'
const ALL_ON = { pairing: true, wizard_unix_daemon: true, wizard_pairing: true }

/** A refusal as `api` throws it: an `Error` carrying the HTTP status. */
const refused = (status: number) => Object.assign(new Error(`API error ${status}`), { status })

/**
 * FR-129 B1-s — `GET /api/setup/features` fails CLOSED: every switch is
 * false before the answer, after a failed request, on an older server, and
 * for anything the server did not send as a JSON `true`.
 */
describe('useSetupFeaturesStore', () => {
  beforeEach(() => {
    setActivePinia(createPinia())
    vi.clearAllMocks()
  })

  it('fails CLOSED before the server has answered', () => {
    const store = useSetupFeaturesStore()
    expect(store.loaded).toBe(false)
    expect(store.pairing).toBe(false)
    expect(store.wizardUnixDaemon).toBe(false)
    expect(store.wizardPairing).toBe(false)
    expect(store.wizardPairs).toBe(false)
  })

  it('switches on only what the server said true', async () => {
    mockApi.get.mockResolvedValueOnce(ALL_ON)
    const store = useSetupFeaturesStore()
    await store.load()
    expect(mockApi.get).toHaveBeenCalledWith('/setup/features')
    expect(store.loaded).toBe(true)
    expect(store.failed).toBe(false)
    expect(store.pairing).toBe(true)
    expect(store.wizardUnixDaemon).toBe(true)
    expect(store.wizardPairing).toBe(true)
    expect(store.wizardPairs).toBe(true)

    store.reset()
    mockApi.get.mockResolvedValueOnce({ pairing: true, wizard_unix_daemon: false, wizard_pairing: false })
    await store.load()
    expect(store.pairing).toBe(true)
    expect(store.wizardUnixDaemon).toBe(false)
    expect(store.wizardPairing).toBe(false)
    expect(store.wizardPairs).toBe(false)
  })

  /** The lead's gate: the server pairs AND the released wizard does. The
   *  shipped default is `pairing: true` with `wizard_pairing: false` — a
   *  server promoted before the first wizard that pairs — and that must NOT
   *  read as "the download signs you in from the browser". */
  it('wizardPairs needs both: the server pairs AND the released wizard does', async () => {
    const store = useSetupFeaturesStore()
    for (const [body, want] of [
      [{ pairing: true, wizard_pairing: false }, false],
      [{ pairing: false, wizard_pairing: true }, false],
      [{ pairing: true }, false],
      [{ wizard_pairing: true }, false],
      [{ pairing: true, wizard_pairing: true }, true],
    ] as Array<[Record<string, unknown>, boolean]>) {
      store.reset()
      mockApi.get.mockResolvedValueOnce(body)
      await store.load()
      expect(store.wizardPairs, JSON.stringify(body)).toBe(want)
      expect(downloadLead(store.wizardPairs)).toBe(want ? DOWNLOAD.leadPairing : DOWNLOAD.lead)
    }
  })

  it.each([
    ['a string "true"', { pairing: 'true', wizard_unix_daemon: 'true', wizard_pairing: 'true' }],
    ['a number', { pairing: 1, wizard_unix_daemon: 1, wizard_pairing: 1 }],
    ['missing keys', {}],
    ['a null body', null],
    ['an array', [true, true, true]],
    ['a blob (a non-JSON content type)', new Blob(['<html>'])],
  ])('reads anything but a JSON true as false: %s', async (_, body) => {
    mockApi.get.mockResolvedValueOnce(body)
    const store = useSetupFeaturesStore()
    await store.load()
    expect(store.loaded).toBe(true)
    expect(store.pairing).toBe(false)
    expect(store.wizardUnixDaemon).toBe(false)
    expect(store.wizardPairing).toBe(false)
    expect(store.wizardPairs).toBe(false)
  })

  it('stays closed, quietly, on an older server (404), and says so on any other failure', async () => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => {})
    mockApi.get.mockRejectedValueOnce(refused(404))
    const store = useSetupFeaturesStore()
    await store.load()
    expect(store.loaded).toBe(true)
    expect(store.failed).toBe(true)
    expect(store.pairing).toBe(false)
    expect(store.wizardUnixDaemon).toBe(false)
    expect(store.wizardPairing).toBe(false)
    expect(warn).not.toHaveBeenCalled()

    store.reset()
    mockApi.get.mockRejectedValueOnce(refused(503)) // the fleet module unmounted
    await store.load()
    expect(store.failed).toBe(true)
    expect(store.wizardUnixDaemon).toBe(false)
    expect(warn).toHaveBeenCalledTimes(1)

    store.reset()
    mockApi.get.mockRejectedValueOnce(new Error('offline')) // no status at all
    await store.load()
    expect(store.failed).toBe(true)
    expect(store.pairing).toBe(false)
    expect(warn).toHaveBeenCalledTimes(2)
    warn.mockRestore()
  })

  /** The consumers' contract: a failed or absent fetch leaves today's output
   *  — the cards and the lead exactly as before FR-129. */
  it('a failed or absent fetch leaves the download cards and the lead as they were', async () => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => {})
    const before = installCards(ORIGIN)
    const store = useSetupFeaturesStore()
    // Absent: nothing fetched yet (the first paint).
    expect(installCards(ORIGIN, { wizardUnixDaemon: store.wizardUnixDaemon })).toEqual(before)
    expect(downloadLead(store.wizardPairs)).toBe(DOWNLOAD.lead)
    // Failed.
    mockApi.get.mockRejectedValueOnce(refused(503))
    await store.load()
    expect(installCards(ORIGIN, { wizardUnixDaemon: store.wizardUnixDaemon })).toEqual(before)
    expect(downloadLead(store.wizardPairs)).toBe(DOWNLOAD.lead)
    expect(before.map((c) => [c.os, c.wizardUrl])).toEqual([
      ['windows', `${ORIGIN}/api/setup/windows`],
      ['linux', null],
      ['macos', null],
    ])
    // And once the server has said so, the cards and the lead follow.
    store.reset()
    mockApi.get.mockResolvedValueOnce(ALL_ON)
    await store.load()
    expect(installCards(ORIGIN, { wizardUnixDaemon: store.wizardUnixDaemon }).map((c) => c.wizardUrl)).toEqual([
      `${ORIGIN}/api/setup/windows`,
      `${ORIGIN}/api/setup/linux`,
      `${ORIGIN}/api/setup/macos`,
    ])
    expect(downloadLead(store.wizardPairs)).toBe(DOWNLOAD.leadPairing)
    warn.mockRestore()
  })

  it('shares one in-flight request and does not refetch once loaded', async () => {
    mockApi.get.mockResolvedValueOnce({ pairing: true, wizard_unix_daemon: false, wizard_pairing: false })
    const store = useSetupFeaturesStore()
    await Promise.all([store.load(), store.ready(), store.load()])
    await store.ready()
    await store.load()
    expect(mockApi.get).toHaveBeenCalledTimes(1)
  })

  it('reset forgets the answer and fails closed again', async () => {
    mockApi.get.mockResolvedValueOnce(ALL_ON)
    const store = useSetupFeaturesStore()
    await store.load()
    expect(store.wizardUnixDaemon).toBe(true)
    expect(store.wizardPairs).toBe(true)
    store.reset()
    expect(store.loaded).toBe(false)
    expect(store.pairing).toBe(false)
    expect(store.wizardUnixDaemon).toBe(false)
    expect(store.wizardPairing).toBe(false)
    expect(store.wizardPairs).toBe(false)
  })
})
