// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// #1637 — the call page follows the room in its URL. The app bar's Active
// Calls menu, and the "Call started in …" snackbar, push another room's call
// onto the call page IN PLACE: `router.push({ name: 'room-call', … })` with
// only `roomId` changed. Vue Router reuses the mounted view for that, and the
// view ran its room once, in `onMounted`. So the page in room A's call showed
// A's call, chat and name under room B's URL, a message typed there went to
// B, and Leave left B, a call it was never in, while the server kept it in A.
//
// Now the route remounts the page by `roomId` (`meta.remountOn`, as #1631 did
// for the remote viewer), and a page that is in its room's call asks first:
// Switch leaves that call, then the new page joins the other; Cancel keeps the
// call and the page where they are.
//
// These run the REAL view against the real route record's meta, with the
// network and the media stubbed at the stores.
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest'
import { flushPromises, mount, type VueWrapper } from '@vue/test-utils'
import { defineComponent, h } from 'vue'
import { createPinia, setActivePinia } from 'pinia'
import { createVuetify } from 'vuetify'
import * as components from 'vuetify/components'
import * as directives from 'vuetify/directives'
import { createMemoryHistory, createRouter, type RouteRecordRaw, type Router } from 'vue-router'

const hoisted = vi.hoisted(() => ({
  ws: {
    send: () => {},
    onMediaMessage: () => {},
    offMediaMessage: () => {},
    waitForMessage: () => Promise.resolve(),
    // The page's socket already has its id, so a join is not held (#3416).
    waitForConnectionId: () => Promise.resolve('conn-1'),
  },
  /** A child the page renders, stubbed: these specs are about the page. */
  async stub(name: string) {
    const { defineComponent, h } = await import('vue')
    return { default: defineComponent({ name, render: () => h('div', { 'data-stub': name }) }) }
  },
}))

vi.mock('@/stores/ws', () => ({ useWsStore: () => hoisted.ws }))
vi.mock('@/composables/usePictureInPicture', () => ({
  usePictureInPicture: () => ({
    isSupported: { value: false },
    isPiPActive: { value: false },
    requestPiP: async () => null,
    exitPiP: async () => {},
  }),
}))
vi.mock('@/composables/useAudioPlayback', () => ({
  useAudioPlayback: () => ({ handlePlaybackMessage: () => {}, stopCurrentPlayback: () => {} }),
}))
vi.mock('@/composables/useBlackPictureTiles', () => ({
  useBlackPictureTiles: () => ({ isBlack: () => false, report: () => {} }),
}))
vi.mock('@/components/conference/layouts/TiledLayout.vue', () => hoisted.stub('TiledLayout'))
vi.mock('@/components/conference/layouts/SpotlightLayout.vue', () => hoisted.stub('SpotlightLayout'))
vi.mock('@/components/conference/layouts/SidebarLayout.vue', () => hoisted.stub('SidebarLayout'))
vi.mock('@/components/conference/LayoutSwitcher.vue', () => hoisted.stub('LayoutSwitcher'))
vi.mock('@/components/conference/ConferenceFilesPanel.vue', () => hoisted.stub('ConferenceFilesPanel'))
vi.mock('@/components/chat/MessageBubble.vue', () => hoisted.stub('MessageBubble'))
vi.mock('@/components/chat/MessageEditor.vue', () => hoisted.stub('MessageEditor'))

import appRouter from '@/plugins/router'
import i18n from '@/plugins/i18n'
import KeyedRouterView from '@/components/layout/KeyedRouterView.vue'
import { CALL_SWITCH_HANDOFF_MS, handOffJoin, takeJoin } from '@/composables/callSwitch'
import ConferenceView from '@/views/conference/ConferenceView.vue'
import { useConferenceStore } from '@/stores/conference'
import { useRoomStore } from '@/stores/rooms'
import { useMessageStore } from '@/stores/messages'
import { useAuthStore } from '@/stores/auth'

const vuetify = createVuetify({ components, directives })

beforeAll(() => {
  // Vuetify overlays measure themselves against the viewport; jsdom has
  // neither a ResizeObserver nor a visualViewport.
  const g = globalThis as unknown as Record<string, unknown>
  if (!('ResizeObserver' in g)) {
    g.ResizeObserver = class {
      observe() {}
      unobserve() {}
      disconnect() {}
    }
  }
  if (!('visualViewport' in g)) {
    g.visualViewport = {
      width: 1024,
      height: 768,
      offsetLeft: 0,
      offsetTop: 0,
      pageLeft: 0,
      pageTop: 0,
      scale: 1,
      addEventListener() {},
      removeEventListener() {},
    }
  }
})

/** The real `room-call` record's meta: a record that loses `remountOn` fails here. */
function realCallMeta(): RouteRecordRaw['meta'] {
  let found: RouteRecordRaw | undefined
  const walk = (rs: readonly RouteRecordRaw[]) => {
    for (const r of rs) {
      if (r.name === 'room-call') found = r
      if (r.children) walk(r.children)
    }
  }
  walk(appRouter.options.routes)
  expect(found, 'the app has a room-call route').toBeTruthy()
  return found!.meta
}

const ROOMS = {
  A: { id: 'A', name: 'Room A', conference_status: 'in_progress', participant_count: 2 },
  B: { id: 'B', name: 'Room B', conference_status: 'in_progress', participant_count: 3 },
} as const

let router: Router
let wrapper: VueWrapper | null = null
let conference: ReturnType<typeof useConferenceStore>
let rooms: ReturnType<typeof useRoomStore>
let messages: ReturnType<typeof useMessageStore>

beforeEach(() => {
  setActivePinia(createPinia())
  conference = useConferenceStore()
  rooms = useRoomStore()
  messages = useMessageStore()
  useAuthStore().user = { id: 'u1', display_name: 'Me' } as never

  rooms.rooms = [ROOMS.A, ROOMS.B] as never
  vi.spyOn(rooms, 'fetchRoom').mockImplementation(async (_t: string, id: string) => {
    rooms.current = ROOMS[id as keyof typeof ROOMS] as never
    return rooms.current as never
  })
  vi.spyOn(rooms, 'startCall').mockResolvedValue(undefined as never)
  vi.spyOn(rooms, 'joinCall').mockResolvedValue(undefined as never)
  vi.spyOn(rooms, 'leaveCall').mockResolvedValue(undefined as never)
  vi.spyOn(rooms, 'fetchParticipants').mockResolvedValue(undefined as never)
  vi.spyOn(rooms, 'fetchMembers').mockResolvedValue([] as never)
  vi.spyOn(rooms, 'markAllRead').mockResolvedValue(undefined as never)
  vi.spyOn(rooms, 'clearRoomFiles').mockImplementation(() => {})
  vi.spyOn(messages, 'fetchMessages').mockResolvedValue(undefined as never)

  vi.spyOn(conference, 'enumerateDevices').mockResolvedValue(undefined as never)
  vi.spyOn(conference, 'startActiveSpeaker').mockImplementation(() => {})
  vi.spyOn(conference, 'stopActiveSpeaker').mockImplementation(() => {})
  vi.spyOn(conference, 'produceLocalMedia').mockResolvedValue(undefined as never)
  vi.spyOn(conference, 'joinRoom').mockImplementation(async (t: string, r: string, n: string) => {
    conference.isInCall = true
    conference.tenantId = t
    conference.roomId = r
    conference.roomName = n
  })
  vi.spyOn(conference, 'leaveRoom').mockImplementation(() => {
    conference.isInCall = false
    conference.tenantId = null
    conference.roomId = null
    conference.roomName = null
  })

  router = createRouter({
    history: createMemoryHistory(),
    routes: [
      { path: '/tenant/:tenantId/room/:roomId/call', name: 'room-call', component: ConferenceView, meta: realCallMeta() },
      { path: '/tenant/:tenantId', name: 'tenant-home', component: { render: () => h('div', 'home') } },
    ],
  })
})

afterEach(() => {
  wrapper?.unmount()
  wrapper = null
  document.body.innerHTML = ''
  vi.restoreAllMocks()
})

/** The call page of `room`, as a person in that room's call returns to it. */
async function openCallPage(room: 'A' | 'B', inThatCall: boolean) {
  if (inThatCall) {
    conference.isInCall = true
    conference.tenantId = 'T'
    conference.roomId = room
    conference.roomName = ROOMS[room].name
  }
  await router.push(`/tenant/T/room/${room}/call`)
  await router.isReady()
  wrapper = mount(defineComponent({ render: () => h(KeyedRouterView) }), {
    global: { plugins: [vuetify, i18n, router] },
    attachTo: document.body,
  })
  await flushPromises()
}

/** What the Active Calls menu does: room B's call, pushed onto the page in place. */
function switchInPlace(room: 'A' | 'B') {
  return router.push({ name: 'room-call', params: { tenantId: 'T', roomId: room } })
}

const byTestId = (id: string) => document.body.querySelector<HTMLElement>(`[data-testid="${id}"]`)
/** The dialog's button once it renders (an overlay mounts on a later tick), or null: on a page that never asks, nothing waits on it. */
async function button(id: string, tries = 30): Promise<HTMLElement | null> {
  for (let i = 0; i < tries; i++) {
    const el = byTestId(id)
    if (el) return el
    await flushPromises()
    await new Promise((r) => setTimeout(r, 10))
  }
  return null
}
const title = () => document.body.querySelector('.v-toolbar-title')?.textContent?.trim()
const order = (fn: unknown, i = 0) => (fn as { mock: { invocationCallOrder: number[] } }).mock.invocationCallOrder[i]

describe('#1637: the call page follows the room in its URL', () => {
  it('in A\'s call, switching to B in place asks, then leaves A\'s call and joins B\'s', async () => {
    await openCallPage('A', true)
    expect(title()).toBe('Room A')

    const nav = switchInPlace('B')
    const confirm = await button('call-switch-confirm')
    confirm?.click()
    await nav
    await flushPromises()

    expect(rooms.leaveCall).toHaveBeenCalledWith('T', 'A')
    expect(conference.leaveRoom).toHaveBeenCalled()
    expect(rooms.joinCall).toHaveBeenCalledWith('T', 'B')
    expect(conference.joinRoom).toHaveBeenCalledWith('T', 'B', 'Room B')
    // A's call is left before B's is joined, never the other way round.
    expect(order(rooms.leaveCall)).toBeLessThan(order(rooms.joinCall))
    expect(rooms.leaveCall).not.toHaveBeenCalledWith('T', 'B')
    expect(router.currentRoute.value.params.roomId).toBe('B')
    expect(title()).toBe('Room B')
    expect(messages.fetchMessages).toHaveBeenLastCalledWith('T', 'B')
  })

  it('Cancel keeps the call and the page where they are', async () => {
    await openCallPage('A', true)

    const nav = switchInPlace('B')
    const cancel = await button('call-switch-cancel')
    expect(cancel, 'the page asks before it leaves a call').not.toBeNull()
    cancel!.click()
    await nav
    await flushPromises()

    expect(router.currentRoute.value.params.roomId).toBe('A')
    expect(rooms.leaveCall).not.toHaveBeenCalled()
    expect(conference.leaveRoom).not.toHaveBeenCalled()
    expect(conference.roomId).toBe('A')
    expect(title()).toBe('Room A')
  })

  it('a page in no call switches with no question and shows the new room', async () => {
    await openCallPage('A', false)
    expect(title()).toBe('Room A')

    await switchInPlace('B')
    await flushPromises()

    expect(byTestId('call-switch-confirm')).toBeNull()
    expect(rooms.leaveCall).not.toHaveBeenCalled()
    expect(rooms.joinCall).not.toHaveBeenCalled()
    expect(title()).toBe('Room B')
    expect(messages.fetchMessages).toHaveBeenLastCalledWith('T', 'B')
  })
})

describe('#1637: the join hand-off across the remount', () => {
  it('is for that one room, once', () => {
    handOffJoin('B', 1_000)
    expect(takeJoin('B', 1_001)).toBe(true)
    expect(takeJoin('B', 1_002)).toBe(false)
  })

  it('is spent by the next page whatever its room, so it cannot reach a later one', () => {
    handOffJoin('B', 1_000)
    expect(takeJoin('C', 1_001)).toBe(false)
    expect(takeJoin('B', 1_002)).toBe(false)
  })

  it('does not outlive a navigation that never arrived', () => {
    handOffJoin('B', 1_000)
    expect(takeJoin('B', 1_000 + CALL_SWITCH_HANDOFF_MS + 1)).toBe(false)
  })
})
