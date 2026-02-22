import { type Page, expect } from '@playwright/test'

const API_URL = process.env.E2E_API_URL || 'http://localhost:5001'

let userCounter = 0

/** Generate a unique test user for each test */
export function uniqueUser() {
  userCounter++
  const id = `e2e_${Date.now()}_${userCounter}`
  return {
    email: `${id}@test.local`,
    username: id,
    displayName: `E2E User ${userCounter}`,
    password: 'TestPass123!',
  }
}

/** Register a user via the API and return credentials */
export async function registerUserViaApi(user: ReturnType<typeof uniqueUser>) {
  const resp = await fetch(`${API_URL}/api/auth/register`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({
      email: user.email,
      username: user.username,
      password: user.password,
      display_name: user.displayName,
    }),
  })
  if (!resp.ok) throw new Error(`Register failed: ${resp.status}`)
  return (await resp.json()) as { access_token: string; user: { id: string } }
}

/** Create a tenant via the API */
export async function createTenantViaApi(token: string, name: string, slug: string) {
  const resp = await fetch(`${API_URL}/api/tenant`, {
    method: 'POST',
    headers: {
      'Content-Type': 'application/json',
      Authorization: `Bearer ${token}`,
    },
    body: JSON.stringify({ name, slug }),
  })
  if (!resp.ok) throw new Error(`Create tenant failed: ${resp.status}`)
  return (await resp.json()) as { id: string; name: string; slug: string }
}

/** Create a room via the API */
export async function createRoomViaApi(
  token: string,
  tenantId: string,
  name: string,
  isOpen = true,
  options: { media_settings?: Record<string, unknown>; parent_id?: string } = {},
) {
  const resp = await fetch(`${API_URL}/api/tenant/${tenantId}/room`, {
    method: 'POST',
    headers: {
      'Content-Type': 'application/json',
      Authorization: `Bearer ${token}`,
    },
    body: JSON.stringify({ name, is_open: isOpen, ...options }),
  })
  if (!resp.ok) throw new Error(`Create room failed: ${resp.status}`)
  return (await resp.json()) as { id: string; name: string; path: string; has_media: boolean; parent_id?: string }
}

/** End a call in a room via the API */
export async function endCallViaApi(token: string, tenantId: string, roomId: string) {
  const resp = await fetch(`${API_URL}/api/tenant/${tenantId}/room/${roomId}/call/end`, {
    method: 'POST',
    headers: { Authorization: `Bearer ${token}` },
  })
  if (!resp.ok) throw new Error(`End call failed: ${resp.status}`)
  return resp.json()
}

/** Login through the UI */
export async function loginViaUi(page: Page, username: string, password: string) {
  await page.goto('/login')
  await page.locator('input').first().fill(username)
  await page.locator('input[type="password"]').fill(password)
  await page.getByRole('button', { name: /login/i }).click()
  await expect(page).toHaveURL(/\/$/, { timeout: 5000 })
}

/** Join a room via the API */
export async function joinRoomViaApi(token: string, tenantId: string, roomId: string) {
  const resp = await fetch(`${API_URL}/api/tenant/${tenantId}/room/${roomId}/join`, {
    method: 'POST',
    headers: { Authorization: `Bearer ${token}` },
  })
  if (!resp.ok) throw new Error(`Join room failed: ${resp.status}`)
  return resp.json()
}

/** Start a call in a room via the API */
export async function startCallViaApi(token: string, tenantId: string, roomId: string) {
  const resp = await fetch(`${API_URL}/api/tenant/${tenantId}/room/${roomId}/call/start`, {
    method: 'POST',
    headers: { Authorization: `Bearer ${token}` },
  })
  if (!resp.ok) throw new Error(`Start call failed: ${resp.status}`)
  return resp.json()
}

/** Join a call in a room via the API */
export async function joinCallViaApi(token: string, tenantId: string, roomId: string) {
  const resp = await fetch(`${API_URL}/api/tenant/${tenantId}/room/${roomId}/call/join`, {
    method: 'POST',
    headers: { Authorization: `Bearer ${token}` },
  })
  if (!resp.ok) throw new Error(`Join call failed: ${resp.status}`)
  return (await resp.json()) as { member_id: string; joined: boolean }
}

/** Send a message via the API */
export async function sendMessageViaApi(
  token: string,
  tenantId: string,
  roomId: string,
  content: string,
) {
  const resp = await fetch(`${API_URL}/api/tenant/${tenantId}/room/${roomId}/message`, {
    method: 'POST',
    headers: {
      'Content-Type': 'application/json',
      Authorization: `Bearer ${token}`,
    },
    body: JSON.stringify({ content }),
  })
  if (!resp.ok) throw new Error(`Send message failed: ${resp.status}`)
  return (await resp.json()) as { id: string; content: string; author_id: string }
}

/** Add a user to a tenant via the API */
export async function addTenantMemberViaApi(
  token: string,
  tenantId: string,
  userId: string,
) {
  const resp = await fetch(`${API_URL}/api/tenant/${tenantId}/member`, {
    method: 'POST',
    headers: {
      'Content-Type': 'application/json',
      Authorization: `Bearer ${token}`,
    },
    body: JSON.stringify({ user_id: userId }),
  })
  if (!resp.ok) throw new Error(`Add tenant member failed: ${resp.status}`)
  return resp.json()
}

/** Create an invite via the API */
export async function createInviteViaApi(
  token: string,
  tenantId: string,
  options: { target_email?: string; max_uses?: number; expires_in_hours?: number } = {},
) {
  const resp = await fetch(`${API_URL}/api/tenant/${tenantId}/invite`, {
    method: 'POST',
    headers: {
      'Content-Type': 'application/json',
      Authorization: `Bearer ${token}`,
    },
    body: JSON.stringify(options),
  })
  if (!resp.ok) throw new Error(`Create invite failed: ${resp.status}`)
  return (await resp.json()) as { id: string; code: string; status: string }
}

/** Accept an invite via the API */
export async function acceptInviteViaApi(token: string, code: string) {
  const resp = await fetch(`${API_URL}/api/invite/${code}/accept`, {
    method: 'POST',
    headers: { Authorization: `Bearer ${token}` },
  })
  if (!resp.ok) throw new Error(`Accept invite failed: ${resp.status}`)
  return (await resp.json()) as { tenant_id: string; tenant_name: string; tenant_slug: string }
}

/** Revoke an invite via the API */
export async function revokeInviteViaApi(token: string, tenantId: string, inviteId: string) {
  const resp = await fetch(`${API_URL}/api/tenant/${tenantId}/invite/${inviteId}`, {
    method: 'DELETE',
    headers: { Authorization: `Bearer ${token}` },
  })
  if (!resp.ok) throw new Error(`Revoke invite failed: ${resp.status}`)
  return resp.json()
}

/** Register through the UI */
export async function registerViaUi(
  page: Page,
  email: string,
  username: string,
  displayName: string,
  password: string,
) {
  await page.goto('/register')
  const inputs = page.locator('input')
  await inputs.nth(0).fill(email)
  await inputs.nth(1).fill(username)
  await inputs.nth(2).fill(displayName)
  await page.locator('input[type="password"]').fill(password)
  await page.getByRole('button', { name: /register/i }).click()
  await expect(page).toHaveURL(/\/$/, { timeout: 5000 })
}
