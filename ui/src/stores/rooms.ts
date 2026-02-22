import { defineStore } from 'pinia'
import { ref, computed } from 'vue'
import { api } from '@/api/client'

export interface Room {
  id: string
  tenant_id: string
  parent_id?: string
  name: string
  path: string
  emoji?: string
  topic?: { value?: string }
  purpose?: string
  is_open: boolean
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
}

export interface Participant {
  id: string
  user_id?: string
  display_name: string
  is_muted: boolean
  is_video_on: boolean
  is_screen_sharing: boolean
  is_hand_raised: boolean
}

export interface TranscriptSegment {
  id: string
  segment_id: string
  user_id: string
  speaker_name: string
  text: string
  language?: string
  confidence?: number
  start_time: number
  end_time: number
  inference_duration_ms?: number
  is_final: boolean
  received_at: number
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

export interface ChatMsg {
  id: string
  room_id: string
  author_id: string
  display_name: string
  content: string
  created_at: string
}

export const useRoomStore = defineStore('rooms', () => {
  const rooms = ref<Room[]>([])
  const current = ref<Room | null>(null)
  const participants = ref<Participant[]>([])
  const loading = ref(false)
  const chatMessages = ref<ChatMsg[]>([])
  const transcriptSegments = ref<TranscriptSegment[]>([])
  const transcriptionEnabled = ref(false)
  const selectedTranscriptionModel = ref<'whisper' | 'canary'>('whisper')
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
  async function fetchRooms(tenantId: string) {
    loading.value = true
    try {
      rooms.value = await api.get<Room[]>(`/tenant/${tenantId}/room`)
    } finally {
      loading.value = false
    }
  }

  async function createRoom(tenantId: string, payload: Partial<Room> & { has_media?: boolean }) {
    const body: Record<string, unknown> = { ...payload }
    if (body.has_media) {
      body.media_settings = {} // serde defaults: bitrate=256000, user_limit=0, quality=auto
      delete body.has_media
    }
    const room = await api.post<Room>(`/tenant/${tenantId}/room`, body)
    rooms.value.push(room)
    return room
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

  async function joinCall(tenantId: string, roomId: string) {
    const data = await api.post<{ participant_id: string; joined: boolean; transports?: unknown }>(
      `/tenant/${tenantId}/room/${roomId}/call/join`,
    )
    return data
  }

  async function leaveCall(tenantId: string, roomId: string) {
    await api.post(`/tenant/${tenantId}/room/${roomId}/call/leave`)
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
    return api.get<Participant[]>(`/tenant/${tenantId}/room/${roomId}/member`)
  }

  // --- In-call chat ---
  async function fetchChatMessages(tenantId: string, roomId: string) {
    const data = await api.get<{ items: ChatMsg[] }>(
      `/tenant/${tenantId}/room/${roomId}/call/message`,
    )
    chatMessages.value = data.items
  }

  async function sendChatMessage(tenantId: string, roomId: string, content: string) {
    const msg = await api.post<ChatMsg>(
      `/tenant/${tenantId}/room/${roomId}/call/message`,
      { content },
    )
    chatMessages.value.push(msg)
    return msg
  }

  function addChatMessageFromWs(msg: ChatMsg) {
    if (!chatMessages.value.some((m) => m.id === msg.id)) {
      chatMessages.value.push(msg)
    }
  }

  function clearChatMessages() {
    chatMessages.value = []
  }

  // --- Transcript ---
  function addTranscriptFromWs(data: {
    user_id: string
    speaker_name: string
    text: string
    language?: string
    confidence?: number
    start_time: number
    end_time: number
    inference_duration_ms?: number
    is_final: boolean
    segment_id: string
  }) {
    const idx = transcriptSegments.value.findIndex((s) => s.segment_id === data.segment_id)
    if (idx >= 0) {
      transcriptSegments.value[idx].text = data.text
      transcriptSegments.value[idx].end_time = data.end_time
      transcriptSegments.value[idx].confidence = data.confidence
      transcriptSegments.value[idx].is_final = data.is_final
      transcriptSegments.value[idx].received_at = Date.now()
    } else {
      transcriptSegments.value.push({
        id: data.segment_id,
        segment_id: data.segment_id,
        user_id: data.user_id,
        speaker_name: data.speaker_name,
        text: data.text,
        language: data.language,
        confidence: data.confidence,
        start_time: data.start_time,
        end_time: data.end_time,
        inference_duration_ms: data.inference_duration_ms,
        is_final: data.is_final,
        received_at: Date.now(),
      })
    }
  }

  function setTranscriptionEnabled(enabled: boolean) {
    transcriptionEnabled.value = enabled
  }

  function setSelectedTranscriptionModel(model: 'whisper' | 'canary') {
    selectedTranscriptionModel.value = model
  }

  function clearTranscript() {
    transcriptSegments.value = []
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
    await api.delete(`/tenant/${tenantId}/file/${fileId}`)
    roomFiles.value = roomFiles.value.filter((f) => f.id !== fileId)
  }

  function clearRoomFiles() {
    roomFiles.value = []
  }

  return {
    // State
    rooms,
    current,
    participants,
    loading,
    rootRooms,
    tree,
    transcriptSegments,
    transcriptionEnabled,
    selectedTranscriptionModel,
    roomFiles,
    filesLoading,
    // Room operations
    childrenOf,
    fetchRooms,
    createRoom,
    joinRoom,
    leaveRoom,
    fetchRoom,
    explore,
    setCurrent,
    fetchMembers,
    // Call operations
    updateRoomCallStatus,
    startCall,
    joinCall,
    leaveCall,
    endCall,
    fetchParticipants,
    // In-call chat
    chatMessages,
    fetchChatMessages,
    sendChatMessage,
    addChatMessageFromWs,
    clearChatMessages,
    // Transcript
    addTranscriptFromWs,
    setTranscriptionEnabled,
    setSelectedTranscriptionModel,
    clearTranscript,
    // Room files
    fetchRoomFiles,
    uploadRoomFile,
    deleteRoomFile,
    clearRoomFiles,
  }
})
