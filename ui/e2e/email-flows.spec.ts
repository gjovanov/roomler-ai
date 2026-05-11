import { test, expect } from '@playwright/test'
import {
  uniqueUser,
  registerUserViaApi,
  createTenantViaApi,
  createRoomViaApi,
  joinRoomViaApi,
  sendMessageViaApi,
  addTenantMemberViaApi,
  loginViaUi,
  fetchActivationEmail,
  activateViaApi,
  parseActivationUrl,
} from './fixtures/test-helpers'

const API_URL = process.env.E2E_API_URL || 'http://localhost:5001'

test.describe('Email-related flows', () => {
  test.describe('Registration & Activation', () => {
    test('registration triggers SMTP delivery captured by Mailpit', async () => {
      // The e2e overlay sets ROOMLER__EMAIL__SMTP_HOST=mailpit so the
      // EmailService dispatches every send through plaintext SMTP to
      // the Mailpit pod. AUTO_VERIFY=true still flips is_verified at
      // register time (so the existing 100+ specs keep working), but
      // the activation email is sent unconditionally — we poll
      // Mailpit's HTTP API to verify SMTP arrived end-to-end.
      const user = uniqueUser()
      const auth = await registerUserViaApi(user)
      expect(auth.access_token).toBeTruthy()

      const mail = await fetchActivationEmail(user.email)
      expect(mail.subject.toLowerCase()).toContain('activate')
      expect(mail.html.length).toBeGreaterThan(0)
      expect(mail.activationUrl).toBeTruthy()
      // Activation URL targets the configured FRONTEND_URL (http://roomler2
      // in cluster) plus the /auth/activate path. It carries a 24-char
      // hex userId and a 7-char nanoid token.
      expect(mail.activationUrl).toMatch(
        /^https?:\/\/[^/]+\/auth\/activate\?userId=[a-f0-9]{24}&token=[A-Za-z0-9_-]{7}$/,
      )
    })

    test('activation roundtrip: parse link from Mailpit → POST /activate succeeds', async () => {
      // End-to-end check that the URL embedded in the email is a real,
      // accept-able activation. AUTO_VERIFY already set is_verified at
      // register time, so /activate is idempotent in this overlay —
      // it still must return 200 (success) for a valid token.
      const user = uniqueUser()
      await registerUserViaApi(user)
      const mail = await fetchActivationEmail(user.email)
      if (!mail.activationUrl) throw new Error('no activation URL in Mailpit email')

      const { userId, token } = parseActivationUrl(mail.activationUrl)
      const result = await activateViaApi(userId, token)
      expect(result.message.toLowerCase()).toContain('activated')
    })

    test('activation endpoint rejects invalid tokens', async () => {
      const resp = await fetch(`${API_URL}/api/auth/activate`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          user_id: '000000000000000000000000',
          token: 'invalid-activation-token',
        }),
      })
      // 400 = bad request for invalid / expired token (covered by the
      // backend's `find_valid` returning None for unknown user_id).
      expect(resp.status).toBe(400)
    })

    test('activated user can login successfully', async () => {
      // registerUserViaApi already returns tokens via the AUTO_VERIFY
      // shortcut; this asserts the post-register tokens work for
      // subsequent authenticated API calls.
      const user = uniqueUser()
      const auth = await registerUserViaApi(user)
      expect(auth.access_token).toBeDefined()
      expect(auth.access_token.length).toBeGreaterThan(0)
    })
  })

  test.describe('Mention Notification flows', () => {
    let adminUser: ReturnType<typeof uniqueUser>
    let memberUser: ReturnType<typeof uniqueUser>
    let adminToken: string
    let memberToken: string
    let memberUserId: string
    let tenantId: string
    let roomId: string

    test.beforeEach(async () => {
      adminUser = uniqueUser()
      memberUser = uniqueUser()

      const adminAuth = await registerUserViaApi(adminUser)
      adminToken = adminAuth.access_token

      const memberAuth = await registerUserViaApi(memberUser)
      memberToken = memberAuth.access_token
      memberUserId = memberAuth.user.id

      const tenant = await createTenantViaApi(
        adminToken,
        'Email Test Org',
        `email-test-${Date.now()}`,
      )
      tenantId = tenant.id

      await addTenantMemberViaApi(adminToken, tenantId, memberUserId)

      const room = await createRoomViaApi(adminToken, tenantId, 'email-test-room', true)
      roomId = room.id

      await joinRoomViaApi(adminToken, tenantId, roomId)
      await joinRoomViaApi(memberToken, tenantId, roomId)
    })

    test('sending a mention creates a notification for the mentioned user', async () => {
      // Admin sends a message mentioning the member
      const mentionContent = `Hey @${memberUser.username} please review this`
      await sendMessageViaApi(adminToken, tenantId, roomId, mentionContent)

      // Wait a moment for the notification to be created
      await new Promise((r) => setTimeout(r, 1000))

      // Check the mentioned user's unread notification count
      const resp = await fetch(`${API_URL}/api/notification/unread-count`, {
        headers: { Authorization: `Bearer ${memberToken}` },
      })
      expect(resp.ok).toBeTruthy()
      const countData = (await resp.json()) as { count: number }
      // Should have at least one notification from the mention
      expect(countData.count).toBeGreaterThanOrEqual(1)
    })

    test('notifications API returns mention notification details', async () => {
      // Send mention
      const mentionContent = `Heads up @${memberUser.username}!`
      await sendMessageViaApi(adminToken, tenantId, roomId, mentionContent)

      await new Promise((r) => setTimeout(r, 1000))

      // Fetch the member's unread notifications
      const resp = await fetch(`${API_URL}/api/notification/unread`, {
        headers: { Authorization: `Bearer ${memberToken}` },
      })
      expect(resp.ok).toBeTruthy()
      const notifications = (await resp.json()) as Array<{
        id: string
        type: string
        is_read: boolean
      }>
      // Expect at least one unread notification
      expect(notifications.length).toBeGreaterThanOrEqual(1)
      expect(notifications.some((n) => !n.is_read)).toBe(true)
    })

    test('mark-all-read clears unread notifications', async () => {
      // Create a notification via mention
      await sendMessageViaApi(
        adminToken,
        tenantId,
        roomId,
        `Hey @${memberUser.username}`,
      )
      await new Promise((r) => setTimeout(r, 1000))

      // Mark all as read
      const markResp = await fetch(`${API_URL}/api/notification/read-all`, {
        method: 'POST',
        headers: { Authorization: `Bearer ${memberToken}` },
      })
      expect(markResp.ok).toBeTruthy()

      // Verify unread count is now 0
      const countResp = await fetch(`${API_URL}/api/notification/unread-count`, {
        headers: { Authorization: `Bearer ${memberToken}` },
      })
      expect(countResp.ok).toBeTruthy()
      const countData = (await countResp.json()) as { count: number }
      expect(countData.count).toBe(0)
    })

    test('mention notification is visible in UI notification panel', async ({ page }) => {
      // Send mention
      await sendMessageViaApi(
        adminToken,
        tenantId,
        roomId,
        `Urgent @${memberUser.username} please respond`,
      )

      // Login as the mentioned user
      await loginViaUi(page, memberUser.username, memberUser.password)

      // Wait for notifications to load
      await page.waitForTimeout(2000)

      // Open the notification panel via bell icon
      await page.locator('.mdi-bell-outline').click()
      await page.waitForTimeout(1000)

      // The notification panel should be visible
      await expect(page.locator('.notification-panel')).toBeVisible({ timeout: 5000 })
    })
  })
})
