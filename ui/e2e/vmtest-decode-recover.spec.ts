/**
 * FR-135 R1: the field proof for a decode error and a chunk gap, on a real session against a named vmtest device.
 *
 * It forces the failure from outside the product. The page's decode worker is fetched through a route that prepends
 * `SHIM`, which wraps `VideoDecoder` and the canvas and logs epoch times as `[dr]` console lines. Nothing in the
 * product changes, so the same spec reads RED on an old viewer and GREEN on a new one:
 *
 *   DR_MODE=error  forced through FR-132 AC3's method: a random 2 KB key chunk into the running decoder, which answers
 *                  `Decoding error.` Reads the time from the decoder's error to the next paint in any worker of this
 *                  page, because on the old path that paint comes from the session the watchdog re-created.
 *   DR_MODE=gap    the page drops the first chunk of a multi-chunk frame on its way to the worker, so the worker's
 *                  FR-17 parse reports a gap. Reads how many deltas reached the decoder between the gap and the next
 *                  key frame, and the time from the gap to that key frame's paint.
 *
 * Other optional env:
 *   DR_ROUNDS            forced events in this session, default 3
 *   DR_SPACING_MS        hold between them, default 15000, longer than the old path's 10 s outage
 *   DR_DECODE_RECOVER    set to `0` to store the kill switch before the session
 *   DR_EXPECT_MAX_MS     fail when a round's reading is above this. Unset, the spec only reports.
 *   DR_EXPECT_DELTAS     `0`: fail when a delta reached the decoder inside a gap
 *   DR_UI_DIST           serve the viewer from this `dist` directory at the production origin, for a read before a promote
 *   E2E_RC_CODEC         the codec picker's value for this agent
 *
 * The device must be sending: a still screen sends key frames only when asked, so a gap round needs motion on it.
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
const MODE = process.env.DR_MODE || 'error'
const ROUNDS = Number(process.env.DR_ROUNDS || '3')
const SPACING_MS = Number(process.env.DR_SPACING_MS || '15000')
const SWITCH = process.env.DR_DECODE_RECOVER || ''
const EXPECT_MAX_MS = process.env.DR_EXPECT_MAX_MS ? Number(process.env.DR_EXPECT_MAX_MS) : null
const EXPECT_DELTAS = process.env.DR_EXPECT_DELTAS ? Number(process.env.DR_EXPECT_DELTAS) : null
const UI_DIST = process.env.DR_UI_DIST || ''
const CODEC_STORAGE_PREFIX = 'roomler-rc-codec.v1:'
// FR-135 R3 made the two data-channel workers one, `rc-dc-worker`; the old
// names still match a viewer older than that, so the spec reads either.
const WORKER_FILE = /\/rc-(dc|vp9-444|hevc)-worker[^/]*\.js$/

const say = (s: string) => console.log(`[dr-spec] ${new Date().toISOString()} ${s}`)

/** Prepended to the decode worker. No imports: it runs before the module's own code uses `VideoDecoder`. */
const SHIM = `;(() => {
  const now = () => performance.timeOrigin + performance.now();
  const log = (s) => console.info('[dr] ' + s + ' t=' + now().toFixed(1));
  const Orig = self.VideoDecoder;
  const origDecode = Orig.prototype.decode;
  let current = null;
  let lastTs = 0;
  let paintPending = 'boot';
  let inGap = false;
  let gapDeltas = 0;
  log('boot');
  self.VideoDecoder = class extends Orig {
    constructor(init) {
      super({
        output: (frame) => init.output(frame),
        error: (e) => { paintPending = 'error'; log('error name=' + e.name + ' msg=' + e.message); init.error(e); },
      });
      current = this;
    }
    decode(chunk) {
      lastTs = chunk.timestamp;
      if (inGap) {
        if (chunk.type === 'delta') gapDeltas++;
        else { inGap = false; paintPending = 'gap-key'; log('gap-key deltas=' + gapDeltas); }
      }
      return origDecode.call(this, chunk);
    }
  };
  const proto = self.OffscreenCanvasRenderingContext2D && self.OffscreenCanvasRenderingContext2D.prototype;
  if (proto) {
    const origDraw = proto.drawImage;
    proto.drawImage = function (...a) {
      // After the draw: a draw that stalls is time the person still has no picture.
      const drawn = origDraw.apply(this, a);
      if (paintPending) { const was = paintPending; paintPending = null; log('paint after=' + was); }
      return drawn;
    };
  }
  const origPost = self.postMessage.bind(self);
  self.postMessage = (m, t) => {
    if (m && m.type === 'frame-rejected' && m.reason === 'chunk-gap' && !inGap) { inGap = true; gapDeltas = 0; log('gap'); }
    return origPost(m, t);
  };
  new BroadcastChannel('dr').onmessage = (ev) => {
    if (ev.data !== 'inject' || !current) return;
    const data = crypto.getRandomValues(new Uint8Array(2048));
    log('inject');
    try {
      origDecode.call(current, new EncodedVideoChunk({ type: 'key', timestamp: lastTs + 1, data }));
    } catch (e) {
      log('inject-threw ' + e.name);
    }
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

test('FR-135 R1: a forced decode failure, timed to the next paint', async ({ context, browserName }) => {
  test.setTimeout(120_000 + ROUNDS * (SPACING_MS + 30_000))
  test.skip(!API_URL || !TENANT_ID || !EMAIL || !PASSWORD || !AGENT_NAME, 'vmtest env not set')

  const login = await context.request.post(`${API_URL}/api/auth/login`, { data: { email: EMAIL, password: PASSWORD } })
  expect(login.ok(), `login failed: ${login.status()}`).toBeTruthy()
  const token = ((await login.json()) as { access_token: string }).access_token
  const agentId = await findNamedOnlineAgent(token)
  expect(agentId, `agent "${AGENT_NAME}" never reported online`).toBeTruthy()

  await context.addInitScript(
    ([id, codec, switchValue, prefix, mode]: string[]) => {
      try {
        window.localStorage.setItem('roomler-signed-in', '1')
        window.localStorage.setItem(`roomler-rc-display-match.v2:${id}`, '0')
        if (codec) window.localStorage.setItem(`${prefix}${id}`, codec)
        if (switchValue) window.localStorage.setItem('roomler-rc-decode-recover', switchValue)
        // FR-135 R3 — the gap is dropped on the page's relay; a channel moved
        // into the worker would carry every chunk past this script.
        if (mode === 'gap') window.localStorage.setItem('roomler-rc-dc-worker', '0')
      } catch {
        /* the cookie still authenticates */
      }
      // The gap: the next chunk message that starts a multi-chunk frame never reaches the worker.
      const w = window as unknown as { __drDrop?: boolean; __drDropped?: number }
      w.__drDropped = 0
      const origPost = Worker.prototype.postMessage
      Worker.prototype.postMessage = function (this: Worker, ...args: unknown[]) {
        const m = args[0] as { type?: string; bytes?: ArrayBuffer } | undefined
        if (w.__drDrop && m?.type === 'chunk' && m.bytes && m.bytes.byteLength >= 8) {
          const view = new DataView(m.bytes)
          if (view.getUint16(4, true) === 0 && view.getUint16(6, true) >= 2) {
            w.__drDrop = false
            w.__drDropped!++
            return
          }
        }
        return (origPost as (...a: unknown[]) => void).apply(this, args)
      } as typeof Worker.prototype.postMessage
    },
    [agentId as string, RC_CODEC, SWITCH, CODEC_STORAGE_PREFIX, MODE],
  )
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
  const paints: { t: number; after: string }[] = []
  const errors: number[] = []
  const gapKeys: { t: number; deltas: number }[] = []
  const gaps: number[] = []
  let sessionsAsked = 0
  page.on('websocket', (ws) => {
    ws.on('framesent', (f) => {
      if (typeof f.payload === 'string' && f.payload.includes('rc:session.request')) sessionsAsked++
    })
  })
  page.on('console', (m) => {
    const text = m.text()
    if (text.startsWith('[dr] ')) {
      say(text)
      const t = Number(/ t=([\d.]+)$/.exec(text)?.[1] ?? NaN)
      if (text.startsWith('[dr] paint')) paints.push({ t, after: /after=(\S+)/.exec(text)?.[1] ?? '' })
      else if (text.startsWith('[dr] error')) errors.push(t)
      else if (text.startsWith('[dr] gap-key')) gapKeys.push({ t, deltas: Number(/deltas=(\d+)/.exec(text)?.[1] ?? NaN) })
      else if (text.startsWith('[dr] gap')) gaps.push(t)
    } else if (text.includes('[rc]') && /decod|keyframe|stalled|re-creating|frame rejected|recovering|recovered|fallback|backlog/i.test(text)) {
      say(text.slice(0, 240))
    }
  })

  await page.goto(`${BASE_URL}/tenant/${TENANT_ID}/agent/${agentId}/remote`)
  const connect = page.locator('[data-tour="viewer-connect"]').first()
  await expect(connect).toBeVisible({ timeout: 30_000 })
  say(`browser=${browserName} mode=${MODE} ui=${UI_DIST ? 'local dist' : 'served'} switch=${SWITCH || 'unset'} connect`)
  await connect.click()
  const t0 = Date.now()
  while (paints.length === 0 && Date.now() - t0 < 60_000) await page.waitForTimeout(250)
  expect(paints.length, 'no frame was ever painted').toBeGreaterThan(0)
  say(`first paint ${Date.now() - t0} ms after connect`)
  await page.waitForTimeout(5_000)

  const readings: number[] = []
  const deltas: number[] = []
  for (let round = 1; round <= ROUNDS; round++) {
    if (MODE === 'gap') {
      const before = gapKeys.length
      await page.evaluate(() => {
        ;(window as unknown as { __drDrop?: boolean }).__drDrop = true
      })
      const armed = Date.now()
      while (gapKeys.length === before && Date.now() - armed < 20_000) await page.waitForTimeout(100)
      const key = gapKeys[before]
      const gapAt = gaps.at(-1)
      if (!key || gapAt === undefined) {
        say(`RESULT round=${round} gap=none in 20 s (no multi-chunk frame, or no key frame after it)`)
      } else {
        await page.waitForTimeout(1_000)
        const paint = paints.find((p) => p.t >= key.t)
        const ms = paint ? Math.round(paint.t - gapAt) : -1
        say(`RESULT round=${round} gap_deltas_to_decoder=${key.deltas} gap_to_key_paint_ms=${ms}`)
        readings.push(ms)
        deltas.push(key.deltas)
      }
    } else {
      const before = errors.length
      await page.evaluate(() => new BroadcastChannel('dr').postMessage('inject'))
      const armed = Date.now()
      while (errors.length === before && Date.now() - armed < 10_000) await page.waitForTimeout(25)
      const errorAt = errors[before]
      if (errorAt === undefined) {
        say(`RESULT round=${round} error=none in 10 s (the decoder took the chunk)`)
      } else {
        const waitFrom = Date.now()
        while (!paints.some((p) => p.t > errorAt) && Date.now() - waitFrom < 30_000) await page.waitForTimeout(25)
        const paint = paints.find((p) => p.t > errorAt)
        const ms = paint ? Math.round(paint.t - errorAt) : -1
        say(`RESULT round=${round} error_to_paint_ms=${ms} session_requests=${sessionsAsked}`)
        readings.push(ms)
      }
    }
    await page.waitForTimeout(SPACING_MS)
  }

  say(`SUMMARY browser=${browserName} mode=${MODE} readings=${JSON.stringify(readings)} deltas=${JSON.stringify(deltas)} session_requests=${sessionsAsked}`)
  const disconnect = page.locator('button[aria-label="Disconnect"]').first()
  if (await disconnect.isVisible().catch(() => false)) await disconnect.click().catch(() => undefined)
  await page.waitForTimeout(1_500)

  expect(readings.length, 'no forced event produced a reading').toBeGreaterThan(0)
  if (EXPECT_MAX_MS !== null) {
    for (const ms of readings) {
      expect(ms, 'no paint came').toBeGreaterThanOrEqual(0)
      expect(ms).toBeLessThanOrEqual(EXPECT_MAX_MS)
    }
  }
  if (EXPECT_DELTAS !== null) {
    for (const n of deltas) expect(n).toBeLessThanOrEqual(EXPECT_DELTAS)
  }
})
