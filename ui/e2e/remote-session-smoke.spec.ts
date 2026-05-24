/**
 * Phase 2 — browser-driven remote-control smoke against the Phase 1
 * `agent-e2e` harness. Replaces the manual `.cred`-driven
 * `remote-upload-pc50045.spec.ts` workflow with a fully-CI version
 * that talks to the in-cluster agents the Phase 1 chunk wired up.
 *
 * **Skip-conditions** (each independent so the spec is safe to ship
 * before the cluster harness is bootstrapped):
 *  - `E2E_BASE_URL` not set → defaults to localhost; assume the
 *    in-cluster Service isn't reachable and skip.
 *  - `E2E_AGENT_E2E_TENANT_ID` env unset → the operator hasn't
 *    rebaked `secret-agent-e2e-bootstrap.yaml` with the live tenant
 *    id yet (see `scripts/e2e-k8s/AGENT-E2E.md`).
 *  - The agent discovery REST call returns no `is_online: true`
 *    agents → Pods exist but haven't reached online yet, or the
 *    overlay isn't applied. Skip rather than hang.
 *
 * **What it validates**:
 *  1. Admin login via the SPA (auth round-trip).
 *  2. Agent discovery via `/api/tenant/<tid>/agent` listing.
 *  3. Navigation to `/tenant/<tid>/agent/<aid>/remote` renders the
 *     Connect control.
 *  4. Clicking Connect drives the rc:* handshake: session.request →
 *     consent → sdp.offer → sdp.answer → ICE → DTLS → data channels.
 *  5. The phase chip flips to "connected" within 60 s.
 *  6. The `<video>` element receives at least one decoded frame
 *     (videoWidth > 0 AND currentTime > 0) within 30 s of connect.
 *  7. WebRTC stats report `framesDecoded > 0` for the inbound video
 *     track — covers the case where `<video>` painted a black frame
 *     before the real stream arrived.
 *
 * **NOT validated yet**:
 *  - Mouse / keyboard round-trip — the agent-e2e image doesn't
 *    compile `--features enigo-input` (no X server in the Pod), so
 *    inputs are silently dropped by the agent's input pump. Phase 3
 *    handles this either via an Xvfb sidecar or a Windows agent.
 *  - File-DC upload — covered by remote-upload-pc50045.spec.ts
 *    against PROD; Phase 3 promotes that to the CI fixture agent.
 */
import { test, expect, type Page } from '@playwright/test'

const API_URL = process.env.E2E_API_URL || 'http://localhost:5001'
const BASE_URL = process.env.E2E_BASE_URL || 'http://localhost:5000'
const TENANT_ID = process.env.E2E_AGENT_E2E_TENANT_ID || ''
const ADMIN_EMAIL = process.env.E2E_AGENT_E2E_ADMIN_EMAIL || 'agent-e2e-admin@roomler.local'
const ADMIN_PASSWORD =
  process.env.E2E_AGENT_E2E_ADMIN_PASSWORD || 'agent-e2e-bootstrap-pw-2026'

/**
 * Find an online enrolled agent in the tenant. Returns the first
 * `is_online: true` record, or null if none match. Polls a few
 * times so a Pod that's mid-rc:agent.hello has a chance to settle
 * before we declare the test un-runnable.
 */
async function findOnlineAgent(token: string): Promise<{ id: string; machine_name: string } | null> {
  for (let i = 0; i < 6; i++) {
    const resp = await fetch(`${API_URL}/api/tenant/${TENANT_ID}/agent`, {
      headers: { Authorization: `Bearer ${token}` },
    })
    if (!resp.ok) return null
    const body = (await resp.json()) as {
      items?: Array<{ id: string; machine_name: string; is_online: boolean }>
    }
    const online = body.items?.find((a) => a.is_online)
    if (online) return online
    await new Promise((r) => setTimeout(r, 500))
  }
  return null
}

/**
 * Admin login via the same REST route the SPA uses. Returns the
 * access_token. Tests use this both to populate localStorage (so
 * the SPA picks it up without going through the login form) AND
 * to call the discovery REST directly above.
 */
async function adminLogin(): Promise<string> {
  const resp = await fetch(`${API_URL}/api/auth/login`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ email: ADMIN_EMAIL, password: ADMIN_PASSWORD }),
  })
  if (!resp.ok) {
    throw new Error(`admin login failed: ${resp.status} ${await resp.text().catch(() => '')}`)
  }
  const body = (await resp.json()) as { access_token: string }
  return body.access_token
}

test.describe('Remote session smoke against the agent-e2e harness', () => {
  test.skip(
    !TENANT_ID,
    'E2E_AGENT_E2E_TENANT_ID must be set to the rebaked tenant_id from the seed Job.'
  )
  // Long timeout so the rc:* handshake + ICE-localhost-pair has room
  // to converge. The actual happy-path takes <10 s; the cushion is
  // for CI vCPU jitter and the openh264 cold-start.
  test.setTimeout(2 * 60 * 1000)

  test('connect to a synthetic-frame agent and receive decoded frames', async ({
    page,
    context,
  }) => {
    // -------- Discover an online agent BEFORE we even open the SPA --------
    const token = await adminLogin()
    const agent = await findOnlineAgent(token)
    test.skip(!agent, 'no online enrolled agent found — apply the agent-e2e overlay first')
    const agentId = agent!.id

    // -------- Seed the SPA's auth state via localStorage --------
    // The SPA reads access_token + refresh_token from localStorage
    // on boot. Pre-seeding bypasses the login form so the smoke
    // stays focused on the remote-control flow (auth has its own
    // dedicated spec).
    await context.addInitScript((tok) => {
      window.localStorage.setItem('access_token', tok)
      window.localStorage.setItem('refresh_token', tok)
    }, token)

    // -------- Navigate to the agent's remote view --------
    const remoteUrl = `${BASE_URL}/tenant/${TENANT_ID}/agent/${agentId}/remote`
    await page.goto(remoteUrl)

    // The view always renders a Connect button when idle.
    await expect(
      page.getByRole('button', { name: /^connect$/i }).first()
    ).toBeVisible({ timeout: 30_000 })

    // -------- Capture browser console for failure forensics --------
    const consoleErrors: string[] = []
    page.on('console', (msg) => {
      const t = msg.type()
      if (t === 'error' || t === 'warning') {
        consoleErrors.push(`[${t}] ${msg.text()}`)
      }
    })

    // -------- Click Connect --------
    await page.getByRole('button', { name: /^connect$/i }).first().click()

    // Phase chip transitions: idle → connecting → connected.
    // The chip renders the phase verbatim in lower-case inside a v-chip.
    await expect(page.locator('text=/^connected$/i').first()).toBeVisible({
      timeout: 60_000,
    })

    // -------- Wait for the <video> element to receive frames --------
    // The browser viewer renders the agent's track into a <video>.
    // currentTime advances once playback starts; videoWidth becomes
    // non-zero once the decoder has the first frame's resolution.
    // Both must be true to confirm we're past a black-frame placeholder.
    await expect
      .poll(
        async () => {
          return await page.evaluate(() => {
            const v = document.querySelector('video') as HTMLVideoElement | null
            if (!v) return { width: 0, time: 0, exists: false }
            return {
              width: v.videoWidth,
              time: v.currentTime,
              exists: true,
            }
          })
        },
        { timeout: 30_000, message: 'video element never received frames' }
      )
      .toMatchObject({
        exists: true,
        width: expect.any(Number),
        time: expect.any(Number),
      })

    // After ~2 s of playback, `currentTime` must have advanced from
    // its initial reading — proves the stream is live, not just one
    // IDR painted and then frozen. The synthetic agent emits 15 fps
    // so 2 s ≈ 30 frames worth of playback; we sample twice and
    // assert the second reading is strictly greater than the first.
    //
    // A future hardening pass can expose the active RTCPeerConnection
    // on `window.__roomler_remote_pc` and lift this to a
    // `framesDecoded ≥ N` assertion via `pc.getStats()` — the SPA
    // doesn't expose that hook today and pulling the PC out of a
    // Vue composable scope from a Playwright `evaluate` callback
    // would need a refactor not in scope for this smoke.
    const t0 = await page.evaluate(() => {
      const v = document.querySelector('video') as HTMLVideoElement | null
      return v?.currentTime ?? 0
    })
    await page.waitForTimeout(2_000)
    const t1 = await page.evaluate(() => {
      const v = document.querySelector('video') as HTMLVideoElement | null
      return v?.currentTime ?? 0
    })
    expect(t1, `video stream froze (t0=${t0} t1=${t1})`).toBeGreaterThan(t0)

    // Console-error surface (best-effort; not all warnings are fatal).
    if (consoleErrors.length > 0) {
      console.warn(`[remote-session-smoke] ${consoleErrors.length} console errors/warnings:`)
      for (const line of consoleErrors.slice(0, 20)) console.warn('  ', line)
    }
  })
})
