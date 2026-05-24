/**
 * Phase 3 — file-DC upload smoke against the Phase 1 `agent-e2e`
 * harness. Promotes the env-gated `remote-upload-pc50045.spec.ts`
 * (which targets PROD with a `.cred` file + an externally-supplied
 * `E2E_UPLOAD_FILE`) into a CI spec that drives the in-cluster
 * fixture agents with a synthesised payload.
 *
 * **Skip-conditions** (mirror remote-session-smoke.spec.ts so the
 * spec ships safely before the cluster harness is bootstrapped):
 *  - `E2E_AGENT_E2E_TENANT_ID` env unset → seed Job hasn't been
 *    run + Secret rebaked.
 *  - No online agent → overlay isn't applied or Pods are still
 *    booting.
 *
 * **What it validates**:
 *  1. The same Connect → phase-chip-"connected" handshake as Phase 2.
 *  2. The file-DC v2 toolbar accepts a file via the hidden
 *     `<input type="file">`.
 *  3. The Transfers panel shows a row matching the upload filename
 *     within 30 s of submission.
 *  4. The transfer reaches `complete` within 2 min — a 1 MB synthetic
 *     payload over the in-cluster network typically takes <2 s, but
 *     SCTP slow-start + the rc.21 SCTP 16 KiB chunk cap make the
 *     ceiling generous.
 *
 * **NOT validated yet**:
 *  - SHA-256 match on the agent-side written file. Needs a test-
 *    only HTTP endpoint on the agent behind a build flag — out of
 *    scope for the first Phase 3 cut. Without it, this spec proves
 *    "the wire protocol completes" but not "the bytes arrived
 *    correctly". A bit-flip in transit would still pass.
 *  - Clipboard round-trip. The agent-e2e Pod has no X server, so
 *    arboard's Linux backend can't connect to a clipboard
 *    regardless of how the controller writes. Needs an Xvfb sidecar
 *    or a separate Windows agent (deferred to Phase 6's installer-
 *    smoke pack).
 */
import { test, expect, type Page } from '@playwright/test'
import * as fs from 'node:fs'
import * as os from 'node:os'
import * as path from 'node:path'

const API_URL = process.env.E2E_API_URL || 'http://localhost:5001'
const BASE_URL = process.env.E2E_BASE_URL || 'http://localhost:5000'
const TENANT_ID = process.env.E2E_AGENT_E2E_TENANT_ID || ''
const ADMIN_EMAIL = process.env.E2E_AGENT_E2E_ADMIN_EMAIL || 'agent-e2e-admin@roomler.local'
const ADMIN_PASSWORD =
  process.env.E2E_AGENT_E2E_ADMIN_PASSWORD || 'agent-e2e-bootstrap-pw-2026'

/** Same discovery helper as remote-session-smoke.spec.ts. Inline-
 *  duplicated rather than extracted into test-helpers.ts to keep
 *  each Phase 2/3 spec self-contained for `git log --follow`. */
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

/**
 * Generate a 1 MB pseudo-random binary in the runner's tmpdir.
 * Pseudo-random (deterministic seed) so a future Phase 3 follow-on
 * that adds the agent-side SHA-256 check can recompute the expected
 * hash without exchanging the file out-of-band.
 */
function makeUploadPayload(): { path: string; size: number; name: string } {
  const size = 1024 * 1024 // 1 MiB
  const buf = Buffer.alloc(size)
  // Deterministic byte pattern — LCG seeded with 0xDEADBEEF. Not a
  // crypto-quality PRNG; we only need "not all-zeros" so the
  // encoder can't degenerate to a constant-stream optimisation
  // (analogous to the synthetic-frame backend's anti-flat-image
  // posture).
  let s = 0xdeadbeef
  for (let i = 0; i < size; i++) {
    s = (s * 1664525 + 1013904223) >>> 0
    buf[i] = s & 0xff
  }
  const name = `upload-smoke-${Date.now()}.bin`
  const filePath = path.join(os.tmpdir(), name)
  fs.writeFileSync(filePath, buf)
  return { path: filePath, size, name }
}

test.describe('Remote file-DC upload smoke against the agent-e2e harness', () => {
  test.skip(
    !TENANT_ID,
    'E2E_AGENT_E2E_TENANT_ID must be set to the rebaked tenant_id from the seed Job.'
  )
  test.setTimeout(3 * 60 * 1000)

  test('upload a 1 MiB payload to a synthetic-frame agent and wait for complete', async ({
    page,
    context,
  }) => {
    const token = await adminLogin()
    const agent = await findOnlineAgent(token)
    test.skip(!agent, 'no online enrolled agent found — apply the agent-e2e overlay first')
    const agentId = agent!.id

    await context.addInitScript((tok) => {
      window.localStorage.setItem('access_token', tok)
      window.localStorage.setItem('refresh_token', tok)
    }, token)

    const remoteUrl = `${BASE_URL}/tenant/${TENANT_ID}/agent/${agentId}/remote`
    await page.goto(remoteUrl)

    await expect(
      page.getByRole('button', { name: /^connect$/i }).first()
    ).toBeVisible({ timeout: 30_000 })
    await page.getByRole('button', { name: /^connect$/i }).first().click()
    await expect(page.locator('text=/^connected$/i').first()).toBeVisible({
      timeout: 60_000,
    })

    // Build + submit the payload.
    const payload = makeUploadPayload()
    try {
      const fileInput = page.locator('input[type="file"]').first()
      await fileInput.setInputFiles(payload.path)

      // Transfers panel: one row per in-flight transfer, labelled
      // by filename. Confirm our row appears before assuming the
      // upload was accepted at all (vs the file input being a
      // separate "open image" path that doesn't go through the DC).
      const uploadName = payload.name
      const escapedName = uploadName.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')
      const transferRow = page.locator(`text=/${escapedName}/i`).first()
      await expect(
        transferRow,
        'transfer row never appeared in Transfers panel'
      ).toBeVisible({ timeout: 30_000 })

      // Poll for completion. Per remote-upload-pc50045.spec.ts there's
      // no terminal `exhausted` state any more — only `complete` or
      // `error`. With a 1 MiB payload over in-cluster network the
      // expected dwell is well under 30 s; 2 min ceiling absorbs
      // CI vCPU jitter + cold-start.
      const completionDeadline = Date.now() + 2 * 60 * 1000
      let lastStatus = ''
      while (Date.now() < completionDeadline) {
        const completePill = page.locator('text=/complete/i').first()
        const errorPill = page.locator('text=/^error$/i').first()
        if (await completePill.isVisible().catch(() => false)) {
          lastStatus = 'complete'
          break
        }
        if (await errorPill.isVisible().catch(() => false)) {
          lastStatus = 'error'
          break
        }
        await page.waitForTimeout(1000)
      }

      expect(lastStatus, 'transfer did not reach complete within 2 min').toBe('complete')
    } finally {
      // Clean up the tmpfile regardless of pass/fail.
      try {
        fs.unlinkSync(payload.path)
      } catch {
        // best-effort
      }
    }
  })
})
