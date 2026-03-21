import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest'
import { setActivePinia, createPinia } from 'pinia'

// Mock router
vi.mock('@/plugins/router', () => ({
  default: { push: vi.fn() },
}))

vi.mock('@/composables/usePush', () => ({
  subscribePush: vi.fn(() => Promise.resolve()),
  unsubscribePush: vi.fn(() => Promise.resolve()),
}))

vi.mock('@/api/client', () => ({
  api: {
    get: vi.fn(),
    post: vi.fn(),
    put: vi.fn(),
    delete: vi.fn(),
  },
}))

// Mock dependent stores used inside handleMessage
vi.mock('@/stores/notification', () => ({
  useNotificationStore: vi.fn(() => ({
    addFromWs: vi.fn(),
    setUnreadCount: vi.fn(),
  })),
}))

vi.mock('@/stores/tasks', () => ({
  useTaskStore: vi.fn(() => ({
    updateFromWs: vi.fn(),
  })),
}))

vi.mock('@/stores/conference', () => ({
  useConferenceStore: vi.fn(() => ({
    isInCall: false,
    roomId: null,
    leaveRoom: vi.fn(),
  })),
}))

// WebSocket mock
class MockWebSocket {
  static CONNECTING = 0
  static OPEN = 1
  static CLOSING = 2
  static CLOSED = 3

  readyState = MockWebSocket.CONNECTING
  onopen: (() => void) | null = null
  onmessage: ((event: { data: string }) => void) | null = null
  onclose: (() => void) | null = null
  onerror: (() => void) | null = null
  sentMessages: string[] = []

  send(data: string) {
    this.sentMessages.push(data)
  }

  close() {
    this.readyState = MockWebSocket.CLOSED
  }

  // Test helpers
  simulateOpen() {
    this.readyState = MockWebSocket.OPEN
    this.onopen?.()
  }

  simulateMessage(data: unknown) {
    this.onmessage?.({ data: JSON.stringify(data) })
  }

  simulateClose() {
    this.readyState = MockWebSocket.CLOSED
    this.onclose?.()
  }
}

let mockWsInstance: MockWebSocket

// Replace global WebSocket with our mock class.
// We use a subclass so `new WebSocket(url)` works with Reflect.construct.
const OriginalMockWebSocket = MockWebSocket
const WebSocketProxy = new Proxy(OriginalMockWebSocket, {
  construct(_target, args) {
    mockWsInstance = new OriginalMockWebSocket()
    // Store the URL for potential assertions
    ;(mockWsInstance as unknown as Record<string, unknown>).url = args[0]
    return mockWsInstance
  },
})
// Copy static constants
Object.defineProperty(WebSocketProxy, 'CONNECTING', { value: 0 })
Object.defineProperty(WebSocketProxy, 'OPEN', { value: 1 })
Object.defineProperty(WebSocketProxy, 'CLOSING', { value: 2 })
Object.defineProperty(WebSocketProxy, 'CLOSED', { value: 3 })

vi.stubGlobal('WebSocket', WebSocketProxy)

// Stub import.meta.env.DEV
vi.stubGlobal('location', { protocol: 'http:', host: 'localhost:5000' })

import { useWsStore } from '@/stores/ws'
import { useMessageStore } from '@/stores/messages'
import { useRoomStore } from '@/stores/rooms'

describe('useWsStore', () => {
  beforeEach(() => {
    setActivePinia(createPinia())
    vi.clearAllMocks()
    vi.useFakeTimers()
  })

  afterEach(() => {
    vi.useRealTimers()
  })

  describe('status transitions', () => {
    it('should start as disconnected', () => {
      const store = useWsStore()
      expect(store.status).toBe('disconnected')
    })

    it('should transition to connecting when connect is called', () => {
      const store = useWsStore()
      store.connect('test-token')
      expect(store.status).toBe('connecting')
    })

    it('should transition to connected on WebSocket open', () => {
      const store = useWsStore()
      store.connect('test-token')
      mockWsInstance.simulateOpen()
      expect(store.status).toBe('connected')
    })

    it('should transition to disconnected on WebSocket close', () => {
      const store = useWsStore()
      store.connect('test-token')
      mockWsInstance.simulateOpen()
      expect(store.status).toBe('connected')

      mockWsInstance.simulateClose()
      expect(store.status).toBe('disconnected')
    })

    it('should transition to disconnected on explicit disconnect', () => {
      const store = useWsStore()
      store.connect('test-token')
      mockWsInstance.simulateOpen()

      store.disconnect()
      expect(store.status).toBe('disconnected')
    })
  })

  describe('sendTyping', () => {
    it('should send correct message format', () => {
      const store = useWsStore()
      store.connect('tok')
      mockWsInstance.simulateOpen()

      store.sendTyping('room-123')

      expect(mockWsInstance.sentMessages).toHaveLength(1)
      const sent = JSON.parse(mockWsInstance.sentMessages[0])
      expect(sent).toEqual({
        type: 'typing:start',
        data: { room_id: 'room-123' },
      })
    })

    it('should not send when socket is not open', () => {
      const store = useWsStore()
      store.connect('tok')
      // Don't simulate open — socket is still CONNECTING

      store.sendTyping('room-123')

      expect(mockWsInstance.sentMessages).toHaveLength(0)
    })
  })

  describe('send', () => {
    it('should send JSON-serialized message', () => {
      const store = useWsStore()
      store.connect('tok')
      mockWsInstance.simulateOpen()

      store.send('custom:event', { foo: 'bar' })

      const sent = JSON.parse(mockWsInstance.sentMessages[0])
      expect(sent).toEqual({ type: 'custom:event', data: { foo: 'bar' } })
    })
  })

  describe('message routing', () => {
    it('should route message:create to messageStore.addMessageFromWs', () => {
      const store = useWsStore()
      store.connect('tok')
      mockWsInstance.simulateOpen()

      const messageStore = useMessageStore()
      const spy = vi.spyOn(messageStore, 'addMessageFromWs')

      const msgData = { id: 'm1', room_id: 'r1', content: 'hi' }
      mockWsInstance.simulateMessage({ type: 'message:create', data: msgData })

      expect(spy).toHaveBeenCalledWith(msgData)
    })

    it('should route message:update to messageStore.updateMessageFromWs', () => {
      const store = useWsStore()
      store.connect('tok')
      mockWsInstance.simulateOpen()

      const messageStore = useMessageStore()
      const spy = vi.spyOn(messageStore, 'updateMessageFromWs')

      const msgData = { id: 'm1', content: 'edited' }
      mockWsInstance.simulateMessage({ type: 'message:update', data: msgData })

      expect(spy).toHaveBeenCalledWith(msgData)
    })

    it('should route message:delete to messageStore.removeMessageFromWs', () => {
      const store = useWsStore()
      store.connect('tok')
      mockWsInstance.simulateOpen()

      const messageStore = useMessageStore()
      const spy = vi.spyOn(messageStore, 'removeMessageFromWs')

      const msgData = { id: 'm1', room_id: 'r1' }
      mockWsInstance.simulateMessage({ type: 'message:delete', data: msgData })

      expect(spy).toHaveBeenCalledWith(msgData)
    })

    it('should route message:reaction to messageStore.handleReactionFromWs', () => {
      const store = useWsStore()
      store.connect('tok')
      mockWsInstance.simulateOpen()

      const messageStore = useMessageStore()
      const spy = vi.spyOn(messageStore, 'handleReactionFromWs')

      const data = { action: 'add', message_id: 'm1', emoji: '👍', user_id: 'u1' }
      mockWsInstance.simulateMessage({ type: 'message:reaction', data })

      expect(spy).toHaveBeenCalledWith(data)
    })

    it('should increment unread for message:create in a non-current room', () => {
      const store = useWsStore()
      store.connect('tok')
      mockWsInstance.simulateOpen()

      const roomStore = useRoomStore()
      roomStore.setCurrent({ id: 'current-room' } as never)
      const spy = vi.spyOn(roomStore, 'incrementUnread')

      mockWsInstance.simulateMessage({
        type: 'message:create',
        data: { id: 'm1', room_id: 'other-room', content: 'hi' },
      })

      expect(spy).toHaveBeenCalledWith('other-room')
    })

    it('should NOT increment unread for message:create in the current room', () => {
      const store = useWsStore()
      store.connect('tok')
      mockWsInstance.simulateOpen()

      const roomStore = useRoomStore()
      roomStore.setCurrent({ id: 'current-room' } as never)
      const spy = vi.spyOn(roomStore, 'incrementUnread')

      mockWsInstance.simulateMessage({
        type: 'message:create',
        data: { id: 'm1', room_id: 'current-room', content: 'hi' },
      })

      expect(spy).not.toHaveBeenCalled()
    })

    it('should route room:call_started to roomStore.updateRoomCallStatus', () => {
      const store = useWsStore()
      store.connect('tok')
      mockWsInstance.simulateOpen()

      const roomStore = useRoomStore()
      const spy = vi.spyOn(roomStore, 'updateRoomCallStatus')

      mockWsInstance.simulateMessage({
        type: 'room:call_started',
        data: { room_id: 'r1', room_name: 'Room', started_by: 'u1' },
      })

      expect(spy).toHaveBeenCalledWith('r1', 'in_progress')
    })

    it('should route room:call_ended to roomStore.updateRoomCallStatus with null', () => {
      const store = useWsStore()
      store.connect('tok')
      mockWsInstance.simulateOpen()

      const roomStore = useRoomStore()
      const spy = vi.spyOn(roomStore, 'updateRoomCallStatus')

      mockWsInstance.simulateMessage({
        type: 'room:call_ended',
        data: { room_id: 'r1' },
      })

      expect(spy).toHaveBeenCalledWith('r1', null, 0)
    })
  })

  describe('media handlers', () => {
    it('should register and invoke persistent media handlers', () => {
      const store = useWsStore()
      store.connect('tok')
      mockWsInstance.simulateOpen()

      const handler = vi.fn()
      store.onMediaMessage('media:produce', handler)

      mockWsInstance.simulateMessage({ type: 'media:produce', data: { id: 'p1' } })

      expect(handler).toHaveBeenCalledWith({ id: 'p1' })
    })

    it('should remove media handler with offMediaMessage', () => {
      const store = useWsStore()
      store.connect('tok')
      mockWsInstance.simulateOpen()

      const handler = vi.fn()
      store.onMediaMessage('media:consume', handler)
      store.offMediaMessage('media:consume')

      mockWsInstance.simulateMessage({ type: 'media:consume', data: {} })

      expect(handler).not.toHaveBeenCalled()
    })
  })

  describe('ping interval', () => {
    it('should send ping every 30 seconds after connecting', () => {
      const store = useWsStore()
      store.connect('tok')
      mockWsInstance.simulateOpen()

      vi.advanceTimersByTime(30_000)

      expect(mockWsInstance.sentMessages).toHaveLength(1)
      const ping = JSON.parse(mockWsInstance.sentMessages[0])
      expect(ping).toEqual({ type: 'ping' })
    })
  })
})
