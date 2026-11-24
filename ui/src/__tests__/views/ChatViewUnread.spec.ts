// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// #4395 — a message for a room the person is not looking at raises that
// room's unread count, also when that room is the last one they opened in a
// chat and they have since gone to another page of the same org. The rooms
// store keeps naming that room as its current room after the chat page is
// left (the Files page defaults to it), so an unread check that asks the
// current room counted nothing there.
//
// These run the REAL chat page and the REAL socket store's `message:create`
// handling, with the network stubbed at the API client and the socket.
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest'
import { flushPromises, mount, type VueWrapper } from '@vue/test-utils'
import { defineComponent, h } from 'vue'
import { createPinia, setActivePinia } from 'pinia'
import { createVuetify } from 'vuetify'
import * as components from 'vuetify/components'
import * as directives from 'vuetify/directives'
import { createMemoryHistory, createRouter, RouterView, type Router } from 'vue-router'

const hoisted = vi.hoisted(() => ({
  /** A child the page renders, stubbed: these specs are about the page. */
  async stub(name: string) {
    const { defineComponent, h } = await import('vue')
    return { default: defineComponent({ name, render: () => h('div', { 'data-stub': name }) }) }
  },
}))

vi.mock('@/api/client', () => ({
  api: {
    get: vi.fn(() => Promise.resolve([])),
    post: vi.fn(() => Promise.resolve({})),
    put: vi.fn(() => Promise.resolve({})),
    delete: vi.fn(() => Promise.resolve({})),
  },
}))
vi.mock('@/composables/usePush', () => ({
  subscribePush: vi.fn(() => Promise.resolve()),
  unsubscribePush: vi.fn(() => Promise.resolve()),
}))
// The socket store's other handlers, as the ws store's own spec stubs them.
vi.mock('@/stores/notification', () => ({
  useNotificationStore: vi.fn(() => ({ addFromWs: vi.fn(), setUnreadCount: vi.fn() })),
}))
vi.mock('@/stores/tasks', () => ({
  useTaskStore: vi.fn(() => ({ updateFromWs: vi.fn() })),
}))
vi.mock('@/stores/conference', () => ({
  useConferenceStore: vi.fn(() => ({ isInCall: false, roomId: null, leaveRoom: vi.fn() })),
}))
vi.mock('@/components/chat/MessageBubble.vue', () => hoisted.stub('MessageBubble'))
vi.mock('@/components/chat/MessageEditor.vue', () => hoisted.stub('MessageEditor'))
vi.mock('@/components/chat/MemberPanel.vue', () => hoisted.stub('MemberPanel'))
vi.mock('@/components/chat/FilePanel.vue', () => hoisted.stub('FilePanel'))
vi.mock('@/components/chat/EmojiPicker.vue', () => hoisted.stub('EmojiPicker'))
vi.mock('@/components/chat/GiphyPicker.vue', () => hoisted.stub('GiphyPicker'))

import i18n from '@/plugins/i18n'
import ChatView from '@/views/chat/ChatView.vue'
import { useRoomStore } from '@/stores/rooms'
import { useMessageStore } from '@/stores/messages'
import { useAuthStore } from '@/stores/auth'
import { useTenantStore } from '@/stores/tenant'
import { useWsStore } from '@/stores/ws'

// The socket, as the ws store's spec mocks it: `new WebSocket(url)` hands
// back one instance a test can open and deliver frames on.
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
  send() {}
  close() {
    this.readyState = MockWebSocket.CLOSED
  }
  simulateOpen() {
    this.readyState = MockWebSocket.OPEN
    this.onopen?.()
  }
  simulateMessage(data: unknown) {
    this.onmessage?.({ data: JSON.stringify(data) })
  }
}
let socket: MockWebSocket
const WebSocketProxy = new Proxy(MockWebSocket, {
  construct() {
    socket = new MockWebSocket()
    return socket
  },
})
for (const [k, v] of Object.entries({ CONNECTING: 0, OPEN: 1, CLOSING: 2, CLOSED: 3 })) {
  Object.defineProperty(WebSocketProxy, k, { value: v })
}

const vuetify = createVuetify({ components, directives })

beforeAll(() => {
  const g = globalThis as unknown as Record<string, unknown>
  vi.stubGlobal('WebSocket', WebSocketProxy)
  // jsdom has no IntersectionObserver (the page's read receipts) and no
  // ResizeObserver (Vuetify).
  for (const name of ['IntersectionObserver', 'ResizeObserver']) {
    if (!(name in g)) {
      g[name] = class {
        observe() {}
        unobserve() {}
        disconnect() {}
      }
    }
  }
})

const ROOMS = {
  R: { id: 'R', name: 'Room R' },
  S: { id: 'S', name: 'Room S' },
} as const

let router: Router
let wrapper: VueWrapper | null = null
let rooms: ReturnType<typeof useRoomStore>

beforeEach(() => {
  setActivePinia(createPinia())
  rooms = useRoomStore()
  const messages = useMessageStore()
  useAuthStore().user = { id: 'me', display_name: 'Me' } as never
  useTenantStore().current = { id: 'T', name: 'Org' } as never

  rooms.rooms = [ROOMS.R, ROOMS.S] as never
  // What the page's fetch leaves in the store: the room and its org.
  vi.spyOn(rooms, 'fetchRoom').mockImplementation(async (t: string, id: string) => {
    rooms.setCurrent(ROOMS[id as keyof typeof ROOMS] as never, t)
    return rooms.current as never
  })
  vi.spyOn(rooms, 'fetchMembers').mockResolvedValue([] as never)
  vi.spyOn(rooms, 'markAllRead').mockResolvedValue(undefined as never)
  vi.spyOn(messages, 'fetchMessages').mockResolvedValue(undefined as never)

  router = createRouter({
    history: createMemoryHistory(),
    routes: [
      { path: '/tenant/:tenantId/room/:roomId', name: 'room-chat', component: ChatView },
      { path: '/tenant/:tenantId/files', name: 'files', component: { render: () => h('div', 'files') } },
    ],
  })

  const ws = useWsStore()
  ws.connect()
  socket.simulateOpen()
})

afterEach(() => {
  wrapper?.unmount()
  wrapper = null
  useWsStore().disconnect?.()
  document.body.innerHTML = ''
  vi.restoreAllMocks()
})

/** The chat page of `room`, opened from the sidebar. */
async function openChat(room: keyof typeof ROOMS) {
  await router.push(`/tenant/T/room/${room}`)
  await router.isReady()
  wrapper = mount(defineComponent({ render: () => h(RouterView) }), {
    global: { plugins: [vuetify, i18n, router] },
    attachTo: document.body,
  })
  await flushPromises()
}

/** Another member writes in `room` of the org the page is in. */
function messageFromAnotherMember(room: string, id = 'm1') {
  socket.simulateMessage({
    type: 'message:create',
    data: { id, room_id: room, tenant_id: 'T', author_id: 'other', content: 'hi' },
  })
}

describe('#4395: unread counts follow the room a page shows', () => {
  it('a message for the last opened room counts once the chat page is left', async () => {
    await openChat('R')
    await router.push('/tenant/T/files')
    await flushPromises()

    messageFromAnotherMember('R')

    expect(rooms.unreadCounts.R).toBe(1)
    // The current room keeps naming the last opened room (the Files page's
    // default room, #4332): only the unread check stopped asking it.
    expect(rooms.current?.id).toBe('R')
    expect(rooms.currentTenantId).toBe('T')
  })

  it('a message for the room the chat page shows is not counted', async () => {
    await openChat('R')

    messageFromAnotherMember('R')

    expect(rooms.unreadCounts.R ?? 0).toBe(0)
  })

  it('a room switch in place: the room left counts, the room shown does not', async () => {
    await openChat('R')
    await router.push('/tenant/T/room/S')
    await flushPromises()

    messageFromAnotherMember('R', 'm1')
    messageFromAnotherMember('S', 'm2')

    expect(rooms.unreadCounts.R).toBe(1)
    expect(rooms.unreadCounts.S ?? 0).toBe(0)
  })
})
