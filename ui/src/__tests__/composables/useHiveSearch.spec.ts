// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * FR-90 P2i-3 — a search's browser half against a fake socket and a fake
 * RTCPeerConnection: the handshake's ORDER (a viewer's), the device's search
 * protocol over the channel, that a viewer and a search in one tab each get
 * their own frames from a store that holds ONE handler per type, and that
 * every way out closes the peer.
 */
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { defineComponent, h, nextTick, reactive } from 'vue'
import { mount } from '@vue/test-utils'
import { decodeJson, encodeJson, Reassembler } from '@/utils/hiveFraming'

// ─── the user socket: ONE handler per type, as the store's Map ──────────────
// eslint-disable-next-line @typescript-eslint/no-explicit-any
type Handler = (data: any) => void
const handlers = new Map<string, Handler>()
const sent: { type: string; data: Record<string, unknown> }[] = []
const wsState = reactive<{ connectionId: string | null }>({ connectionId: 'conn-1' })
vi.mock('@/stores/ws', () => ({
  useWsStore: () => ({
    get connectionId() {
      return wsState.connectionId
    },
    send: (type: string, data: Record<string, unknown>) => sent.push({ type, data }),
    onMediaMessage: (type: string, h: Handler) => handlers.set(type, h),
  }),
}))

function serverSays(type: string, data: Record<string, unknown>): void {
  handlers.get(type)?.(data)
}

// ─── a fake peer ────────────────────────────────────────────────────────────
class FakeChannel {
  readyState = 'connecting'
  binaryType = 'blob'
  onopen: (() => void) | null = null
  onclose: (() => void) | null = null
  onmessage: ((ev: { data: ArrayBuffer }) => void) | null = null
  outbox: Uint8Array[] = []
  closed = false
  send(frame: Uint8Array) {
    this.outbox.push(frame)
  }
  close() {
    this.closed = true
  }
  open() {
    this.readyState = 'open'
    this.onopen?.()
  }
  deliver(message: unknown) {
    for (const f of encodeJson(9, message)) this.onmessage?.({ data: f.slice().buffer })
  }
  requests(): Record<string, unknown>[] {
    const r = new Reassembler({ maxMessageBytes: 1 << 20, maxInFlight: 4 })
    const out: Record<string, unknown>[] = []
    for (const f of this.outbox) {
      const m = r.push(f)
      if (m instanceof Uint8Array) out.push(decodeJson(m) as Record<string, unknown>)
    }
    return out
  }
}

class FakePeer {
  static all: FakePeer[] = []
  config: RTCConfiguration
  channel = new FakeChannel()
  remote: RTCSessionDescriptionInit | null = null
  added: RTCIceCandidateInit[] = []
  closed = false
  connectionState = 'new'
  onicecandidate: ((ev: { candidate: { toJSON: () => RTCIceCandidateInit } | null }) => void) | null = null
  onconnectionstatechange: (() => void) | null = null
  constructor(config: RTCConfiguration) {
    this.config = config
    FakePeer.all.push(this)
  }
  static get last(): FakePeer | undefined {
    return FakePeer.all.at(-1)
  }
  createDataChannel(label: string) {
    expect(label).toBe('hive')
    return this.channel
  }
  async createOffer() {
    return { type: 'offer', sdp: 'v=0 browser-offer' }
  }
  async setLocalDescription() {
    this.onicecandidate?.({ candidate: { toJSON: () => ({ candidate: 'early-local' }) } })
  }
  async setRemoteDescription(d: RTCSessionDescriptionInit) {
    this.remote = d
  }
  async addIceCandidate(c: RTCIceCandidateInit) {
    if (!this.remote) throw new Error('addIceCandidate before setRemoteDescription')
    this.added.push(c)
  }
  close() {
    this.closed = true
  }
}

async function flush(): Promise<void> {
  for (let i = 0; i < 6; i++) {
    await Promise.resolve()
    await nextTick()
  }
}

// eslint-disable-next-line @typescript-eslint/no-explicit-any
async function mountSearch(): Promise<{ search: any; unmount: () => void }> {
  const { useHiveSearch } = await import('@/composables/useHiveSearch')
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  let search: any
  const C = defineComponent({
    setup() {
      search = useHiveSearch()
      return () => h('div')
    },
  })
  const w = mount(C)
  return { search, unmount: () => w.unmount() }
}

const opens = () => sent.filter((s) => s.type === 'hive:search.open')

/** The open answered, the peer up and the device's hello: a search peer. */
async function bringUp(grant: string, sessions = 3): Promise<FakePeer> {
  const ref = opens().at(-1)!.data.ref
  serverSays('hive:search.ready', { ref, grant_id: grant, ice_servers: [], ttl_secs: 600, sessions })
  await flush()
  const peer = FakePeer.last!
  peer.channel.open()
  await flush()
  peer.channel.deliver({ op: 'hello', v: 1, search: true, sessions })
  await flush()
  return peer
}

/** The device answers the latest search it was asked. */
function answerLatest(peer: FakePeer, answer: Record<string, unknown>): Record<string, unknown> {
  const req = peer.channel.requests().filter((r) => r.op === 'search').at(-1)!
  peer.channel.deliver({ op: 'search', id: req.id, ...answer })
  return req
}

describe('useHiveSearch', () => {
  beforeEach(() => {
    handlers.clear()
    sent.length = 0
    wsState.connectionId = 'conn-1'
    FakePeer.all = []
    vi.stubGlobal('RTCPeerConnection', FakePeer)
  })
  afterEach(() => {
    vi.unstubAllGlobals()
    vi.useRealTimers()
  })

  it('opens at the first search, dials only after ready in a viewer\'s order, then asks hello and the search', async () => {
    const { search, unmount } = await mountSearch()
    expect(opens(), 'nothing asked before a search').toEqual([])
    const done = search.run('t1', '  kubelet  ')
    await flush()
    expect(opens().length).toBe(1)
    expect(opens()[0].data.tenant_id).toBe('t1')
    expect(FakePeer.last, 'no peer before the device confirmed').toBeUndefined()
    expect(search.status.value).toBe('opening')

    serverSays('hive:search.ready', {
      ref: opens()[0].data.ref,
      grant_id: 'gs1',
      ice_servers: [{ urls: ['stun:example'] }],
      ttl_secs: 600,
      sessions: 7,
    })
    await flush()
    expect(FakePeer.all.length, 'dialled once the device confirmed').toBe(1)
    const peer = FakePeer.last!
    expect(peer.config.iceServers).toEqual([{ urls: ['stun:example'] }])
    expect(search.named.value).toBe(7)
    // The candidate gathered during setLocalDescription waited for the offer;
    // the peer rides the VIEW's frames, by its grant.
    const offerAt = sent.findIndex((s) => s.type === 'hive:view.offer')
    const iceAt = sent.findIndex((s) => s.type === 'hive:view.ice')
    expect(sent[offerAt].data).toEqual({ grant_id: 'gs1', sdp: 'v=0 browser-offer' })
    expect(iceAt, 'our candidates wait for our offer').toBeGreaterThan(offerAt)
    expect(sent[iceAt].data).toEqual({ grant_id: 'gs1', candidate: { candidate: 'early-local' } })

    // Theirs waits for the answer.
    serverSays('hive:view.ice', { grant_id: 'gs1', candidate: { candidate: 'device-1' } })
    await flush()
    expect(peer.added).toEqual([])
    serverSays('hive:view.answer', { grant_id: 'gs1', sdp: 'v=0 device-answer' })
    await flush()
    expect(peer.remote).toEqual({ type: 'answer', sdp: 'v=0 device-answer' })
    expect(peer.added, 'theirs, held until the answer').toEqual([{ candidate: 'device-1' }])

    peer.channel.open()
    await flush()
    expect(peer.channel.requests()).toEqual([{ op: 'hello' }])
    peer.channel.deliver({ op: 'hello', v: 1, search: true, sessions: 5 })
    await flush()
    expect(search.status.value).toBe('open')
    expect(search.searched.value).toBe(5)
    const req = peer.channel.requests()[1]
    expect(req).toEqual({ op: 'search', id: req.id, q: 'kubelet', limit: 20 })

    peer.channel.deliver({
      op: 'search',
      id: req.id,
      ok: true,
      hits: [{ session: 's1', seq: 4, snippet: 'the [kubelet] restarted' }, { bogus: true }],
    })
    await done
    expect(search.hits.value).toEqual([{ session: 's1', seq: 4, snippet: 'the [kubelet] restarted' }])
    expect(search.answered.value).toBe('kubelet')
    expect(search.searching.value).toBe(false)

    // A second search rides the same peer.
    const again = search.run('t1', 'etcd')
    await flush()
    expect(opens().length).toBe(1)
    answerLatest(peer, { ok: true, hits: [] })
    await again
    expect(search.hits.value).toEqual([])
    expect(search.answered.value).toBe('etcd')

    unmount()
    expect(peer.closed).toBe(true)
    expect(peer.channel.closed).toBe(true)
    expect(sent.some((s) => s.type === 'hive:view.close' && s.data.grant_id === 'gs1')).toBe(true)
  })

  it('shares the socket with a viewer: each gets its own frames, by ref and by grant', async () => {
    // The ws store holds ONE handler per frame type. Were the search to
    // register `hive:view.answer` itself, the viewer's answer would be lost.
    const { useHiveViewer } = await import('@/composables/useHiveViewer')
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    let viewer: any
    const V = defineComponent({
      setup() {
        viewer = useHiveViewer()
        return () => h('div')
      },
    })
    const vw = mount(V)
    const { search, unmount } = await mountSearch()

    viewer.open('sess-v')
    const viewRef = sent.find((s) => s.type === 'hive:view.open')!.data.ref
    void search.run('t1', 'disk')
    await flush()
    const searchRef = opens()[0].data.ref
    expect(searchRef).not.toBe(viewRef)

    serverSays('hive:view.ready', { ref: viewRef, grant_id: 'gv', ice_servers: [], ttl_secs: 600 })
    await flush()
    const viewPeer = FakePeer.last!
    serverSays('hive:search.ready', { ref: searchRef, grant_id: 'gs', ice_servers: [], ttl_secs: 600, sessions: 2 })
    await flush()
    const searchPeer = FakePeer.last!
    expect(FakePeer.all.length).toBe(2)
    expect(searchPeer).not.toBe(viewPeer)

    serverSays('hive:view.answer', { grant_id: 'gv', sdp: 'v=0 view-answer' })
    serverSays('hive:view.answer', { grant_id: 'gs', sdp: 'v=0 search-answer' })
    await flush()
    expect(viewPeer.remote, "the viewer's answer reaches the viewer").toEqual({ type: 'answer', sdp: 'v=0 view-answer' })
    expect(searchPeer.remote, "the search's answer reaches the search").toEqual({
      type: 'answer',
      sdp: 'v=0 search-answer',
    })

    // One grant's end is that grant's alone.
    serverSays('hive:view.closed', { grant_id: 'gs', reason: 'expired' })
    await flush()
    expect(searchPeer.closed).toBe(true)
    expect(viewPeer.closed).toBe(false)
    expect(viewer.status.value).toBe('connecting')

    unmount()
    vw.unmount()
    expect(viewPeer.closed).toBe(true)
  })

  it('lands only the latest answer, and an empty query clears without asking', async () => {
    const { search, unmount } = await mountSearch()
    const first = search.run('t1', 'old')
    await flush()
    const peer = await bringUp('g-latest')
    const firstReq = peer.channel.requests().find((r) => r.op === 'search')!
    const second = search.run('t1', 'new')
    await flush()
    const secondReq = peer.channel.requests().filter((r) => r.op === 'search').at(-1)!
    expect(secondReq.q).toBe('new')

    // The newer answer first, then the older one: the older is dropped.
    peer.channel.deliver({ op: 'search', id: secondReq.id, ok: true, hits: [{ session: 's2', seq: 2, snippet: '[new]' }] })
    await second
    peer.channel.deliver({ op: 'search', id: firstReq.id, ok: true, hits: [{ session: 's1', seq: 1, snippet: '[old]' }] })
    await first
    expect(
      search.hits.value.map((h: { snippet: string }) => h.snippet),
      'an older answer is dropped',
    ).toEqual(['[new]'])
    expect(search.answered.value).toBe('new')

    const asked = peer.channel.requests().length
    await search.run('t1', '   ')
    expect(peer.channel.requests().length).toBe(asked)
    expect(search.hits.value).toEqual([])
    expect(search.answered.value).toBeNull()
    unmount()
  })

  it('waits out one too_soon, and shows any other refusal in the device\'s words', async () => {
    vi.useFakeTimers()
    const { search, unmount } = await mountSearch()
    const done = search.run('t1', 'fast')
    await flush()
    const peer = await bringUp('g-soon')
    answerLatest(peer, { ok: false, error: 'too_soon: one search every 250 ms' })
    await flush()
    expect(peer.channel.requests().filter((r) => r.op === 'search').length).toBe(1)
    await vi.advanceTimersByTimeAsync(300)
    await flush()
    const asked = peer.channel.requests().filter((r) => r.op === 'search')
    expect(asked.length, 'asked once more').toBe(2)
    expect(asked[1].q).toBe('fast')
    answerLatest(peer, { ok: true, hits: [{ session: 's', seq: 1, snippet: '[fast]' }] })
    await done
    expect(search.hits.value.length).toBe(1)
    expect(search.error.value).toBeNull()

    const refused = search.run('t1', 'x'.repeat(3))
    await flush()
    answerLatest(peer, { ok: false, error: 'this device could not search its copies' })
    await vi.advanceTimersByTimeAsync(300)
    await flush()
    expect(peer.channel.requests().filter((r) => r.op === 'search').length, 'no retry but too_soon').toBe(3)
    await refused
    expect(search.hits.value).toEqual([])
    expect(search.error.value).toBe('this device could not search its copies')
    unmount()
  })

  it('keeps a refusal with its detail, and asks the server again only after a pause', async () => {
    vi.useFakeTimers()
    const { search, unmount } = await mountSearch()
    const done = search.run('t1', 'word')
    await flush()
    serverSays('hive:search.refused', { ref: opens()[0].data.ref, reason: 'no_session', message: 'none held' })
    await done
    expect(search.status.value).toBe('refused')
    expect(search.reason.value).toBe('no_session')
    expect(search.detail.value).toBe('none held')
    expect(search.error.value, 'a refusal is no failure').toBeNull()
    expect(FakePeer.all).toEqual([])

    // Typing on does not spend the minute's opens.
    void search.run('t1', 'words')
    await flush()
    expect(opens().length, 'typing on asks the server nothing').toBe(1)
    expect(search.status.value).toBe('refused')

    await vi.advanceTimersByTimeAsync(10_000)
    void search.run('t1', 'words again')
    await flush()
    expect(opens().length).toBe(2)
    expect(search.status.value).toBe('opening')
    unmount()
  })

  it('a grant the server ends closes the peer; the next search asks for a fresh one', async () => {
    const { search, unmount } = await mountSearch()
    const done = search.run('t1', 'one')
    await flush()
    const peer = await bringUp('g-end')
    answerLatest(peer, { ok: true, hits: [] })
    await done

    // Taken out of a room it named: the server ends the grant.
    serverSays('hive:view.closed', { grant_id: 'g-end', reason: 'removed' })
    await flush()
    expect(peer.closed).toBe(true)
    expect(search.status.value).toBe('closed')
    expect(search.reason.value).toBe('removed')
    expect(sent.some((s) => s.type === 'hive:view.close'), "the server's own end is not echoed").toBe(false)

    void search.run('t1', 'two')
    await flush()
    expect(opens().length, 'the next search asks for a fresh grant').toBe(2)
    expect(opens()[1].data.ref).not.toBe(opens()[0].data.ref)
    unmount()
  })

  it('a search on its way when the grant ends is asked once more, on a fresh grant', async () => {
    const { search, unmount } = await mountSearch()
    const done = search.run('t1', 'pending')
    await flush()
    const first = await bringUp('g-a')
    expect(first.channel.requests().filter((r) => r.op === 'search').length).toBe(1)
    serverSays('hive:view.closed', { grant_id: 'g-a', reason: 'expired' })
    await flush()
    expect(opens().length, 'asked once more, on a fresh grant').toBe(2)
    const second = await bringUp('g-b')
    answerLatest(second, { ok: true, hits: [{ session: 's', seq: 3, snippet: '[pending]' }] })
    await done
    expect(search.hits.value.length).toBe(1)
    expect(search.error.value).toBeNull()
    unmount()
  })

  it('asks on the new connection when the socket redials mid-search', async () => {
    const { search, unmount } = await mountSearch()
    const done = search.run('t1', 'redial')
    await flush()
    const first = await bringUp('g-r1')
    // A redial as the store makes one: the id cleared at the close, then a
    // new connection's.
    wsState.connectionId = null
    await flush()
    expect(first.closed, 'the old peer is closed at the redial').toBe(true)
    expect(opens().length, 'nothing asked while the socket is down').toBe(1)
    wsState.connectionId = 'conn-2'
    await flush()
    expect(opens().length).toBe(2)
    const second = await bringUp('g-r2')
    answerLatest(second, { ok: true, hits: [] })
    await done
    expect(search.answered.value).toBe('redial')
    unmount()
  })

  it('waits for the socket before asking: a cold load loses nothing', async () => {
    wsState.connectionId = null
    const { search, unmount } = await mountSearch()
    void search.run('t1', 'cold')
    await flush()
    expect(opens(), 'nothing asked before the socket is up').toEqual([])
    wsState.connectionId = 'conn-first'
    await flush()
    expect(opens().length).toBe(1)
    expect(opens()[0].data.tenant_id).toBe('t1')
    unmount()
  })

  it('gives a peer left unused back to the device, and opens again at the next search', async () => {
    vi.useFakeTimers()
    const { search, unmount } = await mountSearch()
    const done = search.run('t1', 'idle')
    await flush()
    const peer = await bringUp('g-idle')
    answerLatest(peer, { ok: true, hits: [] })
    await done
    await vi.advanceTimersByTimeAsync(119_000)
    expect(peer.closed).toBe(false)
    await vi.advanceTimersByTimeAsync(1_500)
    expect(peer.closed, 'an unused peer is given back').toBe(true)
    expect(sent.some((s) => s.type === 'hive:view.close' && s.data.grant_id === 'g-idle')).toBe(true)
    expect(search.status.value).toBe('idle')

    void search.run('t1', 'back')
    await flush()
    expect(opens().length).toBe(2)
    unmount()
  })

  it('gives up out loud on an open nobody answers', async () => {
    vi.useFakeTimers()
    const { search, unmount } = await mountSearch()
    const done = search.run('t1', 'silence')
    await flush()
    await vi.advanceTimersByTimeAsync(30_000)
    await flush()
    // The retry asks once more, and is not answered either.
    expect(opens().length, 'an open nobody answers is given up on').toBe(2)
    await vi.advanceTimersByTimeAsync(30_000)
    await done
    expect(search.searching.value).toBe(false)
    expect(search.error.value).toBe('no_answer')
    unmount()
  })

  it('takes a peer whose hello is no search\'s for no search peer', async () => {
    const { search, unmount } = await mountSearch()
    void search.run('t1', 'view')
    await flush()
    const ref = opens()[0].data.ref
    serverSays('hive:search.ready', { ref, grant_id: 'g-view', ice_servers: [], ttl_secs: 600, sessions: 1 })
    await flush()
    const peer = FakePeer.last!
    peer.channel.open()
    await flush()
    peer.channel.deliver({ op: 'hello', v: 1, tip: 3, live: true })
    await flush()
    expect(peer.closed, "a hello that is no search's").toBe(true)
    expect(peer.channel.requests().some((r) => r.op === 'search')).toBe(false)
    unmount()
  })
})

describe('snippetParts', () => {
  it('marks what the device marked, and leaves the rest as text', async () => {
    const { snippetParts } = await import('@/composables/useHiveSearch')
    expect(snippetParts('…the [kubelet] restarted [twice]')).toEqual([
      { text: '…the ', mark: false },
      { text: 'kubelet', mark: true },
      { text: ' restarted ', mark: false },
      { text: 'twice', mark: true },
    ])
    expect(snippetParts('no match marked')).toEqual([{ text: 'no match marked', mark: false }])
    // A bracket left open, or one closed with nothing opened, is text.
    expect(snippetParts('a ] b [c'), 'a bracket left open is text').toEqual([{ text: 'a ] b [c', mark: false }])
    expect(snippetParts('[]x')).toEqual([{ text: 'x', mark: false }])
    expect(snippetParts('')).toEqual([])
  })
})
