/**
 * #1637 — the app bar's Active Calls menu pushes another room's call onto the
 * call page in place (`router.push({ name: 'room-call', … })` with only
 * `roomId` changed). Against a real API with mediasoup, so the server's
 * participant lists say which call a person is in, whatever the page shows.
 *
 * The owner and a peer share a tenant with rooms A and B. The peer is in B's
 * call through the API, so B is listed under Active Calls.
 *
 *  1. In A's call, Active Calls → B asks first. Switch Call leaves A's call and
 *     joins B's: the server lists the owner in B and no longer in A, and the
 *     page is B's.
 *  2. Cancel keeps both the call and the URL: still in A's call, on A's page.
 *  3. On A's call page but in no call, the page follows the room with no
 *     question, and joins nothing.
 *
 * The outcome is asserted before the question, so a page that never asks
 * still fails on what the server lists.
 */
import { test, expect, type Browser, type Page } from '@playwright/test'
import {
  uniqueUser,
  registerUserViaApi,
  createTenantViaApi,
  createRoomViaApi,
  startCallViaApi,
  addTenantMemberViaApi,
  joinRoomViaApi,
  joinCallViaApi,
  loginViaUi,
} from './fixtures/test-helpers'

const API_URL = process.env.E2E_API_URL || 'http://localhost:5001'

/** The user ids the server lists in a room's call. */
async function callUserIds(token: string, tenantId: string, roomId: string): Promise<string[]> {
  const resp = await fetch(`${API_URL}/api/tenant/${tenantId}/room/${roomId}/call/participant`, {
    headers: { Authorization: `Bearer ${token}` },
  })
  if (!resp.ok) throw new Error(`List call participants failed: ${resp.status}`)
  const items = (await resp.json()) as { user_id: string | null }[]
  return items.flatMap((p) => (p.user_id ? [p.user_id] : []))
}

test.describe('Call page follows its room (#1637)', () => {
  let ownerUser: ReturnType<typeof uniqueUser>
  let ownerToken: string
  let ownerId: string
  let tenantId: string
  let roomA: string
  let roomB: string

  const callUrl = (roomId: string) => new RegExp(`/tenant/${tenantId}/room/${roomId}/call$`)
  const title = (page: Page) => page.locator('.conference-main-col .v-toolbar-title').first()

  test.beforeEach(async () => {
    ownerUser = uniqueUser()
    const peerUser = uniqueUser()

    const ownerResult = await registerUserViaApi(ownerUser)
    ownerToken = ownerResult.access_token
    ownerId = ownerResult.user.id
    const peerResult = await registerUserViaApi(peerUser)

    const tenant = await createTenantViaApi(ownerToken, 'Room Switch Org', `roomswitch-${Date.now()}`)
    tenantId = tenant.id
    await addTenantMemberViaApi(ownerToken, tenantId, peerResult.user.id)

    roomA = (await createRoomViaApi(ownerToken, tenantId, 'Room A')).id
    roomB = (await createRoomViaApi(ownerToken, tenantId, 'Room B')).id
    await startCallViaApi(ownerToken, tenantId, roomA)
    await startCallViaApi(ownerToken, tenantId, roomB)

    // The peer is in B's call, so B is listed under Active Calls.
    await joinRoomViaApi(peerResult.access_token, tenantId, roomB)
    await joinCallViaApi(peerResult.access_token, tenantId, roomB)
  })

  async function signedIn(browser: Browser) {
    const ctx = await browser.newContext({ permissions: ['camera', 'microphone'] })
    const page = await ctx.newPage()
    await loginViaUi(page, ownerUser.username, ownerUser.password)
    return { ctx, page }
  }

  async function joinA(page: Page) {
    // Join once the page's socket has its connection id, as a person does. A
    // join sent before then opens a session with no connection id, and no
    // later leave from this page closes it: that leave names its connection.
    const socketUp = page.waitForEvent('console', {
      predicate: (m) => m.text().startsWith('[WS] received: connected'),
      timeout: 15_000,
    })
    await page.goto(`/tenant/${tenantId}/room/${roomA}/call`)
    await socketUp
    await expect(page.getByRole('button', { name: 'Join Meeting' })).toBeVisible({ timeout: 10_000 })
    await page.getByRole('button', { name: 'Join Meeting' }).click()
    await expect(page.getByText('You')).toBeVisible({ timeout: 15_000 })
    await expect(title(page)).toHaveText('Room A')
    await expect.poll(() => callUserIds(ownerToken, tenantId, roomA)).toContain(ownerId)
  }

  async function activeCallsTo(page: Page, name: string) {
    await page.locator('button.call-pulse').click()
    await page.locator('.v-menu .v-list-item').filter({ hasText: name }).click()
  }

  /** Waits for the page's question; false when it never comes. */
  async function asked(page: Page, testId: 'call-switch-confirm' | 'call-switch-cancel') {
    return page
      .getByTestId(testId)
      .waitFor({ timeout: 5_000 })
      .then(() => true)
      .catch(() => false)
  }

  test("in A's call, Switch Call leaves A's call and joins B's", async ({ browser }) => {
    const { ctx, page } = await signedIn(browser)
    try {
      await joinA(page)

      await activeCallsTo(page, 'Room B')
      const didAsk = await asked(page, 'call-switch-confirm')
      if (didAsk) await page.getByTestId('call-switch-confirm').click()

      await expect
        .poll(() => callUserIds(ownerToken, tenantId, roomA), { message: "the server still lists the owner in A's call" })
        .not.toContain(ownerId)
      await expect
        .poll(() => callUserIds(ownerToken, tenantId, roomB), { message: "the server lists the owner in B's call" })
        .toContain(ownerId)
      await expect(page).toHaveURL(callUrl(roomB))
      await expect(title(page)).toHaveText('Room B')
      await expect(page.getByText('You')).toBeVisible({ timeout: 15_000 })
      expect(didAsk, 'the page asks before it leaves a call').toBe(true)
    } finally {
      await ctx.close()
    }
  })

  test("in A's call, Cancel keeps A's call and A's page", async ({ browser }) => {
    const { ctx, page } = await signedIn(browser)
    try {
      await joinA(page)

      await activeCallsTo(page, 'Room B')
      const didAsk = await asked(page, 'call-switch-cancel')
      if (didAsk) await page.getByTestId('call-switch-cancel').click()

      await expect(page).toHaveURL(callUrl(roomA))
      await expect(title(page)).toHaveText('Room A')
      expect(await callUserIds(ownerToken, tenantId, roomA), "the server lists the owner in A's call").toContain(ownerId)
      expect(await callUserIds(ownerToken, tenantId, roomB), "the server does not list the owner in B's call").not.toContain(
        ownerId,
      )
      expect(didAsk, 'the page asks before it leaves a call').toBe(true)
    } finally {
      await ctx.close()
    }
  })

  test("on A's call page in no call, the page becomes B's and joins nothing", async ({ browser }) => {
    const { ctx, page } = await signedIn(browser)
    try {
      await page.goto(`/tenant/${tenantId}/room/${roomA}/call`)
      await expect(page.getByRole('button', { name: 'Join Meeting' })).toBeVisible({ timeout: 10_000 })
      await expect(title(page)).toHaveText('Room A')

      await activeCallsTo(page, 'Room B')

      await expect(page).toHaveURL(callUrl(roomB))
      await expect(title(page)).toHaveText('Room B')
      await expect(page.getByRole('button', { name: 'Join Meeting' })).toBeVisible()
      await expect(page.getByTestId('call-switch-confirm')).toHaveCount(0)
      expect(await callUserIds(ownerToken, tenantId, roomA)).not.toContain(ownerId)
      expect(await callUserIds(ownerToken, tenantId, roomB)).not.toContain(ownerId)
    } finally {
      await ctx.close()
    }
  })
})
