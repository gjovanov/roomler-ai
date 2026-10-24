// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * #2815 — roomler-desktop's main-window consent modal is the FALLBACK for
 * the small prompt window, not a second copy of it. Field 2026-10-24, on the
 * GNOME Wayland lab box: one exec request, and then one SSH request, each came
 * up twice at once, in the prompt window and as this modal in the main window.
 * Both filtered out only what the daemon shows natively.
 *
 * Driven by the REAL `consent.js`, in jsdom, against a mocked Tauri `invoke`:
 * the same approach as the viewing banner's spec.
 */
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { readFileSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'

const HERE = dirname(fileURLToPath(import.meta.url))
const FRONT = join(HERE, '..', '..', '..', '..', 'agents', 'roomler-desktop', 'src', 'front')
const SCRIPT = readFileSync(join(FRONT, 'consent.js'), 'utf8')

type Invoke = (name: string, payload?: Record<string, unknown>) => Promise<unknown>

const request = (over: Record<string, unknown> = {}) => ({
  session_id: 'g-1',
  controller_name: 'Alice (via build-01)',
  kind: 'exec',
  detail: 'id',
  surface: 'companion',
  ...over,
})

/** `invoke` answering the two questions the modal's poll asks. */
function backend(pending: unknown[], modalNeeded: boolean | Error): Invoke {
  return vi.fn(async (name: string) => {
    if (name === 'cmd_get_pending_consents') return pending
    if (name === 'cmd_consent_modal_needed') {
      if (modalNeeded instanceof Error) throw modalNeeded
      return modalNeeded
    }
    return undefined
  })
}

function mount(invoke: Invoke) {
  document.body.innerHTML = ''
  ;(window as unknown as Record<string, unknown>).__TAURI__ = { core: { invoke } }
  window.eval(SCRIPT)
}

const settle = () => vi.advanceTimersByTimeAsync(0)
const modal = () => document.getElementById('consent-modal')
const shown = () => modal() !== null && !(modal() as HTMLElement).hidden

describe('companion consent modal (#2815)', () => {
  beforeEach(() => {
    vi.useFakeTimers()
  })
  afterEach(() => {
    vi.clearAllTimers()
    vi.useRealTimers()
  })

  it('stays out of the way while the prompt window asks the question', async () => {
    mount(backend([request()], false))
    await settle()
    expect(shown()).toBe(false)
  })

  it('asks itself when the prompt window could not be opened', async () => {
    mount(backend([request()], true))
    await settle()
    expect(shown()).toBe(true)
    expect(document.getElementById('consent-who')?.textContent).toBe('Alice (via build-01)')
    expect(document.getElementById('consent-title')?.textContent).toBe('Command execution request')
  })

  it('asks itself when it cannot tell, rather than leave a question unasked', async () => {
    mount(backend([request()], new Error('unknown command')))
    await settle()
    expect(shown()).toBe(true)
  })

  it('never asks what the daemon shows natively', async () => {
    mount(backend([request({ surface: 'native' })], true))
    await settle()
    expect(shown()).toBe(false)
  })
})
