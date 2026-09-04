/**
 * FR-61 (vmtest) — the remote-desktop check for the throwaway-OS install &
 * verify matrix (docs/fr/FR-61-vmtest-matrix.md, #1199).
 *
 * remote-session-smoke.spec.ts discovers "the first online agent", which is
 * correct for the single-agent agent-e2e harness and WRONG for vmtest: a
 * matrix run keeps several throwaway VMs alive in the same org, so this spec
 * selects the agent by EXACT name (`E2E_AGENT_NAME`) and never falls back.
 *
 * Driven by roomler-ai-deploy/vmtest/playwright/run-rd-check.sh from mars
 * against the real server. Asserts, in order:
 *   1. the named agent reports online in the org listing (polls — the VM
 *      enrolled seconds ago),
 *   2. the viewer connects (phase chip "connected"),
 *   3. FRAMES FLOW, twice, via two independent oracles that together cover
 *      every media path:
 *        - `__roomler_remote_pc` → getStats `framesDecoded` (the RTP track
 *          path; the hook remote-session-smoke documents wishing it had),
 *        - `__roomler_remote_stats` → the composable's live fps/bitrate (the
 *          DataChannel pump paths, where inbound-rtp stats are silent).
 *      While sampling, the mouse wiggles over the surface so input-capable
 *      cells produce real motion; input-less cells are still covered by the
 *      idle keepalive re-encode (FR-38), which keeps fps > 0 on a static
 *      desktop.
 *
 * Env (all required; spec skips otherwise): E2E_BASE_URL, E2E_API_URL,
 * E2E_VMTEST_TENANT_ID, E2E_VMTEST_EMAIL, E2E_VMTEST_PASSWORD, E2E_AGENT_NAME.
 */
import { test, expect, type Page } from '@playwright/test'

const API_URL = process.env.E2E_API_URL || ''
const BASE_URL = process.env.E2E_BASE_URL || ''
const TENANT_ID = process.env.E2E_VMTEST_TENANT_ID || ''
const EMAIL = process.env.E2E_VMTEST_EMAIL || ''
const PASSWORD = process.env.E2E_VMTEST_PASSWORD || ''
const AGENT_NAME = process.env.E2E_AGENT_NAME || ''

/** The named agent's id once it reports online. Polls up to ~2 min — the VM
 *  enrolled moments before this spec started. Exact-name match only. */
async function findNamedOnlineAgent(token: string): Promise<string | null> {
  for (let i = 0; i < 24; i++) {
    const resp = await fetch(`${API_URL}/api/tenant/${TENANT_ID}/agent`, {
      headers: { Authorization: `Bearer ${token}` },
    })
    if (resp.ok) {
      const body = (await resp.json()) as {
        items?: Array<{
          id: string
          machine_name?: string
          name?: string
          display_name?: string
          is_online: boolean
        }>
      }
      const hit = body.items?.find(
        (a) =>
          a.is_online &&
          (a.machine_name === AGENT_NAME || a.name === AGENT_NAME || a.display_name === AGENT_NAME),
      )
      if (hit) return hit.id
    }
    await new Promise((r) => setTimeout(r, 5000))
  }
  return null
}

/** Path-agnostic media progress: max(framesDecoded across inbound-rtp video)
 *  and the composable's live fps. Either strictly advancing proves a live
 *  stream. */
async function mediaProgress(page: Page): Promise<{ frames: number; fps: number }> {
  return await page.evaluate(async () => {
    const w = window as unknown as Record<string, unknown>
    const pc = w.__roomler_remote_pc as RTCPeerConnection | undefined
    let frames = -1
    if (pc) {
      const stats = await pc.getStats()
      stats.forEach((r) => {
        const rep = r as unknown as { type?: string; kind?: string; framesDecoded?: number }
        if (rep.type === 'inbound-rtp' && rep.kind === 'video' && rep.framesDecoded !== undefined) {
          frames = Math.max(frames, rep.framesDecoded)
        }
      })
    }
    const statsRef = w.__roomler_remote_stats as { value?: { fps?: number } } | undefined
    const fps = statsRef?.value?.fps ?? -1
    return { frames, fps }
  })
}

/** A signature of whatever surface is actually painting — `<video>` for the
 *  RTP path, the largest `<canvas>` for the DataChannel paths (VP9-444 /
 *  HEVC-over-DC, what a SW-encode agent negotiates, which has NO <video> and
 *  NO inbound-rtp at all). 'none' = nothing live yet. */
async function surfaceSig(page: Page): Promise<string> {
  return await page.evaluate(() => {
    const v = document.querySelector('video') as HTMLVideoElement | null
    if (v && v.videoWidth > 0) return 'v:' + v.currentTime
    const c = (Array.from(document.querySelectorAll('canvas')) as HTMLCanvasElement[])
      .filter((x) => x.width > 100 && x.height > 100)
      .sort((a, b) => b.width * b.height - a.width * a.height)[0]
    if (!c) return 'none'
    try {
      const s = document.createElement('canvas')
      s.width = 32
      s.height = 20
      s.getContext('2d')!.drawImage(c, 0, 0, 32, 20)
      return 'c:' + s.toDataURL().slice(-96)
    } catch {
      return 'tainted'
    }
  })
}

/** Ground truth for ANY transport: a live surface appears and its pixels
 *  CHANGE across a 3 s window while the pointer wiggles. A frozen or absent
 *  stream fails; a streaming one passes whether it rides RTP or a DataChannel. */
async function proveSurfaceLive(page: Page): Promise<void> {
  await expect
    .poll(
      async () => {
        await wiggle(page)
        return await surfaceSig(page)
      },
      { timeout: 60_000, message: 'no live remote surface (video/canvas) appeared' },
    )
    .not.toBe('none')
  const s0 = await surfaceSig(page)
  await wiggle(page)
  await page.waitForTimeout(3_000)
  const s1 = await surfaceSig(page)
  expect(s0, 'remote surface pixels unreadable (tainted)').not.toBe('tainted')
  expect(
    s1 !== s0,
    `remote surface static over 3s — stream frozen or not painting (${s0.slice(0, 24)} → ${s1.slice(0, 24)})`,
  ).toBe(true)
}

/** Wiggle the pointer over the remote surface so input-capable agents
 *  produce real motion (and the input path gets a free smoke). Best-effort:
 *  a cell whose agent cannot inject still passes via keepalive frames. */
async function wiggle(page: Page): Promise<void> {
  const surface = page.locator('video, canvas').first()
  try {
    const box = await surface.boundingBox()
    if (!box) return
    for (let i = 0; i < 6; i++) {
      await page.mouse.move(
        box.x + box.width * (0.3 + 0.07 * i),
        box.y + box.height * (0.4 + 0.05 * (i % 3)),
        { steps: 4 },
      )
      await page.waitForTimeout(120)
    }
  } catch {
    /* surface not interactable — keepalive frames carry the check */
  }
}

test.describe('vmtest remote-desktop check (named agent)', () => {
  test.skip(
    !API_URL || !BASE_URL || !TENANT_ID || !EMAIL || !PASSWORD || !AGENT_NAME,
    'vmtest env not set (E2E_BASE_URL/E2E_API_URL/E2E_VMTEST_TENANT_ID/E2E_VMTEST_EMAIL/E2E_VMTEST_PASSWORD/E2E_AGENT_NAME)',
  )
  test.setTimeout(5 * 60 * 1000)

  test('named throwaway agent streams decoded, advancing frames', async ({ page, context }) => {
    // Sessions are COOKIE-ONLY since #680/#690 — seeding localStorage
    // access/refresh tokens does NOTHING and the SPA shows the login form.
    // Log in through the context's request API so the HttpOnly session cookie
    // lands in the jar shared with this context's pages, and set the
    // `roomler-signed-in` hint so the SPA renders its shell and refreshes via
    // the cookie instead of bouncing to /login.
    const loginResp = await context.request.post(`${API_URL}/api/auth/login`, {
      data: { email: EMAIL, password: PASSWORD },
    })
    expect(loginResp.ok(), `browser login failed: ${loginResp.status()}`).toBeTruthy()
    const token = ((await loginResp.json()) as { access_token: string }).access_token
    await context.addInitScript(() => {
      try {
        window.localStorage.setItem('roomler-signed-in', '1')
      } catch {
        /* private mode / storage blocked — the cookie still authenticates */
      }
    })

    const agentId = await findNamedOnlineAgent(token)
    expect(agentId, `agent "${AGENT_NAME}" never reported online in tenant ${TENANT_ID}`).toBeTruthy()

    const consoleErrors: string[] = []
    page.on('console', (msg) => {
      if (msg.type() === 'error') consoleErrors.push(msg.text())
    })

    await page.goto(`${BASE_URL}/tenant/${TENANT_ID}/agent/${agentId}/remote`)
    await expect(page.getByRole('button', { name: /^connect$/i }).first()).toBeVisible({
      timeout: 30_000,
    })
    await page.getByRole('button', { name: /^connect$/i }).first().click()

    await expect(page.locator('text=/^connected$/i').first()).toBeVisible({ timeout: 90_000 })

    const hooks = await page.evaluate(() => {
      const w = window as unknown as Record<string, unknown>
      return { pc: !!w.__roomler_remote_pc, stats: !!w.__roomler_remote_stats }
    })
    if (!hooks.pc && !hooks.stats) {
      // Viewer build predates the FR-61 hooks (e.g. prod not yet redeployed).
      // Degrade to a TRANSPORT-AGNOSTIC surface oracle: the RTP path paints a
      // <video> (advancing currentTime), the DataChannel paths (VP9-444 /
      // HEVC-over-DC — what a SW-encode agent actually negotiates) paint a
      // <canvas> via WebCodecs with NO <video> at all. Sample whichever surface
      // is live and require it to CHANGE across a 3 s window while the pointer
      // wiggles — a frozen or absent stream fails, a live one passes on any
      // transport. The hooks strengthen this automatically once they ship.
      console.warn('[vmtest-remote] FR-61 hooks absent -- using the surface pixel-change fallback')
      await proveSurfaceLive(page)
      return
    }

    // ── first frames: prefer the counters, but NEVER fail on them alone ────
    // ⚠️ Both counters are RTP-shaped: `getStats` has no inbound-rtp video and
    // `__roomler_remote_stats` is the RTP stats ref, so BOTH read zero on the
    // DataChannel transports even while the viewer paints a perfect stream.
    // Measured 2026-09-04: every Ubuntu cell "failed" RD while the toolbar read
    // `VP9 4:4:4 SW (libvpx) · direct · 28 fps · 9.1 Mbps` and the screenshot
    // showed the live desktop. Counters are an optimisation; the SURFACE is
    // ground truth — fall through to it rather than calling a working product
    // broken.
    const gateDeadline = Date.now() + 60_000
    let countersLive = false
    while (Date.now() < gateDeadline) {
      await wiggle(page)
      const p = await mediaProgress(page)
      if (p.frames > 0 || p.fps > 0) {
        countersLive = true
        break
      }
      await page.waitForTimeout(1_000)
    }
    if (!countersLive) {
      console.warn(
        '[vmtest-remote] RTP counters flat (DataChannel transport?) — proving liveness on the SURFACE',
      )
      await proveSurfaceLive(page)
      return
    }

    // ── liveness: progress across a 3 s window (not one painted frame) ─────
    const s0 = await mediaProgress(page)
    await wiggle(page)
    await page.waitForTimeout(3_000)
    const s1 = await mediaProgress(page)
    const advanced =
      (s1.frames > s0.frames && s1.frames > 0) || (s0.fps > 0 && s1.fps > 0)
    expect(
      advanced,
      `stream froze (framesDecoded ${s0.frames} → ${s1.frames}, fps ${s0.fps} → ${s1.fps})`,
    ).toBe(true)

    if (consoleErrors.length > 0) {
      console.warn(`[vmtest-remote] ${consoleErrors.length} console errors:`)
      for (const line of consoleErrors.slice(0, 10)) console.warn('  ', line)
    }
  })
})
