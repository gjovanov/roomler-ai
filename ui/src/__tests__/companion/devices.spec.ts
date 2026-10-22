// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * #2729 — roomler-desktop's Devices view: the View screen button, driven in
 * jsdom against a mocked Tauri `invoke`.
 *
 * The companion ships plain scripts with no bundler, so the test evaluates the
 * REAL `devices.js` the way the webview does, and builds only the actions
 * column: the grid never boots. What it pins: a failed open shows its reason
 * as the button's tooltip, and the next successful open puts the button's own
 * tooltip back. Before #2729 the error stayed, so a device that once had no
 * browser said "No web browser is set up…" on hover through every later open.
 */
import { readFileSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'
import { describe, expect, it, vi } from 'vitest'

// node:path, not `new URL(…)`: under jsdom the global URL is jsdom's, which
// node's fileURLToPath rejects.
const HERE = dirname(fileURLToPath(import.meta.url))
const FRONT = join(HERE, '..', '..', '..', '..', 'agents', 'roomler-desktop', 'src', 'front')
const SCRIPT = readFileSync(join(FRONT, 'devices.js'), 'utf8')

type Invoke = (name: string, payload?: Record<string, unknown>) => Promise<unknown>
type Column = {
  key: string
  build: (td: HTMLElement) => unknown
  update: (state: unknown, row: Record<string, unknown>) => void
}

/** The REAL actions column, over a `window.Roomler` that answers only what
 *  the column asks: `invoke`, and an org list with no secondary orgs. */
function actionsColumn(invoke: Invoke): Column {
  const w = window as unknown as Record<string, unknown>
  w.Roomler = {
    $: (id: string) => document.getElementById(id),
    invoke,
    show: () => {},
    hide: () => {},
    fmtRelative: () => '',
    on: () => {},
    get: () => undefined,
    currentView: () => 'devices',
  }
  window.eval(SCRIPT)
  const columns = (w.RoomlerDevices as { COLUMNS: Column[] }).COLUMNS
  return columns.find((c) => c.key === 'actions')!
}

/** Let the click handler's awaited `invoke` settle. */
const settle = () => new Promise((resolve) => setTimeout(resolve, 0))

const AGENT = { kind: 'agent', is_self: false, id: '0123456789abcdef01234567' }
const NO_BROWSER =
  'No web browser is set up on this computer, so the link cannot open. Install one ' +
  "(Firefox, for example) or choose a default web browser in your desktop's settings, " +
  'then try again.'

describe('companion Devices view: View screen (#2729)', () => {
  it('shows a failure as the tooltip, and a later success puts its own tooltip back', async () => {
    const invoke = vi
      .fn<Invoke>()
      .mockRejectedValueOnce(NO_BROWSER)
      .mockResolvedValueOnce(null)
    const column = actionsColumn(invoke)
    const td = document.createElement('td')
    column.update(column.build(td), AGENT)
    const btn = td.querySelector('button')!
    expect(btn.hidden).toBe(false)
    const own = btn.title
    expect(own).toBe('Open the remote-control viewer in your browser (sign-in required)')

    btn.click()
    await settle()
    expect(invoke).toHaveBeenLastCalledWith('cmd_open_remote', { agentId: AGENT.id, org: null })
    expect(btn.title).toBe(NO_BROWSER)
    expect(btn.classList.contains('is-error')).toBe(true)
    expect(btn.disabled).toBe(false)

    btn.click()
    await settle()
    expect(invoke).toHaveBeenCalledTimes(2)
    expect(btn.title).toBe(own)
    expect(btn.classList.contains('is-error')).toBe(false)
  })

  it('offers no View screen for this device, nor for a tunnel client', () => {
    const column = actionsColumn(vi.fn<Invoke>())
    for (const row of [
      { ...AGENT, is_self: true },
      { kind: 'tunnel_client', is_self: false, id: AGENT.id },
    ]) {
      const td = document.createElement('td')
      column.update(column.build(td), row)
      expect(td.querySelector('button')!.hidden).toBe(true)
    }
  })
})
