// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
import { defineStore } from 'pinia'
import { ref, computed } from 'vue'
import { api } from '@/api/client'
import { alreadyDeleted } from '@/utils/alreadyDeleted'
import { useWsStore } from './ws'

export interface Room {
  id: string
  tenant_id: string
  parent_id?: string
  name: string
  path: string
  emoji?: string
  topic?: { value?: string }
  purpose?: string
  /** Listed in Explore. NOT an access control — see `visibility`. */
  is_open: boolean
  /**
   * Who may READ the room, enforced server-side.
   *
   * Optional because an older API returns rooms without it; treat a missing
   * value as `public`, which is what the server defaults to.
   */
  visibility?: 'public' | 'private' | 'secret'
  is_archived: boolean
  is_read_only: boolean
  is_default: boolean
  has_media: boolean
  conference_status?: string
  meeting_code?: string
  participant_count: number
  member_count: number
  message_count: number
  created_at: string
  /**
   * FR-90 — another module owns what this room means: `{module: 'hive',
   * ref: <session id>}` is an agent session's room. Chat stores it and never
   * interprets it. FR-137 parked FR-90, so the SPA opens such a room as a
   * plain Secret room.
   */
  binding?: { module: string; ref: string }
}

export interface Participant {
  id: string
  user_id?: string
  display_name: string
  username?: string
  avatar?: string
  is_muted: boolean
  is_video_on: boolean
  is_screen_sharing: boolean
  is_hand_raised: boolean
}

export interface FileEntry {
  id: string
  filename: string
  content_type: string
  size: number
  url: string
  uploaded_by: string
  created_at: string
}

export const useRoomStore = defineStore('rooms', () => {
  const rooms = ref<Room[]>([])
  const current = ref<Room | null>(null)
  const participants = ref<Participant[]>([])
  const loading = ref(false)
  const roomFiles = ref<FileEntry[]>([])
  const filesLoading = ref(false)

  // --- Tree hierarchy ---
  const tree = computed(() => {
    const map = new Map<string | undefined, Room[]>()
    for (const r of rooms.value) {
      const parentKey = r.parent_id || undefined
      if (!map.has(parentKey)) map.set(parentKey, [])
      map.get(parentKey)!.push(r)
    }
    return map
  })

  const rootRooms = computed(() => tree.value.get(undefined) || [])

  function childrenOf(parentId: string): Room[] {
    return tree.value.get(parentId) || []
  }

  // --- Room CRUD ---
  /** #4257 — which org `rooms` describes: the org of the latest list fetch,
   *  answered or not. A fetch for another org drops the rows first, so one
   *  org's rooms never show (or fill a picker) under another org's page. */
  const roomsTenantId = ref<string | null>(null)
  // Only the latest list fetch writes: the answer for an org asked before it
  // (the page moved on to another org) is dropped.
  let listSeq = 0
  // The latest list fetch failed: `ensureRooms` asks again.
  let listFailed = false
  // The latest list fetch. One that a newer fetch superseded settles with it,
  // not with its own dropped answer: a caller that reads the rooms right
  // after its await then reads the newest ones. The welcome tour counts them
  // so (AppLayout), and an empty count could send a new user to the tutorial.
  let latestFetch: Promise<void> = Promise.resolve()

  function fetchRooms(tenantId: string): Promise<void> {
    const mySeq = ++listSeq
    if (roomsTenantId.value !== null && roomsTenantId.value !== tenantId) rooms.value = []
    roomsTenantId.value = tenantId
    listFailed = false
    loading.value = true
    const run = (async (): Promise<void> => {
      try {
        const list = await api.get<Room[]>(`/tenant/${tenantId}/room`)
        if (mySeq !== listSeq) return latestFetch
        rooms.value = list
      } catch (e) {
        if (mySeq !== listSeq) return latestFetch
        listFailed = true
        throw e
      } finally {
        if (mySeq === listSeq) loading.value = false
      }
    })()
    latestFetch = run
    return run
  }

  /** #4257 — the org's rooms for a page that only lists, counts or picks
   *  them, fetched only when the store does not hold them already: the shell
   *  loads them on every change of org. A store that holds another org's
   *  rooms, or whose last fetch failed, fetches. One that holds this org's
   *  answers with that fetch, as `ensureAgents` does: it settles only once
   *  the rooms have landed, and fails with it. It replaces a check of an
   *  empty list, which never reloaded a list holding another org's rooms. */
  function ensureRooms(tenantId: string): Promise<void> {
    if (roomsTenantId.value === tenantId && !listFailed) return latestFetch
    return fetchRooms(tenantId)
  }

  /** A room created for `tenantId` joins the list only when the list is not
   *  another org's. */
  const listIsOf = (tenantId: string) =>
    roomsTenantId.value === null || roomsTenantId.value === tenantId

  /** Server-side sidebar search (`GET /room?q=…&page=…`) — respects
   *  secret-room visibility server-side and returns a bare page of rooms.
   *  Deliberately does NOT touch `rooms`: that list must stay COMPLETE
   *  (dashboard tiles, app-bar call menu and unread accounting iterate it);
   *  callers keep search results in their own state. */
  async function searchRooms(
    tenantId: string,
    q: string,
    page = 1,
    perPage = 20,
  ): Promise<Room[]> {
    const params = new URLSearchParams({
      q,
      page: String(page),
      per_page: String(perPage),
    })
    return api.get<Room[]>(`/tenant/${tenantId}/room?${params.toString()}`)
  }

  async function createRoom(tenantId: string, payload: Partial<Room> & { has_media?: boolean }) {
    const body: Record<string, unknown> = { ...payload }
    if (body.has_media) {
      body.media_settings = {} // serde defaults: bitrate=256000, user_limit=0, quality=auto
      delete body.has_media
    }
    const room = await api.post<Room>(`/tenant/${tenantId}/room`, body)
    if (listIsOf(tenantId)) rooms.value.push(room)
    return room
  }

  async function deleteRoom(tenantId: string, roomId: string) {
    await api.delete(`/tenant/${tenantId}/room/${roomId}`)
    rooms.value = rooms.value.filter((r) => r.id !== roomId)
    if (current.value?.id === roomId) {
      current.value = null
    }
    delete unreadCounts.value[roomId]
  }

  async function joinRoom(tenantId: string, roomId: string) {
    await api.post(`/tenant/${tenantId}/room/${roomId}/join`)
  }

  async function leaveRoom(tenantId: string, roomId: string) {
    await api.post(`/tenant/${tenantId}/room/${roomId}/leave`)
  }

  async function fetchRoom(tenantId: string, roomId: string) {
    const room = await api.get<Room>(`/tenant/${tenantId}/room/${roomId}`)
    current.value = room
    return room
  }

  async function explore(tenantId: string, query: string) {
    return api.get<Room[]>(
      `/tenant/${tenantId}/room/explore?q=${encodeURIComponent(query)}`,
    )
  }

  function setCurrent(room: Room | null) {
    current.value = room
  }

  // --- Call operations ---
  async function startCall(tenantId: string, roomId: string) {
    const result = await api.post<{ started: boolean; rtp_capabilities: unknown }>(
      `/tenant/${tenantId}/room/${roomId}/call/start`,
    )
    return result
  }

  // Scope call sessions to THIS WebSocket connection so the same user can
  // join from several browsers/devices without one leave tearing down the
  // others. Read at call time — the id changes on every WS redial.
  function callSessionBody(): { connection_id: string } | undefined {
    const cid = useWsStore().connectionId
    return cid ? { connection_id: cid } : undefined
  }

  // How long a join waits for the page's socket to be given its id: a moment
  // after the page loads, longer on a weak network or a socket redialling.
  const CALL_SOCKET_WAIT_MS = 10_000

  /**
   * The id of this page's socket, which the call session is scoped to,
   * waiting for it up to `CALL_SOCKET_WAIT_MS`. A join sent without one opened
   * a session no leave from the page could close, because the leave names its
   * connection: the person stayed listed, and a call they were alone in never
   * ended (#3416). A call cannot run without the socket, so past the bound
   * this throws, and the join is refused with the reason.
   */
  async function callConnectionId(): Promise<string> {
    const cid = await useWsStore().waitForConnectionId(CALL_SOCKET_WAIT_MS)
    if (!cid) throw new Error('Not connected to the server. Check your connection and try again.')
    return cid
  }

  async function joinCall(tenantId: string, roomId: string) {
    const connection_id = await callConnectionId()
    const data = await api.post<{ participant_id: string; joined: boolean; transports?: unknown }>(
      `/tenant/${tenantId}/room/${roomId}/call/join`,
      { connection_id },
    )
    return data
  }

  async function leaveCall(tenantId: string, roomId: string) {
    await api.post(`/tenant/${tenantId}/room/${roomId}/call/leave`, callSessionBody())
    participants.value = []
  }

  async function endCall(tenantId: string, roomId: string) {
    await api.post(`/tenant/${tenantId}/room/${roomId}/call/end`)
    participants.value = []
  }

  async function fetchParticipants(tenantId: string, roomId: string) {
    const parts = await api.get<Participant[]>(
      `/tenant/${tenantId}/room/${roomId}/call/participant`,
    )
    participants.value = parts
    return parts
  }

  // --- Members ---
  async function fetchMembers(tenantId: string, roomId: string) {
    const data = await api.get<{ items: Participant[] }>(
      `/tenant/${tenantId}/room/${roomId}/member`,
    )
    return data.items
  }

  // --- Call status updates (from WS) ---
  function updateRoomCallStatus(roomId: string, conferenceStatus: string | null, participantCount?: number) {
    const room = rooms.value.find(r => r.id === roomId)
    if (room) {
      room.conference_status = conferenceStatus ?? undefined
      if (participantCount !== undefined) room.participant_count = participantCount
    }
    if (current.value?.id === roomId) {
      current.value.conference_status = conferenceStatus ?? undefined
      if (participantCount !== undefined) current.value.participant_count = participantCount
    }
  }

  // --- Room files ---
  async function fetchRoomFiles(tenantId: string, roomId: string) {
    filesLoading.value = true
    try {
      const data = await api.get<{ items: FileEntry[] }>(
        `/tenant/${tenantId}/room/${roomId}/file`,
      )
      roomFiles.value = data.items
    } finally {
      filesLoading.value = false
    }
  }

  async function uploadRoomFile(tenantId: string, roomId: string, file: File) {
    const formData = new FormData()
    formData.append('file', file)
    const result = await api.upload<FileEntry>(
      `/tenant/${tenantId}/room/${roomId}/file/upload`,
      formData,
    )
    roomFiles.value.push(result)
    return result
  }

  async function deleteRoomFile(tenantId: string, fileId: string) {
    try {
      await api.delete(`/tenant/${tenantId}/file/${fileId}`)
    } catch (e) {
      // A 404 is a file already deleted (#3327): it leaves the room's list
      // all the same, and no error is shown.
      if (!alreadyDeleted(e)) throw e
    }
    roomFiles.value = roomFiles.value.filter((f) => f.id !== fileId)
  }

  function clearRoomFiles() {
    roomFiles.value = []
  }

  // --- Unread messages ---
  const unreadCounts = ref<Record<string, number>>({})

  const totalUnread = computed(() =>
    Object.values(unreadCounts.value).reduce((sum, c) => sum + c, 0),
  )

  async function fetchUnreadCount(tenantId: string, roomId: string) {
    try {
      const data = await api.get<{ count: number }>(
        `/tenant/${tenantId}/room/${roomId}/message/unread-count`,
      )
      unreadCounts.value[roomId] = data.count
    } catch {
      // non-critical
    }
  }

  async function fetchAllUnreadCounts(tenantId: string) {
    // #4257 — over this org's rooms only: a fetch that a newer one superseded
    // settles with it, and the shell's count after it would ask for the new
    // org's rooms under the previous org.
    if (!listIsOf(tenantId)) return
    for (const room of rooms.value) {
      fetchUnreadCount(tenantId, room.id)
    }
  }

  async function markMessagesRead(tenantId: string, roomId: string, messageIds: string[]) {
    if (messageIds.length === 0) return
    try {
      await api.post(`/tenant/${tenantId}/room/${roomId}/message/read`, {
        message_ids: messageIds,
      })
      // Do not optimistically decrement — caller should fetchUnreadCount for accuracy
    } catch {
      // non-critical
    }
  }

  function incrementUnread(roomId: string) {
    unreadCounts.value[roomId] = (unreadCounts.value[roomId] || 0) + 1
  }

  /// Mark EVERY unread message in the room read (server-side, same filter as
  /// the unread count) and zero the badge. Called when the room is opened —
  /// Slack-style auto-clear; also heals historically stuck counts.
  async function markAllRead(tenantId: string, roomId: string) {
    try {
      await api.post(`/tenant/${tenantId}/room/${roomId}/message/read-all`)
      unreadCounts.value[roomId] = 0
    } catch {
      // non-critical
    }
  }

  return {
    // State
    rooms,
    roomsTenantId,
    current,
    participants,
    loading,
    rootRooms,
    tree,
    roomFiles,
    filesLoading,
    // Room operations
    childrenOf,
    fetchRooms,
    ensureRooms,
    searchRooms,
    createRoom,
    deleteRoom,
    joinRoom,
    leaveRoom,
    fetchRoom,
    explore,
    setCurrent,
    fetchMembers,
    // Call operations
    updateRoomCallStatus,
    startCall,
    callConnectionId,
    joinCall,
    leaveCall,
    endCall,
    fetchParticipants,
    // Room files
    fetchRoomFiles,
    uploadRoomFile,
    deleteRoomFile,
    clearRoomFiles,
    // Unread
    unreadCounts,
    totalUnread,
    fetchUnreadCount,
    fetchAllUnreadCounts,
    markMessagesRead,
    markAllRead,
    incrementUnread,
  }
})
