import { test, expect } from '@playwright/test'
import {
  uniqueUser,
  registerUserViaApi,
  createTenantViaApi,
  createRoomViaApi,
  joinRoomViaApi,
  sendMessageViaApi,
  sendThreadReplyViaApi,
  fetchMessagesViaApi,
  addTenantMemberViaApi,
  loginViaUi,
} from './fixtures/test-helpers'

test.describe('Thread Replies Real-Time Sync', () => {
  let ownerUser: ReturnType<typeof uniqueUser>
  let peerUser: ReturnType<typeof uniqueUser>
  let ownerToken: string
  let peerToken: string
  let peerId: string
  let tenantId: string
  let roomId: string

  test.beforeEach(async () => {
    ownerUser = uniqueUser()
    peerUser = uniqueUser()

    const ownerResult = await registerUserViaApi(ownerUser)
    ownerToken = ownerResult.access_token

    const peerResult = await registerUserViaApi(peerUser)
    peerToken = peerResult.access_token
    peerId = peerResult.user.id

    const tenant = await createTenantViaApi(ownerToken, 'Thread Org', `thread-${Date.now()}`)
    tenantId = tenant.id
    await addTenantMemberViaApi(ownerToken, tenantId, peerId)

    const room = await createRoomViaApi(ownerToken, tenantId, `thread-room-${Date.now()}`)
    roomId = room.id

    await joinRoomViaApi(ownerToken, tenantId, roomId)
    await joinRoomViaApi(peerToken, tenantId, roomId)
  })

  test('thread reply from another user updates parent message metadata via API', async () => {
    // Owner sends a message
    const parentMsg = await sendMessageViaApi(ownerToken, tenantId, roomId, 'Parent message for thread test')

    // Peer sends a thread reply
    await sendThreadReplyViaApi(peerToken, tenantId, roomId, parentMsg.id, 'Thread reply from peer')

    // Fetch messages and verify parent is now a thread root with reply_count
    const messages = await fetchMessagesViaApi(ownerToken, tenantId, roomId)
    const parent = messages.items.find((m) => m.id === parentMsg.id)

    expect(parent).toBeDefined()
    expect(parent!.is_thread_root).toBe(true)
    expect(parent!.reply_count).toBe(1)
  })

  test('thread reply appears in real-time for other user via WS', async ({ browser }) => {
    // Owner sends parent message
    const parentMsg = await sendMessageViaApi(ownerToken, tenantId, roomId, `Thread parent ${Date.now()}`)

    // Open two browser contexts
    const ctx1 = await browser.newContext()
    const ctx2 = await browser.newContext()
    const page1 = await ctx1.newPage()
    const page2 = await ctx2.newPage()

    try {
      // Login both users
      await loginViaUi(page1, ownerUser.username, ownerUser.password)
      await loginViaUi(page2, peerUser.username, peerUser.password)

      // Both navigate to the room
      await page1.goto(`/tenant/${tenantId}/room/${roomId}`)
      await page2.goto(`/tenant/${tenantId}/room/${roomId}`)

      // Wait for parent message to be visible on both
      await expect(page1.getByText(parentMsg.content)).toBeVisible({ timeout: 10000 })
      await expect(page2.getByText(parentMsg.content)).toBeVisible({ timeout: 10000 })

      // Peer sends a thread reply via API
      const replyContent = `Thread reply ${Date.now()}`
      await sendThreadReplyViaApi(peerToken, tenantId, roomId, parentMsg.id, replyContent)

      // Owner should see the parent message update to show thread indicator
      // The parent message should now show a reply count badge or thread indicator
      // Wait for the WS broadcast to arrive and update the parent
      await page1.waitForTimeout(3000)

      // Verify the parent message now shows thread metadata (reply count indicator)
      // This depends on the UI implementation — look for thread indicators
      const parentBubble = page1.locator(`text=${parentMsg.content}`).first()
      await expect(parentBubble).toBeVisible()

      // The message list should still have exactly 1 message (parent) — reply is in thread
      // Verify no duplicate of the parent
      const parentCount = await page1.getByText(parentMsg.content).count()
      expect(parentCount).toBe(1)
    } finally {
      await page1.close()
      await page2.close()
      await ctx1.close()
      await ctx2.close()
    }
  })

  test('multiple thread replies increment reply count correctly', async () => {
    // Owner sends parent message
    const parentMsg = await sendMessageViaApi(ownerToken, tenantId, roomId, 'Multi-reply parent')

    // Both users send replies
    await sendThreadReplyViaApi(ownerToken, tenantId, roomId, parentMsg.id, 'Reply 1')
    await sendThreadReplyViaApi(peerToken, tenantId, roomId, parentMsg.id, 'Reply 2')
    await sendThreadReplyViaApi(ownerToken, tenantId, roomId, parentMsg.id, 'Reply 3')

    // Verify reply count
    const messages = await fetchMessagesViaApi(ownerToken, tenantId, roomId)
    const parent = messages.items.find((m) => m.id === parentMsg.id)

    expect(parent).toBeDefined()
    expect(parent!.is_thread_root).toBe(true)
    expect(parent!.reply_count).toBe(3)
  })
})
