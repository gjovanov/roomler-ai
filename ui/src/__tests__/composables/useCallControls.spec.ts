// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// FR-116 P2 — the in-call actions offered OUTSIDE the call page, shared by
// the desktop drawer's MiniConference and the phone's app-bar menu: back to
// the call, and hang up (the server's leave, then the media teardown).
import { beforeEach, describe, expect, it, vi } from 'vitest'

const hoisted = vi.hoisted(() => ({
  push: vi.fn(),
  leaveCall: vi.fn(async () => {}),
  leaveRoom: vi.fn(),
  conference: { tenantId: 't1' as string | null, roomId: 'r1' as string | null },
}))

vi.mock('vue-router', () => ({ useRouter: () => ({ push: hoisted.push }) }))
vi.mock('@/stores/rooms', () => ({ useRoomStore: () => ({ leaveCall: hoisted.leaveCall }) }))
vi.mock('@/stores/conference', () => ({
  useConferenceStore: () => ({
    get tenantId() {
      return hoisted.conference.tenantId
    },
    get roomId() {
      return hoisted.conference.roomId
    },
    leaveRoom: hoisted.leaveRoom,
  }),
}))

import { useCallControls } from '@/composables/useCallControls'

describe('useCallControls', () => {
  beforeEach(() => {
    vi.clearAllMocks()
    hoisted.conference.tenantId = 't1'
    hoisted.conference.roomId = 'r1'
  })

  it('returns to the call page of the call in progress', () => {
    useCallControls().returnToCall()
    expect(hoisted.push).toHaveBeenCalledWith({ name: 'room-call', params: { tenantId: 't1', roomId: 'r1' } })
  })

  it('hangs up on the server first, then tears the media down', async () => {
    const order: string[] = []
    hoisted.leaveCall.mockImplementationOnce(async () => {
      order.push('leaveCall')
    })
    hoisted.leaveRoom.mockImplementationOnce(() => {
      order.push('leaveRoom')
    })
    await useCallControls().hangUp()
    expect(hoisted.leaveCall).toHaveBeenCalledWith('t1', 'r1')
    expect(order).toEqual(['leaveCall', 'leaveRoom'])
  })

  it('without a call, goes nowhere and still releases any media', async () => {
    hoisted.conference.tenantId = null
    hoisted.conference.roomId = null
    const c = useCallControls()
    c.returnToCall()
    await c.hangUp()
    expect(hoisted.push).not.toHaveBeenCalled()
    expect(hoisted.leaveCall).not.toHaveBeenCalled()
    expect(hoisted.leaveRoom).toHaveBeenCalledOnce()
  })
})
