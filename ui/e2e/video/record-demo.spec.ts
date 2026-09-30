// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * The product demo: real machines, one browser tab, each desktop full screen.
 *
 * For each device in `E2E_DEMO_DEVICES` (display names, in order) the take
 * opens its remote page with the sidebar collapsed, presses Connect, waits for
 * the desktop to paint, and enters the viewer's fullscreen, so the machine
 * fills the frame. `scripts/record-demo.sh` cuts the result into the README
 * MP4 and GIF; captions and the title cards are added there, not here.
 *
 * ⚠️ The capture is the DevTools screencast, NOT Playwright's `video` option.
 * Playwright encodes its video at a fixed ~1 Mbit/s VP8, which turns a remote
 * desktop's text into mush at any size worth showing; the screencast hands
 * over every composited frame as a JPEG at the quality we ask for, with its
 * timestamp, and the cut is encoded once, at the end, from those.
 *
 * ⚠️ NOTHING may touch page JS once a stream is painting. The page's main
 * thread is saturated decoding and painting, so `evaluate`, `boundingBox` and
 * every locator query hang rather than answer — four takes of the earlier demo
 * were lost to checks that could never return while the desktop streamed
 * perfectly. Geometry is taken BEFORE Connect; afterwards it is CDP input only.
 * That is why "connected" is read from the screencast (a painted desktop is a
 * far heavier JPEG than the connecting page) rather than asked of the page.
 *
 * ⚠️ The fullscreen button exists only once the session is connected, and
 * until then the SAME spot holds Disconnect. Clicking there on a guess would
 * end the session it was meant to show, so the click happens only after the
 * paint is seen; a device that never paints is filmed in the page instead.
 *
 * ⚠️ Nothing filmed may name the org or the fleet. The device list, the
 * dashboard and the network pages all do (machine names, MagicDNS names, other
 * people's devices), so the take never visits them: it goes straight to each
 * device's own page, where the header shows only its name (relabelled, see
 * LABELS), the OS and the version.
 */
import { test, expect, type CDPSession, type Page } from '@playwright/test'
import { mkdirSync, writeFile, writeFileSync } from 'node:fs'
import { join, resolve } from 'node:path'

const USERNAME = process.env.E2E_USERNAME || ''
const PASSWORD = process.env.E2E_PASSWORD || ''
const TENANT_ID = process.env.E2E_TENANT_ID || ''
const DEVICES = (process.env.E2E_DEMO_DEVICES || '')
  .split(',')
  .map((s) => s.trim())
  .filter(Boolean)
/**
 * Where the frames and the manifest go.
 *
 * ⚠️ Keep it OUTSIDE `e2e/video/output`: Playwright empties its outputDir at
 * the start of every run, so a take kept there is gone the moment anything
 * else runs. `record-demo.sh` points this at the user's Videos folder.
 */
const OUT = resolve(process.env.E2E_DEMO_OUT || 'e2e/video/output/take')
/**
 * The name each device shows ON SCREEN, one per device, `|`-separated.
 *
 * ⚠️ A display name is how the owner tells machines apart, which is exactly
 * why it does not belong in a public video: "Anna's work laptop" says whose
 * machine it is and where. The take rewrites the name in the agent API
 * responses the PAGE receives,
 * so the app renders the label through its own code, in its own font, on every
 * re-render; nothing on the server changes, and the harness's own lookup
 * (`page.request`, which page routes do not touch) still finds the device by
 * its real name.
 */
const LABELS = (process.env.E2E_DEMO_LABELS || '').split('|').map((s) => s.trim())

/** The frame the cut is made at. 1080p is what YouTube and the MP4 want. */
const W = 1920
const H = 1080

/** How long each desktop is held full screen. */
const DWELL_MS = Number(process.env.E2E_DEMO_DWELL_MS || 7000)
/** Longest wait for a desktop to paint after Connect. */
const PAINT_BUDGET_MS = Number(process.env.E2E_DEMO_PAINT_BUDGET_MS || 45_000)

test.use({ video: 'off', viewport: { width: W, height: H }, deviceScaleFactor: 1 })

type Frame = { file: string; t: number; bytes: number }
type Mark = { device: string; what: string; t: number }

/** Every composited frame, as a JPEG on disk with its swap timestamp. */
class Capture {
  readonly frames: Frame[] = []
  private n = 0

  constructor(
    private readonly cdp: CDPSession,
    private readonly dir: string,
  ) {}

  async start() {
    this.cdp.on('Page.screencastFrame', (ev) => {
      const file = `f${String(this.n++).padStart(6, '0')}.jpg`
      const buf = Buffer.from(ev.data, 'base64')
      writeFile(join(this.dir, file), buf, () => {})
      this.frames.push({ file, t: ev.metadata.timestamp ?? Date.now() / 1000, bytes: buf.length })
      // Unacked, the screencast stops after a few frames.
      this.cdp.send('Page.screencastFrameAck', { sessionId: ev.sessionId }).catch(() => {})
    })
    await this.cdp.send('Page.startScreencast', {
      format: 'jpeg',
      quality: 92,
      maxWidth: W,
      maxHeight: H,
      everyNthFrame: 1,
    })
  }

  async stop() {
    await this.cdp.send('Page.stopScreencast').catch(() => {})
  }

  /** Median JPEG size of the frames since `sinceT` (seconds), or of the last frame. */
  baseline(sinceT: number): number {
    const recent = this.frames.filter((f) => f.t >= sinceT).map((f) => f.bytes)
    const pool = recent.length ? recent : this.frames.slice(-1).map((f) => f.bytes)
    if (!pool.length) return 0
    pool.sort((a, b) => a - b)
    return pool[Math.floor(pool.length / 2)]
  }

  /**
   * Resolve once a desktop has painted: several consecutive frames far heavier
   * than the page was before Connect. False when the budget runs out.
   */
  async waitForPaint(baseline: number, sinceT: number, budgetMs: number): Promise<boolean> {
    const threshold = baseline * 1.6 + 40_000
    const deadline = Date.now() + budgetMs
    while (Date.now() < deadline) {
      const after = this.frames.filter((f) => f.t > sinceT)
      let run = 0
      for (const f of after) {
        run = f.bytes > threshold ? run + 1 : 0
        if (run >= 4) return true
      }
      await new Promise((r) => setTimeout(r, 150))
    }
    return false
  }
}

/**
 * Suppress transient toasts for the whole take. They appear DURING a stream —
 * "Connected in 9.1 s, slower than usual", the clipboard-permission prompt —
 * which is exactly when the page cannot be asked to dismiss them, so they are
 * hidden by CSS installed before the app's own JS runs, on every navigation.
 */
async function suppressToasts(page: Page) {
  await page.addInitScript(() => {
    const install = () => {
      const s = document.createElement('style')
      s.textContent =
        '.v-snackbar,.v-snackbar__wrapper,.kb-lock-toast,.kb-lock-pill{display:none !important}'
      ;(document.head || document.documentElement).appendChild(s)
    }
    if (document.head) install()
    else document.addEventListener('DOMContentLoaded', install, { once: true })
  })
}

/** Rail mode is not persisted, so every full page load needs it again. */
async function collapseSidebar(page: Page) {
  const btn = page.getByRole('button', { name: 'Collapse sidebar' })
  if (await btn.isVisible().catch(() => false)) await btn.click()
}

/** A slow, deliberate glide — reads as a hand on a mouse, not a teleport. */
async function glide(page: Page, points: Array<[number, number]>, stepsPer = 40, pauseMs = 320) {
  for (const [x, y] of points) {
    await page.mouse.move(x, y, { steps: stepsPer })
    await page.waitForTimeout(pauseMs)
  }
}

test.describe('Roomler demo recording', () => {
  test('record the product demo', async ({ page }) => {
    test.setTimeout(900_000)

    expect(USERNAME, 'E2E_USERNAME is required').not.toBe('')
    expect(PASSWORD, 'E2E_PASSWORD is required').not.toBe('')
    expect(TENANT_ID, 'E2E_TENANT_ID is required').not.toBe('')
    expect(DEVICES.length, 'E2E_DEMO_DEVICES is required (display names, comma-separated)').toBeGreaterThan(0)

    const framesDir = join(OUT, 'frames')
    mkdirSync(framesDir, { recursive: true })

    await suppressToasts(page)
    await page.goto('/login')
    await page.waitForLoadState('networkidle')
    const user = page.locator('input').first()
    await user.click()
    await user.pressSequentially(USERNAME, { delay: 25 })
    const pass = page.locator('input[type="password"]')
    await pass.click()
    await pass.pressSequentially(PASSWORD, { delay: 25 })
    await page.getByRole('button', { name: /sign in|log in|login/i }).click()
    await page.waitForTimeout(3000)

    // Resolve each device by display name, and refuse an offline one now: a
    // take that films a spinner is a wasted run that reports success.
    const res = await page.request.get(`/api/tenant/${TENANT_ID}/agent?per_page=100`)
    expect(res.ok(), `agent list failed: ${res.status()}`).toBeTruthy()
    const body = await res.json()
    const agents: Array<Record<string, unknown>> = body.items ?? body ?? []
    const shots = DEVICES.map((name, i) => {
      const a = agents.find((x) => String(x.display_name ?? x.name ?? '') === name)
      expect(a, `no device with display name "${name}" in this org`).toBeTruthy()
      expect(a?.is_online, `"${name}" is offline`).toBeTruthy()
      // `shown` is what the take and its manifest call the device: the label
      // when one is given, so the real name never lands in the take folder.
      return { name, shown: LABELS[i] || name, id: String(a?.id), os: String(a?.os ?? '') }
    })

    const label = new Map(shots.map((s, i) => [s.id, LABELS[i] || '']).filter(([, l]) => l))
    if (label.size) {
      const relabel = (a: Record<string, unknown>) => {
        const l = label.get(String(a?.id))
        // Both names: the header shows the machine name beside the display
        // name whenever the viewer's preference says so.
        if (l) Object.assign(a, { display_name: l, name: l })
      }
      await page.route(
        (url) => url.pathname.startsWith(`/api/tenant/${TENANT_ID}/agent`),
        async (route) => {
          const path = new URL(route.request().url()).pathname
          const list = path === `/api/tenant/${TENANT_ID}/agent`
          if (route.request().method() !== 'GET' || !(list || /\/agent\/[0-9a-f]{24}$/.test(path))) {
            return route.continue()
          }
          const resp = await route.fetch()
          const json = await resp.json().catch(() => null)
          if (!json) return route.fulfill({ response: resp })
          if (list) for (const a of json.items ?? json ?? []) relabel(a)
          else relabel(json)
          await route.fulfill({ response: resp, json })
        },
      )
    }

    const cdp = await page.context().newCDPSession(page)
    const cap = new Capture(cdp, framesDir)
    const marks: Mark[] = []
    const mark = (device: string, what: string) => {
      marks.push({ device, what, t: Date.now() / 1000 })
      console.log(`  ${device.padEnd(14)} ${what}`)
    }
    await cap.start()

    for (const shot of shots) {
      await page.goto(`/tenant/${TENANT_ID}/agent/${shot.id}/remote`)
      await page.waitForLoadState('networkidle')
      await collapseSidebar(page)
      const connect = page.getByRole('button', { name: /^connect$/i }).first()
      await connect.waitFor({ state: 'visible', timeout: 15_000 })
      const box = await connect.boundingBox()
      expect(box, 'Connect has no box').toBeTruthy()
      // Park the pointer in the empty stage, away from any hover state.
      await page.mouse.move(W * 0.55, H * 0.6)
      await page.waitForTimeout(1500)
      mark(shot.shown, 'ready')

      const before = Date.now() / 1000
      const baseline = cap.baseline(before - 1.5)
      await connect.click()
      mark(shot.shown, 'connect')

      // ── from here: no page queries, CDP input only ─────────────────────────
      // ⚠️ A connected session can stream a BLACK screen: a laptop whose
      // display has gone to sleep sends black frames at a few kbit/s until
      // something wakes it, and waiting for a paint without touching the
      // mouse then waits forever (the first take did exactly that, 45 s of
      // "connected · 12 kbps" over a black stage). So the wait nudges the
      // pointer a few pixels in the middle of the stage: before the session
      // is up that goes nowhere, after it the host wakes.
      let painted = false
      const deadline = Date.now() + PAINT_BUDGET_MS
      for (let k = 0; !painted && Date.now() < deadline; k++) {
        await page.mouse.move(W * 0.55 + (k % 2 ? 8 : -8), H * 0.6, { steps: 4 })
        painted = await cap.waitForPaint(baseline, before + 0.2, 2500)
      }
      mark(shot.shown, painted ? 'painted' : 'no-paint')
      if (!painted) {
        await page.waitForTimeout(2000)
        mark(shot.shown, 'end')
        continue
      }
      await page.waitForTimeout(1800) // the desktop, still inside the page
      // Fullscreen is the LAST toolbar button once connected; before Connect
      // that end of the toolbar was Connect's own right edge.
      await page.mouse.click(box!.x + box!.width - 20, box!.y + box!.height / 2)
      mark(shot.shown, 'fullscreen')
      await page.waitForTimeout(1200)

      // A slow pass through the middle of the screen: no clicks, no typing,
      // nothing near a taskbar, a dock or a hot corner.
      mark(shot.shown, 'glide')
      await glide(page, [
        [W * 0.42, H * 0.42],
        [W * 0.58, H * 0.36],
        [W * 0.62, H * 0.55],
        [W * 0.46, H * 0.6],
        [W * 0.5, H * 0.48],
      ])
      await page.waitForTimeout(Math.max(0, DWELL_MS - 4000))
      mark(shot.shown, 'end')
    }

    await cap.stop()
    await page.goto('about:blank')
    writeFileSync(
      join(OUT, 'take.json'),
      JSON.stringify({ viewport: { width: W, height: H }, devices: shots.map((s) => ({ name: s.shown, os: s.os })), marks, frames: cap.frames }, null, 1),
    )
    console.log(`\n  ${cap.frames.length} frames → ${framesDir}`)
    expect(marks.filter((m) => m.what === 'painted').length, 'no device painted').toBeGreaterThan(0)
  })
})
