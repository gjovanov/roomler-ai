/**
 * Roomler Intro Video Recording
 *
 * This Playwright test records a full user journey as a video.
 * It injects an on-screen transcription overlay at each scene,
 * creating a narrated walkthrough suitable for a product intro.
 *
 * Run:
 *   cd ui && bunx playwright test e2e/video/record-intro.spec.ts --config=playwright.video.config.ts
 *
 * Output:
 *   test-results/record-intro-{hash}/video.webm
 *   Convert to MP4: ffmpeg -i video.webm -c:v libx264 -crf 18 roomler-intro.mp4
 */
import { test, type Page } from '@playwright/test'
import transcriptions from './transcriptions.json'

const API_URL = process.env.E2E_API_URL || 'http://localhost:5001'

// ---------------------------------------------------------------------------
// Overlay helpers
// ---------------------------------------------------------------------------

async function injectOverlay(page: Page) {
  await page.evaluate(() => {
    if (document.getElementById('rm-overlay')) return

    const overlay = document.createElement('div')
    overlay.id = 'rm-overlay'
    Object.assign(overlay.style, {
      position: 'fixed',
      bottom: '40px',
      left: '50%',
      transform: 'translateX(-50%)',
      zIndex: '99999',
      background: 'rgba(15, 23, 42, 0.88)',
      color: '#E0F2F1',
      padding: '16px 32px',
      borderRadius: '12px',
      fontSize: '22px',
      fontFamily: "'Inter', 'Segoe UI', system-ui, sans-serif",
      fontWeight: '500',
      letterSpacing: '0.01em',
      maxWidth: '720px',
      textAlign: 'center',
      backdropFilter: 'blur(8px)',
      border: '1px solid rgba(0, 150, 136, 0.3)',
      boxShadow: '0 8px 32px rgba(0, 0, 0, 0.4)',
      opacity: '0',
      transition: 'opacity 0.4s ease',
      pointerEvents: 'none',
    })
    document.body.appendChild(overlay)
  })
}

async function showCaption(page: Page, text: string) {
  await page.evaluate((t) => {
    const el = document.getElementById('rm-overlay')
    if (!el) return
    el.textContent = t
    el.style.opacity = '1'
  }, text)
}

async function hideCaption(page: Page) {
  await page.evaluate(() => {
    const el = document.getElementById('rm-overlay')
    if (el) el.style.opacity = '0'
  })
}

async function caption(page: Page, scene: number) {
  const t = transcriptions.find((s) => s.scene === scene)
  if (!t) return
  await showCaption(page, t.text)
  await page.waitForTimeout(t.duration)
  await hideCaption(page)
  await page.waitForTimeout(400) // fade-out gap
}

function delay(page: Page, ms: number) {
  return page.waitForTimeout(ms)
}

// ---------------------------------------------------------------------------
// Video recording test
// ---------------------------------------------------------------------------

test.describe('Roomler Intro Video', () => {
  test('record full intro walkthrough', async ({ page, context }) => {
    test.setTimeout(300_000) // 5 minutes max

    // Grant camera/mic permissions for conference scene
    await context.grantPermissions(['camera', 'microphone'])

    // Unique suffix to avoid conflicts with previous runs
    const suffix = Date.now().toString().slice(-6)

    await injectOverlay(page)

    // -----------------------------------------------------------------------
    // Scene 1: Landing page
    // -----------------------------------------------------------------------
    await page.goto('/landing')
    await page.waitForLoadState('networkidle')
    await injectOverlay(page)
    await delay(page, 800)
    await caption(page, 1)

    // Scroll through features smoothly
    await page.evaluate(() => {
      document.getElementById('features')?.scrollIntoView({ behavior: 'smooth' })
    })
    await delay(page, 1500)

    // Scroll to pricing
    await page.evaluate(() => {
      document.getElementById('pricing')?.scrollIntoView({ behavior: 'smooth' })
    })
    await delay(page, 1500)

    // Scroll back to top
    await page.evaluate(() => window.scrollTo({ top: 0, behavior: 'smooth' }))
    await delay(page, 1000)

    // -----------------------------------------------------------------------
    // Scene 2: Register
    // -----------------------------------------------------------------------
    await page.getByRole('link', { name: 'Get Started Free' }).first().click()
    await page.waitForURL('**/register')
    await page.waitForLoadState('networkidle')
    await injectOverlay(page)
    await delay(page, 500)
    await caption(page, 2)

    // Fill registration form with visible typing
    const inputs = page.locator('input')
    const emailInput = inputs.nth(0)
    await emailInput.click()
    await emailInput.pressSequentially(`demo${suffix}@roomler.live`, { delay: 60 })
    await delay(page, 300)

    const usernameInput = inputs.nth(1)
    await usernameInput.click()
    await usernameInput.pressSequentially(`demo${suffix}`, { delay: 60 })
    await delay(page, 300)

    const displayNameInput = inputs.nth(2)
    await displayNameInput.click()
    await displayNameInput.pressSequentially('Alex Demo', { delay: 60 })
    await delay(page, 300)

    const passwordInput = page.locator('input[type="password"]')
    await passwordInput.click()
    await passwordInput.pressSequentially('SecureP@ss123', { delay: 60 })
    await delay(page, 500)

    await page.getByRole('button', { name: /register/i }).click()
    await page.waitForURL('**/', { timeout: 15_000 })
    await delay(page, 1000)

    // -----------------------------------------------------------------------
    // Scene 3: Create workspace (Dashboard)
    // -----------------------------------------------------------------------
    await injectOverlay(page)
    await caption(page, 3)

    // Dashboard shows "Create Your First Workspace" form
    const workspaceNameInput = page.locator('input').first()
    await workspaceNameInput.waitFor({ state: 'visible', timeout: 5_000 })
    await workspaceNameInput.click()
    await workspaceNameInput.pressSequentially(`Acme Team ${suffix}`, { delay: 60 })
    await delay(page, 300)

    // Fill slug
    const slugInput = page.locator('input').nth(1)
    await slugInput.click()
    await slugInput.pressSequentially(`acme-${suffix}`, { delay: 60 })
    await delay(page, 500)

    await page.getByRole('button', { name: /create/i }).click()

    // Wait for redirect to tenant dashboard
    await page.waitForURL(/\/tenant\/[^/]+/, { timeout: 15_000 })
    await page.waitForLoadState('networkidle')
    await delay(page, 800)

    // Extract tenantId from URL for subsequent navigation
    const tenantUrl = page.url()
    const tenantId = tenantUrl.match(/\/tenant\/([^/]+)/)?.[1] || ''

    // -----------------------------------------------------------------------
    // Scene 4: Rooms — create rooms
    // -----------------------------------------------------------------------
    await page.goto(`/tenant/${tenantId}/rooms`)
    await page.waitForLoadState('networkidle')
    await injectOverlay(page)
    await delay(page, 500)
    await caption(page, 4)

    // Create "general" room with media
    await page.getByRole('button', { name: /create room/i }).click()
    await delay(page, 500)

    const roomNameInput = page.locator('.v-dialog input').first()
    await roomNameInput.waitFor({ state: 'visible', timeout: 5_000 })
    await roomNameInput.click()
    await roomNameInput.pressSequentially('general', { delay: 60 })
    await delay(page, 300)

    // Toggle "Enable calls" checkbox ON
    const hasMediaCheckbox = page.getByLabel(/enable calls/i)
    if (await hasMediaCheckbox.isVisible({ timeout: 2000 }).catch(() => false)) {
      await hasMediaCheckbox.check()
      await delay(page, 300)
    }

    await page.getByRole('button', { name: /save/i }).click()
    await delay(page, 1000)

    // Wait for room to appear
    await page.getByText('general').first().waitFor({ state: 'visible', timeout: 5_000 })

    // Create "random" room (no media)
    await page.getByRole('button', { name: /create room/i }).click()
    await delay(page, 500)

    const roomNameInput2 = page.locator('.v-dialog input').first()
    await roomNameInput2.waitFor({ state: 'visible', timeout: 5_000 })
    await roomNameInput2.click()
    await roomNameInput2.pressSequentially('random', { delay: 60 })
    await delay(page, 300)

    await page.getByRole('button', { name: /save/i }).click()
    await delay(page, 1000)

    // Wait for room to appear
    await page.getByText('random').first().waitFor({ state: 'visible', timeout: 5_000 })
    await delay(page, 500)

    // -----------------------------------------------------------------------
    // Scene 5: Chat — send messages
    // -----------------------------------------------------------------------
    // Click on "general" room to enter it
    await page.getByText('general').first().click()
    await page.waitForURL(/\/room\/[^/]+/, { timeout: 10_000 })
    await page.waitForLoadState('networkidle')
    await injectOverlay(page)
    await delay(page, 500)
    await caption(page, 5)

    // The message editor uses TipTap (contenteditable div), not a regular input
    const editorArea = page.locator('.editor-content .tiptap').first()
    if (await editorArea.isVisible({ timeout: 5_000 }).catch(() => false)) {
      await editorArea.click()
      await editorArea.pressSequentially('Hello team! This is our new workspace', { delay: 50 })
      await delay(page, 300)
      await page.keyboard.press('Enter')
      await delay(page, 1500)

      // Second message
      await editorArea.click()
      await editorArea.pressSequentially("Let's ship something great together.", { delay: 50 })
      await delay(page, 300)
      await page.keyboard.press('Enter')
      await delay(page, 1500)
    }

    // Extract roomId from URL for call scene
    const chatUrl = page.url()
    const roomId = chatUrl.match(/\/room\/([^/]+)/)?.[1] || ''

    // -----------------------------------------------------------------------
    // Scene 6: Call — start and show conference
    // -----------------------------------------------------------------------
    // Start call via API to ensure conference is available
    if (roomId) {
      // Get a fresh token via API login
      const loginResp = await fetch(`${API_URL}/api/auth/login`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          username: `demo${suffix}`,
          password: 'SecureP@ss123',
        }),
      })
      const loginData = (await loginResp.json()) as { access_token: string }
      const token = loginData.access_token

      await fetch(`${API_URL}/api/tenant/${tenantId}/room/${roomId}/call/start`, {
        method: 'POST',
        headers: { Authorization: `Bearer ${token}` },
      })
    }

    // Click "Start Call" or "Join Call" button in chat toolbar
    const callBtn = page.getByRole('button', { name: /start call|join call/i }).first()
    if (await callBtn.isVisible({ timeout: 3000 }).catch(() => false)) {
      await callBtn.click()
    } else {
      // Navigate directly to call view
      await page.goto(`/tenant/${tenantId}/room/${roomId}/call`)
    }
    await page.waitForLoadState('networkidle')
    await injectOverlay(page)
    await delay(page, 500)
    await caption(page, 6)

    // Click join if we see the "Ready to join?" screen
    const joinBtn = page.getByRole('button', { name: /join/i }).first()
    if (await joinBtn.isVisible({ timeout: 3000 }).catch(() => false)) {
      await joinBtn.click()
      await delay(page, 3000) // Let the conference view load with video tiles
    }

    await delay(page, 1500)

    // -----------------------------------------------------------------------
    // Scene 7: Explore
    // -----------------------------------------------------------------------
    await page.goto(`/tenant/${tenantId}/explore`)
    await page.waitForLoadState('networkidle')
    await injectOverlay(page)
    await delay(page, 500)
    await caption(page, 7)

    // Type in search field
    const searchInput = page.locator('input').first()
    if (await searchInput.isVisible({ timeout: 3000 }).catch(() => false)) {
      await searchInput.click()
      await searchInput.pressSequentially('general', { delay: 60 })
      await delay(page, 1500)
    }

    // -----------------------------------------------------------------------
    // Scene 8: Files
    // -----------------------------------------------------------------------
    await page.goto(`/tenant/${tenantId}/files`)
    await page.waitForLoadState('networkidle')
    await injectOverlay(page)
    await delay(page, 500)
    await caption(page, 8)

    // Pause to show file browser UI (may be empty)
    await delay(page, 500)

    // -----------------------------------------------------------------------
    // Scene 9: Invites
    // -----------------------------------------------------------------------
    await page.goto(`/tenant/${tenantId}/invites`)
    await page.waitForLoadState('networkidle')
    await injectOverlay(page)
    await delay(page, 500)
    await caption(page, 9)

    // Click "Create Invite" button
    const createInviteBtn = page.getByRole('button', { name: /create invite/i })
    if (await createInviteBtn.isVisible({ timeout: 3000 }).catch(() => false)) {
      await createInviteBtn.click()
      await delay(page, 500)

      // Select "Shareable Link" radio if visible
      const shareableRadio = page.getByLabel(/shareable link/i)
      if (await shareableRadio.isVisible({ timeout: 2000 }).catch(() => false)) {
        await shareableRadio.click()
        await delay(page, 300)
      }

      // Click Create in dialog
      const createBtn = page.getByRole('button', { name: /^create$/i }).last()
      if (await createBtn.isVisible({ timeout: 2000 }).catch(() => false)) {
        await createBtn.click()
        await delay(page, 1500)
      }
    }

    // Pause to show invite table
    await delay(page, 500)

    // -----------------------------------------------------------------------
    // Scene 10: Admin
    // -----------------------------------------------------------------------
    await page.goto(`/tenant/${tenantId}/admin`)
    await page.waitForLoadState('networkidle')
    await injectOverlay(page)
    await delay(page, 500)
    await caption(page, 10)

    // Click through sections: Settings → Members → Roles
    const membersItem = page.getByText('Members').first()
    if (await membersItem.isVisible({ timeout: 2000 }).catch(() => false)) {
      await membersItem.click()
      await delay(page, 1200)
    }

    const rolesItem = page.getByText('Roles').first()
    if (await rolesItem.isVisible({ timeout: 2000 }).catch(() => false)) {
      await rolesItem.click()
      await delay(page, 1200)
    }

    const settingsItem = page.getByText('Settings').first()
    if (await settingsItem.isVisible({ timeout: 2000 }).catch(() => false)) {
      await settingsItem.click()
      await delay(page, 800)
    }

    // -----------------------------------------------------------------------
    // Scene 11: Profile
    // -----------------------------------------------------------------------
    await page.goto('/profile/edit')
    await page.waitForLoadState('networkidle')
    await injectOverlay(page)
    await delay(page, 500)
    await caption(page, 11)

    // Fill bio with visible typing
    const bioInput = page.locator('textarea').first()
    if (await bioInput.isVisible({ timeout: 3000 }).catch(() => false)) {
      await bioInput.click()
      await bioInput.pressSequentially('Building the future, one room at a time.', { delay: 50 })
      await delay(page, 1000)
    }

    // -----------------------------------------------------------------------
    // Scene 12: Billing
    // -----------------------------------------------------------------------
    await page.goto(`/tenant/${tenantId}/billing`)
    await page.waitForLoadState('networkidle')
    await injectOverlay(page)
    await delay(page, 500)
    await caption(page, 12)

    // Scroll to show plan cards
    await page.evaluate(() => window.scrollTo({ top: 300, behavior: 'smooth' }))
    await delay(page, 2000)

    // -----------------------------------------------------------------------
    // Scene 13: Theme toggle
    // -----------------------------------------------------------------------
    await page.evaluate(() => window.scrollTo({ top: 0, behavior: 'smooth' }))
    await delay(page, 500)
    await injectOverlay(page)
    await caption(page, 13)

    // Find and click theme toggle button (mdi-weather-night → dark mode)
    const themeBtn = page
      .locator('button:has(.mdi-weather-night)')
      .or(page.locator('button:has(.mdi-weather-sunny)'))
      .first()
    if (await themeBtn.isVisible({ timeout: 2000 }).catch(() => false)) {
      await themeBtn.click()
      await delay(page, 2000) // Pause to show dark theme

      // Toggle back to light
      const themeBtnBack = page
        .locator('button:has(.mdi-weather-sunny)')
        .or(page.locator('button:has(.mdi-weather-night)'))
        .first()
      await themeBtnBack.click()
      await delay(page, 1000)
    }

    // -----------------------------------------------------------------------
    // Scene 14: Closing — back to landing
    // -----------------------------------------------------------------------
    await page.goto('/landing')
    await page.waitForLoadState('networkidle')
    await injectOverlay(page)
    await delay(page, 1000)

    // Show closing caption with logo visible
    await caption(page, 14)
    await delay(page, 1000)
  })
})
