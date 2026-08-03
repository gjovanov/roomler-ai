import { defineStore } from 'pinia'
import { ref } from 'vue'
import { useAuthStore } from './auth'
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
  // Server-minted id of THIS WebSocket connection (from the `connected`
  // frame). Scopes call sessions per connection (call/join + call/leave
  // bodies) so the same user can be in a call from several browsers.
  // Re-minted on every redial — always read the current value at call time.
  const connectionId = ref<string | null>(null)
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

  // S6 — tenant-affinity key. Carried as `tid=` on the WS URL so the
  // front load-balancer hashes this connection onto the pod that owns
  // the active tenant (rc-hub / tunnel-hub / mediasoup are pod-local).
  // The server validates membership; other-tenant notifications still
  // arrive via the Redis fan-out, so a multi-tenant user loses nothing.
  let affinityTid: string | null = null
  let lastToken: string | null = null
  // True once a socket has opened — a later open is a RE-connect and the
  // shell must resync state that WS pushes would have delivered meanwhile.
  let hadConnection = false

  function wsUrl(token: string): string {
    const protocol = location.protocol === 'https:' ? 'wss:' : 'ws:'
    // In dev mode, connect directly to the API server to bypass Vite proxy (which doesn't relay WS frames)
    const wsHost = import.meta.env.DEV ? 'localhost:5001' : location.host
    const tidParam = affinityTid ? `&tid=${affinityTid}` : ''
    return `${protocol}//${wsHost}/ws?token=${token}${tidParam}`
  }

  /**
   * Set (or clear) the tenant-affinity key. When it actually changes
   * while a socket is live, the connection re-dials immediately so the
   * LB can re-route it. Deliberately NOT `disconnect()` — that would
   * wipe the rc/media handler registrations that outlive a reconnect;
   * this path mirrors a transient drop instead (handlers intact).
   */
  function setTenantAffinity(tid: string | null) {
    if (tid === affinityTid) return
    affinityTid = tid
    forceRedial()
  }

  /**
   * C-2 — immediate redial with the CURRENT affinity, even when the tid
   * didn't change. The rehome path needs this: after a server roll a
   * socket can sit PARKED on a pod the LB no longer maps this tenant to —
   * the affinity value is right, the placement is wrong, and only a fresh
   * dial re-hashes it. No-op when no socket is live.
   */
  function forceRedial() {
    if (socket && socket.readyState <= WebSocket.OPEN && lastToken) {
      const token = lastToken
      const old = socket
      // This close is intentional — detach the handlers so the normal
      // onclose auto-reconnect (3 s) doesn't race our immediate redial.
      old.onclose = null
      old.onmessage = null
      old.onerror = null
      socket = null
      cleanup()
      try {
        old.close()
      } catch {
        /* ignore */
      }
      connect(token)
    }
  }

  function connect(token: string) {
    if (socket && socket.readyState <= WebSocket.OPEN) return

    status.value = 'connecting'
    lastToken = token
    socket = new WebSocket(wsUrl(token))

    socket.onopen = () => {
      status.value = 'connected'
      // Anything that arrived while the socket was down is lost — let the
      // shell (AppLayout) refetch rooms/unread/current messages on re-open.
      if (hadConnection) {
        window.dispatchEvent(new CustomEvent('ws:reconnected'))
      }
      hadConnection = true
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
        if (msg.type === 'connected' && typeof msg.connection_id === 'string') {
          connectionId.value = msg.connection_id
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
      connectionId.value = null
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
        // Increment unread count if user is not viewing this room — but
        // NEVER for the user's own messages: since 2026-08-04 the server
        // broadcasts to ALL members including the sender (so their other
        // browsers get realtime), and a self-echo must not inflate badges.
        const msgData = msg.data as { room_id?: string; author_id?: string }
        const selfId = useAuthStore().user?.id
        const isOwn = !!selfId && msgData.author_id === selfId
        if (msgData.room_id && !isOwn) {
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
    connectionId.value = null
    pendingWaiters.clear()
    mediaHandlers.clear()
    rcHandlers.clear()
  }

  return {
    status,
    connectionId,
    connect,
    disconnect,
    setTenantAffinity,
    forceRedial,
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
