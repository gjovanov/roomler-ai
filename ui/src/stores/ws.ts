import { defineStore } from 'pinia'
import { ref } from 'vue'
import { useRoomStore } from './rooms'
import { useMessageStore } from './messages'
import { useNotificationStore } from './notification'
import { useTaskStore } from './tasks'
import { useConferenceStore } from './conference'

type WsStatus = 'disconnected' | 'connecting' | 'connected'

// eslint-disable-next-line @typescript-eslint/no-explicit-any
type MediaMessageHandler = (data: any) => void

export const useWsStore = defineStore('ws', () => {
  const status = ref<WsStatus>('disconnected')
  let socket: WebSocket | null = null
  let pingInterval: ReturnType<typeof setInterval> | null = null
  let reconnectTimeout: ReturnType<typeof setTimeout> | null = null

  // Pending one-shot message waiters (resolve on first matching message)
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  const pendingWaiters = new Map<string, { resolve: (data: any) => void; reject: (err: Error) => void }>()

  // Persistent media message handlers
  const mediaHandlers = new Map<string, MediaMessageHandler>()

  // Remote-control message handlers — rc:* messages use a `t` discriminator
  // instead of the `{type, data}` envelope, so they get their own channel.
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  type RcHandler = (msg: any) => void
  const rcHandlers = new Map<string, RcHandler>()

  function connect(token: string) {
    if (socket && socket.readyState <= WebSocket.OPEN) return

    status.value = 'connecting'
    const protocol = location.protocol === 'https:' ? 'wss:' : 'ws:'
    // In dev mode, connect directly to the API server to bypass Vite proxy (which doesn't relay WS frames)
    const wsHost = import.meta.env.DEV ? 'localhost:5001' : location.host
    socket = new WebSocket(`${protocol}//${wsHost}/ws?token=${token}`)

    socket.onopen = () => {
      status.value = 'connected'
      pingInterval = setInterval(() => {
        if (socket?.readyState === WebSocket.OPEN) {
          socket.send(JSON.stringify({ type: 'ping' }))
        }
      }, 30_000)
    }

    socket.onmessage = (event) => {
      try {
        const msg = JSON.parse(event.data)
        if (msg.type?.startsWith('media:') || msg.type === 'connected') {
          console.log('[WS] received:', msg.type)
        }
        // rc:* messages use the flat `{t, ...}` shape from the signalling
        // protocol and don't go through the main dispatch.
        if (typeof msg.t === 'string' && msg.t.startsWith('rc:')) {
          const h = rcHandlers.get(msg.t)
          if (h) h(msg)
          return
        }
        handleMessage(msg)
      } catch {
        // ignore malformed messages
      }
    }

    socket.onclose = () => {
      cleanup()
      status.value = 'disconnected'
      reconnectTimeout = setTimeout(() => connect(token), 3000)
    }

    socket.onerror = () => {
      socket?.close()
    }
  }

  function handleMessage(msg: { type: string; data?: unknown }) {
    const roomStore = useRoomStore()
    const messageStore = useMessageStore()
    const notificationStore = useNotificationStore()
    const taskStore = useTaskStore()

    // Check pending waiters first
    const waiter = pendingWaiters.get(msg.type)
    if (waiter) {
      pendingWaiters.delete(msg.type)
      waiter.resolve(msg.data)
      return
    }

    // Check persistent media handlers
    const handler = mediaHandlers.get(msg.type)
    if (handler) {
      handler(msg.data)
      return
    }

    switch (msg.type) {
      case 'message:create': {
        messageStore.addMessageFromWs(msg.data as never)
        // Increment unread count if user is not viewing this room
        const msgData = msg.data as { room_id?: string }
        if (msgData.room_id) {
          const roomStore = useRoomStore()
          if (roomStore.current?.id !== msgData.room_id) {
            roomStore.incrementUnread(msgData.room_id)
          }
        }
        break
      }
      case 'message:update':
        messageStore.updateMessageFromWs(msg.data as never)
        break
      case 'message:delete':
        messageStore.removeMessageFromWs(msg.data as never)
        break
      case 'message:reaction':
        messageStore.handleReactionFromWs(msg.data as never)
        break
      case 'call:message:create':
        // Legacy call chat event — route to message store for backwards compat
        messageStore.addMessageFromWs(msg.data as never)
        break
      case 'task:update':
        taskStore.updateFromWs(msg.data as never)
        break
      case 'notification:new':
        notificationStore.addFromWs(msg.data as never)
        break
      case 'notification:unread_count': {
        const ncData = msg.data as { count: number }
        notificationStore.setUnreadCount(ncData.count)
        break
      }
      case 'room:call_started': {
        const csData = msg.data as { room_id: string; room_name: string; started_by: string }
        roomStore.updateRoomCallStatus(csData.room_id, 'in_progress')
        window.dispatchEvent(new CustomEvent('room:call_started', { detail: csData }))
        break
      }
      case 'room:call_ended': {
        const ceData = msg.data as { room_id: string }
        roomStore.updateRoomCallStatus(ceData.room_id, null, 0)
        // Auto-teardown conference if the ended call matches the active one
        const confStore = useConferenceStore()
        if (confStore.isInCall && confStore.roomId === ceData.room_id) {
          confStore.leaveRoom()
        }
        break
      }
      case 'room:call_updated': {
        const cuData = msg.data as { room_id: string; participant_count: number; conference_status: string }
        roomStore.updateRoomCallStatus(cuData.room_id, cuData.conference_status, cuData.participant_count)
        break
      }
    }
  }

  function send(type: string, data: unknown) {
    if (socket?.readyState === WebSocket.OPEN) {
      const payload = JSON.stringify({ type, data })
      if (type.startsWith('media:')) {
        console.log('[WS] sending:', type)
      }
      socket.send(payload)
    } else {
      console.warn('[WS] cannot send, socket not open:', type, 'readyState:', socket?.readyState)
    }
  }

  function sendTyping(roomId: string) {
    send('typing:start', { room_id: roomId })
  }

  /** Wait for a specific message type (one-shot). Returns the data payload. */
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  function waitForMessage(type: string, timeoutMs = 10000): Promise<any> {
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        pendingWaiters.delete(type)
        reject(new Error(`Timeout waiting for ${type}`))
      }, timeoutMs)

      pendingWaiters.set(type, {
        resolve: (data) => {
          clearTimeout(timer)
          resolve(data)
        },
        reject: (err) => {
          clearTimeout(timer)
          reject(err)
        },
      })
    })
  }

  /** Register a persistent handler for a media message type. */
  function onMediaMessage(type: string, handler: MediaMessageHandler) {
    mediaHandlers.set(type, handler)
  }

  /** Remove a persistent handler for a media message type. */
  function offMediaMessage(type: string) {
    mediaHandlers.delete(type)
  }

  /** Send a raw object as-is (no `{type, data}` envelope). Used by the
   *  remote-control signalling protocol which has its own `{t, ...}` shape. */
  function sendRaw(msg: unknown) {
    if (socket?.readyState === WebSocket.OPEN) {
      socket.send(JSON.stringify(msg))
    } else {
      console.warn('[WS] cannot sendRaw, socket not open')
    }
  }

  /** Register a handler for an rc:* message type (e.g. `rc:sdp.answer`). */
  function onRcMessage(t: string, handler: RcHandler) {
    rcHandlers.set(t, handler)
  }
  function offRcMessage(t: string) {
    rcHandlers.delete(t)
  }

  function cleanup() {
    if (pingInterval) {
      clearInterval(pingInterval)
      pingInterval = null
    }
    if (reconnectTimeout) {
      clearTimeout(reconnectTimeout)
      reconnectTimeout = null
    }
  }

  function disconnect() {
    cleanup()
    socket?.close()
    socket = null
    status.value = 'disconnected'
    pendingWaiters.clear()
    mediaHandlers.clear()
    rcHandlers.clear()
  }

  return {
    status,
    connect,
    disconnect,
    send,
    sendRaw,
    sendTyping,
    waitForMessage,
    onMediaMessage,
    offMediaMessage,
    onRcMessage,
    offRcMessage,
  }
})
