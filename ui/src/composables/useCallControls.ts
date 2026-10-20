// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
import { useRouter } from 'vue-router'
import { useConferenceStore } from '@/stores/conference'
import { useRoomStore } from '@/stores/rooms'

/**
 * The in-call actions a surface OUTSIDE the call page offers: back to the
 * call, and hang up. ONE copy for the desktop drawer's MiniConference and the
 * phone's app-bar menu (FR-116), so the two cannot leave a call differently.
 * Mute and camera are the conference store's own toggles.
 */
export function useCallControls() {
  const conferenceStore = useConferenceStore()
  const roomStore = useRoomStore()
  const router = useRouter()

  function returnToCall() {
    if (conferenceStore.tenantId && conferenceStore.roomId) {
      router.push({
        name: 'room-call',
        params: {
          tenantId: conferenceStore.tenantId,
          roomId: conferenceStore.roomId,
        },
      })
    }
  }

  async function hangUp() {
    if (conferenceStore.tenantId && conferenceStore.roomId) {
      await roomStore.leaveCall(conferenceStore.tenantId, conferenceStore.roomId)
    }
    conferenceStore.leaveRoom()
  }

  return { returnToCall, hangUp }
}
