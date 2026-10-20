// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// FR-116 D6 — the phone-width smoke: every signed-in route at 360 and 390 px
// (the two projects in playwright.phone.config.ts), against the built SPA with
// every `/api` call answered from fixtures.ts.
//
// Two assertions per page, the two FR-116 kept finding broken:
//  1. Nothing visible is cut off sideways. `v-main` clips horizontal overflow
//     (FR-116 P2), so the page can never scroll sideways; what that rule must
//     not do is HIDE a control. So no button, link, field, chip or heading may
//     sit past the screen's edge — unless it is inside something that scrolls
//     sideways by design (a table's own box, a tab slider). The 4 px a `v-row`
//     gutter reaches past `pa-2` is empty margin, not content, and passes.
//  2. The page's primary action is inside the viewport — the Overlay ACL's
//     "New rule" sat past the right edge, unreachable, for months.
//
// Page errors and unmocked calls are recorded as annotations, not failures:
// the smoke judges layout; the unit and E2E suites judge behaviour.
import { test, expect, type Page } from '@playwright/test'
import { installMocks, TID, ROOM, AGENT } from './fixtures'

interface PhoneRoute {
  path: string
  /** The page's primary action, which must be visible and wholly on screen. */
  action?: string
}

const T = `/tenant/${TID}`
const ROUTES: PhoneRoute[] = [
  { path: '/' },
  { path: T },
  { path: `${T}/devices`, action: '[data-tour="enroll-button"]' },
  { path: `${T}/rooms`, action: '.v-main .v-btn:has(.mdi-plus)' },
  { path: `${T}/room/${ROOM}`, action: '.v-main [contenteditable="true"]' },
  { path: `${T}/room/${ROOM}/call`, action: 'button:has-text("Join Meeting")' },
  { path: `${T}/files`, action: 'button:has-text("Upload")' },
  { path: `${T}/invites`, action: 'button:has-text("Create Invite")' },
  { path: `${T}/analytics` },
  { path: `${T}/admin/settings` },
  { path: `${T}/admin/members`, action: 'button:has-text("Add member")' },
  { path: `${T}/admin/roles`, action: 'button:has-text("New Role")' },
  { path: `${T}/audit/exec` },
  { path: `${T}/network/acl?tab=overlay`, action: 'button:has-text("New rule")' },
  { path: `${T}/network/subnet-routes` },
  { path: `${T}/agent/${AGENT}/remote`, action: '.rc-tb-back' },
  { path: '/observability' },
]

async function settle(page: Page) {
  await page.waitForSelector('.v-main', { timeout: 20_000 })
  await page.waitForLoadState('networkidle').catch(() => {})
  // Lazy panels and transitions: give the layout a beat to land.
  await page.waitForTimeout(600)
}

for (const route of ROUTES) {
  test(`${route.path.replace(TID, ':tid')} fits the phone`, async ({ page, context }, info) => {
    const unmocked = new Set<string>()
    const errors: string[] = []
    page.on('pageerror', (e) => errors.push(e.message.slice(0, 200)))
    await installMocks(context, unmocked)

    await page.goto(route.path)
    await settle(page)

    const width = page.viewportSize()!.width
    const docOver = await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth)
    expect(docOver, 'the document itself scrolls sideways').toBeLessThanOrEqual(1)
    const offscreen = await page.evaluate(() => {
      const vw = window.innerWidth
      // Inside something built to sit off-screen or scroll sideways: a closed
      // drawer (parked past the edge), a tab slider, or a scroller that
      // actually overflows (a table's wrapper, the viewer's tool row).
      const scrollsAway = (el: Element) => {
        if (el.closest('.v-navigation-drawer:not(.v-navigation-drawer--active)')) return true
        if (el.closest('.v-slide-group')) return true
        for (let e = el.parentElement; e && e !== document.body; e = e.parentElement) {
          const ox = getComputedStyle(e).overflowX
          if ((ox === 'auto' || ox === 'scroll') && e.scrollWidth > e.clientWidth + 1) return true
        }
        return false
      }
      const sel = '.v-main :is(button, a[href], input, textarea, [role="button"], .v-chip, h1, h2, h3, .v-card-title, .v-toolbar-title)'
      const out: string[] = []
      for (const el of document.querySelectorAll(sel)) {
        const r = el.getBoundingClientRect()
        if (r.width === 0 || r.height === 0 || getComputedStyle(el).visibility === 'hidden') continue
        if ((r.right > vw + 1 || r.left < -1) && !scrollsAway(el)) {
          const text = (el.textContent || el.getAttribute('aria-label') || '').trim().replace(/\s+/g, ' ').slice(0, 40)
          out.push(`<${el.tagName.toLowerCase()}> "${text}" spans ${Math.round(r.left)}–${Math.round(r.right)} px`)
        }
      }
      return out.slice(0, 8)
    })
    expect(offscreen, 'visible content past the edge of the screen, outside any sideways scroller').toEqual([])

    if (route.action) {
      const action = page.locator(route.action).first()
      await expect(action, `primary action ${route.action}`).toBeVisible()
      const box = (await action.boundingBox())!
      expect(box.x, `${route.action} starts left of the screen`).toBeGreaterThanOrEqual(-0.5)
      expect(box.x + box.width, `${route.action} ends past the right edge`).toBeLessThanOrEqual(width + 0.5)
    }

    if (unmocked.size) info.annotations.push({ type: 'unmocked', description: [...unmocked].join(', ') })
    if (errors.length) info.annotations.push({ type: 'pageerror', description: errors.join(' | ') })
  })
}
