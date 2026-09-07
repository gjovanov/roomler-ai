// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// FR-69 AC7 — the full web app against a server that mounts NO chat and NO
// conference (the `mesh` / `remote` / `access` profiles): the navigation and
// the org dashboard show no collaboration surfaces, a chat deep-link lands on
// the org dashboard instead of a blank page, the devices page still lists,
// and nothing in the flow logs a console error or a failed API call.
//
// Skips itself against a server that mounts `chat` (the nightly runs against
// `full`), so it is inert there and meaningful only where it can fail. Run it
// against a local mesh stack with:
//
//   E2E_BASE_URL=http://localhost:8090 E2E_API_URL=http://localhost:8090 CI=1 \
//     bunx playwright test e2e/mesh-profile.spec.ts --reporter=list
import { test, expect, type Page } from '@playwright/test'
import {
  uniqueUser,
  registerUserViaApi,
  createTenantViaApi,
  loginViaUi,
} from './fixtures/test-helpers'

const API_URL = process.env.E2E_API_URL || 'http://localhost:5001'

async function serverModules(): Promise<string[]> {
  const resp = await fetch(`${API_URL}/api/capabilities`)
  if (!resp.ok) throw new Error(`capabilities: ${resp.status}`)
  const caps = (await resp.json()) as { modules: string[] }
  return caps.modules
}

/** Every console error, page error and failed `/api/` response during the flow. */
function watchForTrouble(page: Page) {
  const trouble: string[] = []
  page.on('console', (msg) => {
    if (msg.type() === 'error') trouble.push(`console.error: ${msg.text()}`)
  })
  page.on('pageerror', (err) => trouble.push(`pageerror: ${err.message}`))
  page.on('response', (resp) => {
    const url = resp.url()
    if (url.includes('/api/') && resp.status() >= 400) {
      trouble.push(`${resp.request().method()} ${url} -> ${resp.status()}`)
    }
  })
  return trouble
}

/** Whether this server mounts `network` — decided once, in `beforeAll`. */
let hasNetwork = false

test.describe('mesh profile — the SPA gates on what the server mounts (FR-69 AC7)', () => {
  test.beforeAll(async () => {
    const modules = await serverModules()
    test.skip(
      modules.includes('chat'),
      `this server mounts chat (${modules.join(',')}) — the spec asserts a server without it`,
    )
    expect(modules).toContain('fleet')
    // ⚠️ Three profiles mount no chat — `mesh` (fleet+network), `remote`
    // (fleet+remote) and `access` (all three) — and the spec is meaningful on
    // all of them. Only the NETWORK-owned surfaces may be asserted
    // unconditionally on `mesh`: `remote` has no tunnels, so demanding a
    // "Tunnels" tile there fails the SPA for doing exactly the right thing.
    // (Measured by FR-75's `remote` cell, #1447.)
    hasNetwork = modules.includes('network')
  })

  test('no chat or conference surfaces, no console errors, no failed API calls', async ({
    page,
  }) => {
    const trouble = watchForTrouble(page)

    const user = uniqueUser()
    const result = await registerUserViaApi(user)
    const tenant = await createTenantViaApi(
      result.access_token,
      'Mesh Org',
      `mesh-${Date.now()}`,
    )
    await loginViaUi(page, user.username, user.password)

    // The org dashboard: fleet tiles present, collaboration tiles absent.
    await page.goto(`/tenant/${tenant.id}`)
    await expect(page.getByRole('heading', { name: 'Mesh Org' })).toBeVisible({ timeout: 10000 })
    await expect(page.getByText('Devices Online', { exact: true })).toBeVisible()
    // `exact`: the Network card's description also says "tunnels".
    if (hasNetwork) await expect(page.getByText('Tunnels', { exact: true })).toBeVisible()
    await expect(page.getByText(/new room/i)).toHaveCount(0)
    await expect(page.getByText(/start call/i)).toHaveCount(0)
    await expect(page.getByText(/upload file/i)).toHaveCount(0)
    await expect(page.getByText('Active Calls', { exact: true })).toHaveCount(0)
    await expect(page.getByText('Messages', { exact: true })).toHaveCount(0)
    await expect(page.getByText(/call minutes/i)).toHaveCount(0)

    // The navigation: Devices + Network groups, no Rooms / Explore / Files.
    const nav = page.locator('nav, .v-navigation-drawer').first()
    await expect(nav.getByText('Devices', { exact: true }).first()).toBeVisible()
    if (hasNetwork) await expect(nav.getByText('Network', { exact: true }).first()).toBeVisible()
    await expect(nav.locator(`a[href$="/tenant/${tenant.id}/files"]`)).toHaveCount(0)
    await expect(nav.locator(`a[href$="/tenant/${tenant.id}/explore"]`)).toHaveCount(0)
    await expect(nav.locator(`a[href$="/tenant/${tenant.id}/rooms"]`)).toHaveCount(0)

    // A chat deep-link is refused by the router guard: it lands on the org
    // dashboard, never on a blank page whose every request 404s.
    await page.goto(`/tenant/${tenant.id}/rooms`)
    await expect(page).toHaveURL(new RegExp(`/tenant/${tenant.id}/?$`), { timeout: 10000 })

    // The devices page still lists (the host's composition view — a server
    // without `network` answers with no tunnel rows, and this one has it).
    const listing = page.waitForResponse(
      (r) => r.url().includes(`/api/tenant/${tenant.id}/device`) && r.request().method() === 'GET',
    )
    await page.goto(`/tenant/${tenant.id}/devices`)
    expect((await listing).status()).toBe(200)

    // The gate itself, as the page sees it.
    const caps = await page.evaluate(async () => {
      const r = await fetch('/api/capabilities')
      return (await r.json()) as { modules: string[] }
    })
    expect(caps.modules).not.toContain('chat')
    expect(caps.modules).not.toContain('conference')

    expect(trouble, `trouble during the flow:\n${trouble.join('\n')}`).toEqual([])
  })
})
