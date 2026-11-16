/**
 * #3416 — a call joined before the page's socket has its connection id. The
 * id comes with the socket's `connected` frame. A join sent before it went
 * out with no body, and the server opened a call session with no connection
 * id. The page's leave names its connection, and the server closed only that
 * connection's session, so the person stayed listed and a call they were alone
 * in never ended.
 *
 * Each test holds back everything the server sends on the page's socket,
 * `connected` first, so the click on Join always lands before the id. Runs
 * against a real API with mediasoup. The verdict is what the server lists,
 * not what the page shows.
 *
 *  1. The frame comes 3 s after the click. The join waits for the id and names
 *     it, and Leave takes the person out: the server no longer lists them, and
 *     the call they were alone in ends.
 *  2. The frame never comes. Join is refused with a message the person sees,
 *     and nothing is started or joined.
 */
import { test, expect, type Browser, type BrowserContext, type Page, type Request } from '@playwright/test'
import { uniqueUser, registerUserViaApi, createTenantViaApi, createRoomViaApi, loginViaUi } from './fixtures/test-helpers'

const API_URL = process.env.E2E_API_URL || 'http://localhost:5001'

async function apiGet<T>(token: string, path: string): Promise<T> {
  const resp = await fetch(`${API_URL}/api${path}`, { headers: { Authorization: `Bearer ${token}` } })
  if (!resp.ok) throw new Error(`GET ${path} failed: ${resp.status}`)
  return (await resp.json()) as T
}

/** The user ids the server lists in a room's call. */
async function callUserIds(token: string, tenantId: string, roomId: string): Promise<string[]> {
  const items = await apiGet<{ user_id: string | null }[]>(token, `/tenant/${tenantId}/room/${roomId}/call/participant`)
  return items.flatMap((p) => (p.user_id ? [p.user_id] : []))
}

async function conferenceStatus(token: string, tenantId: string, roomId: string): Promise<string | null> {
  const room = await apiGet<{ conference_status?: string | null }>(token, `/tenant/${tenantId}/room/${roomId}`)
  return room.conference_status ?? null
}

/**
 * Routes every app socket the context opens from now on through the test, and
 * holds back what the server sends on it until `release()`. Each socket keeps
 * its own queue, in order, so no socket is handed another one's `connected`
 * frame and with it a dead id. The page's own frames reach the server as
 * usual.
 */
async function holdServerFrames(ctx: BrowserContext): Promise<() => void> {
  let released = false
  const flushes: (() => void)[] = []
  await ctx.routeWebSocket(/\/ws(\?|$)/, (ws) => {
    const server = ws.connectToServer()
    const queue: (string | Buffer)[] = []
    const toPage = (m: string | Buffer) => {
      try {
        ws.send(m)
      } catch {
        // That page is gone.
      }
    }
    server.onMessage((m) => {
      if (released) toPage(m)
      else queue.push(m)
    })
    flushes.push(() => queue.splice(0).forEach(toPage))
  })
  return () => {
    released = true
    flushes.forEach((flush) => flush())
  }
}

const isCallPost = (r: Request, action: 'start' | 'join' | 'leave') =>
  r.method() === 'POST' && new URL(r.url()).pathname.endsWith(`/call/${action}`)

test.describe('A call joined before the socket has its id (#3416)', () => {
  let user: ReturnType<typeof uniqueUser>
  let token: string
  let userId: string
  let tenantId: string
  let roomId: string

  test.beforeEach(async () => {
    user = uniqueUser()
    const registered = await registerUserViaApi(user)
    token = registered.access_token
    userId = registered.user.id
    tenantId = (await createTenantViaApi(token, 'Fast Join Org', `fastjoin-${Date.now()}`)).id
    roomId = (await createRoomViaApi(token, tenantId, 'Fast Join Room')).id
  })

  async function onCallPage(browser: Browser) {
    const ctx = await browser.newContext({ permissions: ['camera', 'microphone'] })
    await ctx.route('https://purestat.ai/**', (r) => r.abort())
    const page = await ctx.newPage()
    await loginViaUi(page, user.username, user.password)
    // Only the call page's socket is held: the one login opened is not routed.
    const release = await holdServerFrames(ctx)
    await page.goto(`/tenant/${tenantId}/room/${roomId}/call`)
    await expect(page.getByRole('button', { name: 'Join Meeting' })).toBeVisible({ timeout: 10_000 })
    return { ctx, page, release }
  }

  const leaveButton = (page: Page) => page.locator('button:has(.mdi-phone-hangup):visible')

  test('a join clicked before the id names it, and the leave takes the person out', async ({ browser }) => {
    const { ctx, page, release } = await onCallPage(browser)
    try {
      const join = page.waitForRequest((r) => isCallPost(r, 'join'), { timeout: 30_000 })
      await page.getByRole('button', { name: 'Join Meeting' }).click()
      // Hold the frame until the page has joined without it, or for 3 s.
      await Promise.race([join.catch(() => undefined), new Promise((r) => setTimeout(r, 3_000))])
      release()
      const joinBody = (await join).postDataJSON() as { connection_id?: string } | null

      await expect(page.getByText('You')).toBeVisible({ timeout: 15_000 })
      await expect.poll(() => callUserIds(token, tenantId, roomId)).toContain(userId)

      await leaveButton(page).click()

      await expect
        .poll(() => callUserIds(token, tenantId, roomId), { message: 'the server still lists the person after they left' })
        .not.toContain(userId)
      await expect
        .poll(() => conferenceStatus(token, tenantId, roomId), { message: 'the call they were alone in did not end' })
        .toBe('ended')
      expect(joinBody?.connection_id, 'the join named no connection').toBeTruthy()
    } finally {
      await ctx.close()
    }
  })

  test('a socket that never gets its id: Join is refused, and nothing is joined', async ({ browser }) => {
    const { ctx, page } = await onCallPage(browser)
    try {
      const callPosts: string[] = []
      page.on('request', (r) => {
        for (const action of ['start', 'join'] as const) if (isCallPost(r, action)) callPosts.push(action)
      })
      await page.getByRole('button', { name: 'Join Meeting' }).click()

      await expect(page.getByText(/Not connected to the server/)).toBeVisible({ timeout: 15_000 })
      expect(callPosts, 'the page started or joined a call without its socket').toEqual([])
      expect(await callUserIds(token, tenantId, roomId)).not.toContain(userId)
      expect(await conferenceStatus(token, tenantId, roomId)).not.toBe('in_progress')
    } finally {
      await ctx.close()
    }
  })
})
