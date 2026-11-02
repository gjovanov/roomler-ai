// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * #3287 — a self-hosted call whose media really crosses the compose file's
 * coturn. "coturn is running" and "an allocation succeeded" are both true of a
 * relay that carries nothing, so this asks the browsers: every connected peer
 * connection must have selected a `relay` candidate, and each side must decode
 * the other's frames, advancing, over that path.
 *
 * Runs INSIDE the server VM, for the same three reasons as
 * `selfhost-collab.spec.ts`, against a server started with
 * `ROOMLER__TURN__FORCE_RELAY=true`, the product's own switch, which makes the
 * browser gather relay candidates and nothing else. It therefore fails when
 * coturn is down, when an allocation is refused, and when the relay cannot reach
 * the SFU.
 *
 * Env: E2E_BASE_URL, E2E_API_URL, and E2E_EXPECT_TURN_RELAY=1. The last one keeps
 * this out of every lane whose server does not force the relay, where the direct
 * path would win and the assertion would be wrong rather than informative.
 */
import { test, expect, type Page } from '@playwright/test'
import {
  uniqueUser,
  registerUserViaApi,
  createTenantViaApi,
  createRoomViaApi,
  addTenantMemberViaApi,
  joinRoomViaApi,
  startCallViaApi,
  loginViaUi,
} from './fixtures/test-helpers'
import {
  trackLocalCaptures,
  recordPeerConnections,
  remoteVideoTracks,
  remoteVideoProgress,
  peerPaths,
} from './fixtures/call-media'

const API_URL = process.env.E2E_API_URL || ''
const BASE_URL = process.env.E2E_BASE_URL || ''

async function joinCall(page: Page, tenantId: string, roomId: string): Promise<void> {
  await page.goto(`${BASE_URL}/tenant/${tenantId}/room/${roomId}/call`)
  const join = page.getByRole('button', { name: /join/i }).first()
  await expect(join, 'call page never offered a Join button').toBeVisible({ timeout: 30_000 })
  await join.click()
  await expect(page.getByText('You').first(), 'own tile never rendered after Join').toBeVisible({
    timeout: 30_000,
  })
}

test.describe('#3287 a call over the self-host TURN relay', () => {
  test.skip(
    !API_URL || !BASE_URL || process.env.E2E_EXPECT_TURN_RELAY !== '1',
    'set E2E_BASE_URL, E2E_API_URL and E2E_EXPECT_TURN_RELAY=1 on a server with force_relay on',
  )
  test.setTimeout(5 * 60 * 1000)

  test('both sides decode advancing frames with the relay in the path', async ({ browser }) => {
    const owner = uniqueUser()
    const peer = uniqueUser()
    const o = await registerUserViaApi(owner)
    const p = await registerUserViaApi(peer)
    const tenant = await createTenantViaApi(o.access_token, 'TURN relay', `turn-${Date.now()}`)
    await addTenantMemberViaApi(o.access_token, tenant.id, p.user.id)
    const room = await createRoomViaApi(o.access_token, tenant.id, 'TURN relay room')
    await joinRoomViaApi(p.access_token, tenant.id, room.id)
    await startCallViaApi(o.access_token, tenant.id, room.id)

    const contexts = await Promise.all([
      browser.newContext({ permissions: ['camera', 'microphone'] }),
      browser.newContext({ permissions: ['camera', 'microphone'] }),
    ])
    for (const c of contexts) {
      await trackLocalCaptures(c)
      await recordPeerConnections(c)
    }
    const pages = await Promise.all(contexts.map((c) => c.newPage()))
    const sides = [
      ['owner', pages[0]],
      ['peer', pages[1]],
    ] as const

    try {
      await loginViaUi(pages[0], owner.username, owner.password)
      await loginViaUi(pages[1], peer.username, peer.password)
      await joinCall(pages[0], tenant.id, room.id)
      await joinCall(pages[1], tenant.id, room.id)

      for (const [tag, page] of sides) {
        await expect
          .poll(async () => await remoteVideoTracks(page), {
            timeout: 60_000,
            message: `${tag} was never given a remote video track: this is signalling, not media`,
          })
          .toBeGreaterThan(0)
      }

      // A failed relay shows here: the track exists and its counter never moves.
      for (const [tag, page] of sides) {
        await expect
          .poll(async () => (await remoteVideoProgress(page)).frames, {
            timeout: 90_000,
            message:
              `${tag} decoded no remote frame. Peer paths: ` +
              JSON.stringify(await peerPaths(page)),
          })
          .toBeGreaterThan(0)
      }

      const before = await Promise.all(pages.map(remoteVideoProgress))
      await pages[0].waitForTimeout(5_000)
      const after = await Promise.all(pages.map(remoteVideoProgress))

      for (const [i, [tag, page]] of sides.entries()) {
        const paths = await peerPaths(page)
        console.log(`[turn-relay] ${tag} frames ${before[i].frames} -> ${after[i].frames}`)
        for (const path of paths) console.log(`[turn-relay] ${tag} path ${JSON.stringify(path)}`)

        const connected = paths.filter((path) => path.state === 'connected')
        expect(connected.length, `${tag} has no connected peer connection`).toBeGreaterThan(0)
        for (const path of connected) {
          expect(path.localType, `${tag} selected a pair that is not relayed`).toBe('relay')
          expect(path.bytesReceived, `${tag} received nothing over its relay pair`).toBeGreaterThan(0)
        }
        expect(
          after[i].frames - before[i].frames,
          `${tag}: remote frames did not advance over 5 s`,
        ).toBeGreaterThanOrEqual(5)
      }
    } finally {
      for (const page of pages) await page.close()
      for (const c of contexts) await c.close()
    }
  })
})
