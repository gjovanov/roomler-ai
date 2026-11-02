/**
 * FR-135 R2: what a page says when it has no picture, on a real session against a named vmtest device.
 *
 * It changes nothing in the product. It connects, waits, and reports what a person would see: how many frames were
 * painted, the words on the overlay, and how many sessions the page asked for. The device decides whether a picture
 * can come, so the same spec reads a black page with no reason (RED) and one that names it (GREEN). A screenshot goes
 * to MP_SHOT, for the PR.
 *
 * Optional env:
 *   MP_WAIT_MS          how long to watch, default 30000: past the media watchdog's 10 s, so a re-created session shows
 *   MP_EXPECT_REASON    a regex the overlay must match
 *   MP_EXPECT_PAINTS    `0`: no frame may paint. `1`: a frame must paint
 *   MP_EXPECT_PLAN      a regex a logged `[rc] media plan` line must match
 *   MP_EXPECT_NO_PLAN   `1`: no such line may appear, for an older agent
 *   MP_EXPECT_NO_BLINK  `1`: once the page says why it has no picture, it says so until the end of the wait
 *   MP_EXPECT_NO_BLAME  `1`: the page must not blame the device's network
 *   MP_EXPECT_HZ        a regex a logged `[rc] viewer_hz` line must match
 *   MP_EXPECT_VERB      `1` or `0`: whether the agent's row must advertise `media-plan`
 *   MP_UI_DIST        serve the viewer from this `dist` directory at the production origin, for a read before a promote
 *   MP_SHOT             where the screenshot goes
 *   E2E_RC_CODEC        the codec picker's value for this agent
 */
import { test, expect } from '@playwright/test'
import { existsSync, readFileSync, statSync } from 'node:fs'
import { extname, join, normalize } from 'node:path'

const API_URL = process.env.E2E_API_URL || ''
const BASE_URL = process.env.E2E_BASE_URL || ''
const TENANT_ID = process.env.E2E_VMTEST_TENANT_ID || ''
const EMAIL = process.env.E2E_VMTEST_EMAIL || ''
const PASSWORD = process.env.E2E_VMTEST_PASSWORD || ''
const AGENT_NAME = process.env.E2E_AGENT_NAME || ''
const RC_CODEC = process.env.E2E_RC_CODEC || ''
const WAIT_MS = Number(process.env.MP_WAIT_MS || '30000')
const EXPECT_REASON = process.env.MP_EXPECT_REASON || ''
const EXPECT_PAINTS = process.env.MP_EXPECT_PAINTS || ''
const EXPECT_PLAN = process.env.MP_EXPECT_PLAN || ''
const EXPECT_NO_PLAN = process.env.MP_EXPECT_NO_PLAN === '1'
const EXPECT_NO_BLINK = process.env.MP_EXPECT_NO_BLINK === '1'
const EXPECT_NO_BLAME = process.env.MP_EXPECT_NO_BLAME === '1'
const EXPECT_HZ = process.env.MP_EXPECT_HZ || ''
const EXPECT_VERB = process.env.MP_EXPECT_VERB || ''
const UI_DIST = process.env.MP_UI_DIST || ''
const SHOT = process.env.MP_SHOT || ''
const CODEC_STORAGE_PREFIX = 'roomler-rc-codec.v1:'
const WORKER_FILE = /\/rc-(vp9-444|hevc)-worker[^/]*\.js$/

/** The two reasons a page gives for having no picture, on the connected stage or through a redial. */
const REASON = '[data-testid="rc-media-stop"], [data-testid="rc-media-silent"]'
const NOTICE = /No video from this device[^\n]*/

const say = (s: string) => console.log(`[mp-spec] ${new Date().toISOString()} ${s}`)

/**
 * Runs in the page: logs every time the reason leaves the DOM, on the mutation and not on a poll, so a redial shorter
 * than any sampling still counts.
 */
function watchTheReason(selector: string) {
  let shown = false
  new MutationObserver(() => {
    const now = document.querySelector(selector) !== null
    if (shown && !now) console.info('[mp] reason gone')
    shown = now
  }).observe(document, { childList: true, subtree: true })
}

/** Prepended to the decode worker: counts paints on the data-channel path. */
const SHIM = `;(() => {
  const proto = self.OffscreenCanvasRenderingContext2D && self.OffscreenCanvasRenderingContext2D.prototype;
  if (!proto) return;
  const origDraw = proto.drawImage;
  let painted = false;
  proto.drawImage = function (...a) {
    if (!painted) { painted = true; console.info('[mp] first paint'); }
    return origDraw.apply(this, a);
  };
})();
`

const CONTENT_TYPES: Record<string, string> = {
  '.html': 'text/html', '.js': 'text/javascript', '.css': 'text/css', '.json': 'application/json', '.svg': 'image/svg+xml',
  '.png': 'image/png', '.ico': 'image/x-icon', '.woff2': 'font/woff2', '.wasm': 'application/wasm', '.webmanifest': 'application/manifest+json',
}

async function findNamedOnlineAgent(token: string): Promise<string | null> {
  for (let i = 0; i < 24; i++) {
    for (let page = 1, pages = 1; page <= pages; page++) {
      const resp = await fetch(`${API_URL}/api/tenant/${TENANT_ID}/agent?per_page=100&page=${page}`, {
        headers: { Authorization: `Bearer ${token}` },
      })
      if (!resp.ok) break
      const body = (await resp.json()) as {
        total_pages?: number
        items?: Array<{ id: string; machine_name?: string; name?: string; display_name?: string; is_online: boolean }>
      }
      pages = body.total_pages ?? 1
      const hit = body.items?.find(
        (a) => a.is_online && (a.machine_name === AGENT_NAME || a.name === AGENT_NAME || a.display_name === AGENT_NAME),
      )
      if (hit) return hit.id
    }
    await new Promise((r) => setTimeout(r, 5000))
  }
  return null
}

test.use({ serviceWorkers: 'block' })

test('FR-135 R2: what a page says about its picture', async ({ context, browserName }) => {
  test.setTimeout(120_000 + WAIT_MS)
  test.skip(!API_URL || !TENANT_ID || !EMAIL || !PASSWORD || !AGENT_NAME, 'vmtest env not set')

  const login = await context.request.post(`${API_URL}/api/auth/login`, { data: { email: EMAIL, password: PASSWORD } })
  expect(login.ok(), `login failed: ${login.status()}`).toBeTruthy()
  const token = ((await login.json()) as { access_token: string }).access_token
  const agentId = await findNamedOnlineAgent(token)
  expect(agentId, `agent "${AGENT_NAME}" never reported online`).toBeTruthy()
  const row = (await (
    await fetch(`${API_URL}/api/tenant/${TENANT_ID}/agent/${agentId}`, { headers: { Authorization: `Bearer ${token}` } })
  ).json()) as { capabilities?: { rpc?: string[] } }
  const verb = row.capabilities?.rpc?.includes('media-plan') === true

  await context.addInitScript(
    ([id, codec, prefix]: string[]) => {
      try {
        window.localStorage.setItem('roomler-signed-in', '1')
        window.localStorage.setItem(`roomler-rc-display-match.v2:${id}`, '0')
        if (codec) window.localStorage.setItem(`${prefix}${id}`, codec)
      } catch {
        /* the cookie still authenticates */
      }
    },
    [agentId as string, RC_CODEC, CODEC_STORAGE_PREFIX],
  )
  await context.addInitScript(watchTheReason, REASON)
  await context.route('https://purestat.ai/**', (route) => route.abort())
  await context.route(`${BASE_URL}/**`, async (route) => {
    const url = new URL(route.request().url())
    if (url.pathname.startsWith('/api/') || url.pathname.startsWith('/ws') || url.pathname.startsWith('/derp')) {
      return route.fallback()
    }
    const isWorker = WORKER_FILE.test(url.pathname)
    if (!UI_DIST) {
      if (!isWorker) return route.fallback()
      const response = await route.fetch()
      return route.fulfill({ response, body: SHIM + (await response.text()) })
    }
    const rel = normalize(decodeURIComponent(url.pathname)).replace(/^[/\\]+/, '')
    const candidate = join(UI_DIST, rel)
    const inDist = candidate.startsWith(normalize(UI_DIST)) && existsSync(candidate) && statSync(candidate).isFile()
    const file = inDist ? candidate : join(UI_DIST, 'index.html')
    const contentType = CONTENT_TYPES[extname(file)] ?? 'application/octet-stream'
    return route.fulfill(
      isWorker && inDist
        ? { status: 200, contentType, body: SHIM + readFileSync(file, 'utf8') }
        : { status: 200, contentType, body: readFileSync(file) },
    )
  })

  const page = await context.newPage()
  const plans: string[] = []
  const hzLines: string[] = []
  const overlayWords = new Set<string>()
  const noticeWords = new Set<string>()
  let painted = 0
  let sessionsAsked = 0
  let blink = 0
  const t0 = Date.now()
  page.on('websocket', (ws) => {
    ws.on('framesent', (f) => {
      if (typeof f.payload === 'string' && f.payload.includes('rc:session.request')) {
        sessionsAsked++
        const t = /"preferred_transport":"([^"]+)"/.exec(f.payload)?.[1] ?? 'none'
        say(`rc:session.request ${sessionsAsked} preferred_transport=${t}`)
      }
    })
  })
  let pageErrors = 0
  page.on('pageerror', (e) => {
    pageErrors++
    say(`pageerror: ${e.message.slice(0, 240)}`)
  })
  page.on('console', (m) => {
    const text = m.text()
    if (text === '[mp] first paint') {
      painted++
      say('first paint on the data-channel path')
    } else if (text === '[mp] reason gone') {
      blink++
      say(`the reason left the page after ${Date.now() - t0} ms`)
    } else if (text.startsWith('[rc] media plan')) {
      plans.push(text)
      say(text.slice(0, 600))
    } else if (text.startsWith('[rc] viewer_hz')) {
      hzLines.push(text)
      say(text.slice(0, 200))
    } else if (text.includes('[rc]') && /stalled|re-creating|decoder|failed|banned|fallback|refus/i.test(text)) {
      say(text.slice(0, 240))
    }
  })

  await page.goto(`${BASE_URL}/tenant/${TENANT_ID}/agent/${agentId}/remote`)
  const connect = page.locator('[data-tour="viewer-connect"]').first()
  await expect(connect).toBeVisible({ timeout: 30_000 })
  say(`browser=${browserName} ui=${UI_DIST ? 'local dist' : 'served'} connect`)
  await connect.click()

  // Every overlay this page can show, read once a second, so one that a re-created session clears still counts. So is
  // the backoff's notice. Whether the reason ever left is `watchTheReason`'s to say: a redial is shorter than this read.
  while (Date.now() - t0 < WAIT_MS) {
    const words = await page
      .locator(`.no-media-overlay, ${REASON}`)
      .allInnerTexts()
      .catch(() => [] as string[])
    for (const w of words) {
      const clean = w.replace(/\s+/g, ' ').trim()
      if (clean && !overlayWords.has(clean)) {
        overlayWords.add(clean)
        say(`overlay after ${Date.now() - t0} ms: ${clean.slice(0, 400)}`)
      }
    }
    const notices = await page
      .getByText(NOTICE)
      .allInnerTexts()
      .catch(() => [] as string[])
    for (const n of notices) {
      if (!noticeWords.has(n)) {
        noticeWords.add(n)
        say(`notice after ${Date.now() - t0} ms: ${n.slice(0, 200)}`)
      }
    }
    await page.waitForTimeout(1_000)
  }

  const video = await page
    .locator('video')
    .first()
    .evaluate((v: HTMLVideoElement) => ({ w: v.videoWidth, t: v.currentTime }))
    .catch(() => ({ w: 0, t: 0 }))
  if (SHOT) await page.screenshot({ path: SHOT }).catch(() => undefined)
  const paints = painted + (video.w > 0 && video.t > 0 ? 1 : 0)
  const overlay = [...overlayWords].join(' | ')
  const notice = [...noticeWords].join(' | ')
  say(`RESULT browser=${browserName} paints=${paints} video=${video.w} sessions=${sessionsAsked} plans=${plans.length} hz=${hzLines.length} blink=${blink} verb=${verb} pageerrors=${pageErrors} overlay="${overlay.slice(0, 500)}" notice="${notice.slice(0, 300)}"`)

  const disconnect = page.locator('button[aria-label="Disconnect"]').first()
  if (await disconnect.isVisible().catch(() => false)) await disconnect.click().catch(() => undefined)
  await page.waitForTimeout(1_500)

  if (EXPECT_REASON) expect(overlay).toMatch(new RegExp(EXPECT_REASON))
  if (EXPECT_PAINTS === '0') expect(paints).toBe(0)
  if (EXPECT_PAINTS === '1') expect(paints).toBeGreaterThan(0)
  if (EXPECT_PLAN) expect(plans.join('\n')).toMatch(new RegExp(EXPECT_PLAN))
  if (EXPECT_NO_PLAN) expect(plans.length).toBe(0)
  if (EXPECT_NO_BLINK) expect(blink).toBe(0)
  if (EXPECT_NO_BLAME) expect(notice).not.toMatch(/network may be blocking/)
  if (EXPECT_HZ) expect(hzLines.join('\n')).toMatch(new RegExp(EXPECT_HZ))
  if (EXPECT_VERB) expect(String(verb)).toBe(EXPECT_VERB === '1' ? 'true' : 'false')
  expect(pageErrors).toBe(0)
})
