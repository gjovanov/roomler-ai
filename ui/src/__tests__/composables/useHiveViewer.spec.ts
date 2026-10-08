// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * FR-90 P0d-3 — the viewer peer's browser half against a fake socket and a
 * fake RTCPeerConnection: the handshake's ORDER (offer before our
 * candidates, the answer before theirs), the device's protocol over the
 * channel, and that every way out closes the peer.
 */
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { defineComponent, h, nextTick, reactive } from 'vue'
import { mount } from '@vue/test-utils'
import { decodeJson, encodeJson, Reassembler } from '@/utils/hiveFraming'

// ─── the user socket ────────────────────────────────────────────────────────
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
  /** The device answers with one JSON message. */
  deliver(message: unknown) {
    for (const f of encodeJson(9, message)) this.onmessage?.({ data: f.slice().buffer })
  }
  /** What the browser has sent so far, decoded. */
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
  static last: FakePeer | null = null
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
    FakePeer.last = this
  }
  createDataChannel(label: string) {
    expect(label).toBe('hive')
    return this.channel
  }
  async createOffer() {
    return { type: 'offer', sdp: 'v=0 browser-offer' }
  }
  async setLocalDescription() {
    // Gathering starts here: a candidate fires before the caller resumes.
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
  for (let i = 0; i < 5; i++) {
    await Promise.resolve()
    await nextTick()
  }
}

// eslint-disable-next-line @typescript-eslint/no-explicit-any
async function mountViewer(): Promise<{ viewer: any; unmount: () => void }> {
  const { useHiveViewer } = await import('@/composables/useHiveViewer')
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  let viewer: any
  const C = defineComponent({
    setup() {
      viewer = useHiveViewer()
      return () => h('div')
    },
  })
  const w = mount(C)
  return { viewer, unmount: () => w.unmount() }
}

describe('useHiveViewer', () => {
  beforeEach(() => {
    handlers.clear()
    sent.length = 0
    wsState.connectionId = 'conn-1'
    FakePeer.last = null
    vi.stubGlobal('RTCPeerConnection', FakePeer)
  })
  afterEach(() => {
    vi.unstubAllGlobals()
  })

  it('dials only after ready, sends its offer before its candidates, and buffers theirs until the answer', async () => {
    const { viewer, unmount } = await mountViewer()
    viewer.open('sess-1')
    const openMsg = sent.find((s) => s.type === 'hive:view.open')!
    expect(openMsg.data.session_id).toBe('sess-1')
    expect(FakePeer.last, 'no peer before the device confirmed').toBeNull()

    serverSays('hive:view.ready', {
      ref: openMsg.data.ref,
      grant_id: 'g1',
      session_id: 'sess-1',
      ice_servers: [{ urls: ['stun:example'] }],
      ttl_secs: 600,
      may_prompt: true,
    })
    await flush()
    const peer = FakePeer.last!
    expect(peer.config.iceServers).toEqual([{ urls: ['stun:example'] }])
    // The candidate gathered during setLocalDescription waited for the offer.
    const offerAt = sent.findIndex((s) => s.type === 'hive:view.offer')
    const iceAt = sent.findIndex((s) => s.type === 'hive:view.ice')
    expect(offerAt).toBeGreaterThan(-1)
    expect(iceAt).toBeGreaterThan(offerAt)
    expect(sent[offerAt].data).toEqual({ grant_id: 'g1', sdp: 'v=0 browser-offer' })
    expect(sent[iceAt].data).toEqual({ grant_id: 'g1', candidate: { candidate: 'early-local' } })

    // A device candidate before the answer waits; the answer flushes it.
    serverSays('hive:view.ice', { grant_id: 'g1', candidate: { candidate: 'device-1' } })
    await flush()
    expect(peer.added).toEqual([])
    serverSays('hive:view.answer', { grant_id: 'g1', sdp: 'v=0 device-answer' })
    await flush()
    expect(peer.remote).toEqual({ type: 'answer', sdp: 'v=0 device-answer' })
    expect(peer.added).toEqual([{ candidate: 'device-1' }])

    // A frame for someone else's grant changes nothing.
    serverSays('hive:view.closed', { grant_id: 'g-other', reason: 'x' })
    await flush()
    expect(peer.closed).toBe(false)

    unmount()
    expect(peer.closed).toBe(true)
    expect(sent.some((s) => s.type === 'hive:view.close' && s.data.grant_id === 'g1')).toBe(true)
  })

  it('speaks the device protocol over the channel and keeps events in order, once each', async () => {
    const { viewer, unmount } = await mountViewer()
    viewer.open('sess-2')
    const ref = sent.find((s) => s.type === 'hive:view.open')!.data.ref
    serverSays('hive:view.ready', { ref, grant_id: 'g2', ice_servers: [], ttl_secs: 600, may_prompt: true })
    await flush()
    const ch = FakePeer.last!.channel
    ch.open()
    await flush()
    expect(ch.requests()[0]).toEqual({ op: 'hello' })

    ch.deliver({ op: 'hello', v: 1, tip: 3, live: true, state: 'idle', may_prompt: true })
    await flush()
    expect(ch.requests()[1]).toEqual({ op: 'page', after: 0, limit: 300 })
    const ev = (seq: number) => ({ seq, ts: seq, fence: 1, event: { kind: 'note', text: `n${seq}` } })
    ch.deliver({ op: 'page', after: 0, events: [ev(1), ev(2), ev(3)], more: false, tip: 3 })
    await flush()
    expect(ch.requests()[2]).toEqual({ op: 'follow', after: 3 })
    expect(viewer.events.value.map((e: { seq: number }) => e.seq)).toEqual([1, 2, 3])

    // Live events append; a repeat is dropped.
    ch.deliver({ op: 'events', events: [ev(3), ev(4)] })
    ch.deliver({ op: 'state', state: 'running' })
    await flush()
    expect(viewer.events.value.map((e: { seq: number }) => e.seq)).toEqual([1, 2, 3, 4])
    expect(viewer.runState.value).toBe('running')

    // A prompt is answered by id.
    const pending = viewer.prompt('do it')
    await flush()
    const req = ch.requests().find((r) => r.op === 'prompt')!
    expect(req.text).toBe('do it')
    ch.deliver({ op: 'prompt', id: req.id, ok: false, error: 'read_only: no' })
    await expect(pending).resolves.toEqual({ ok: false, error: 'read_only: no' })
    unmount()
  })

  it('waits for the socket before asking: a cold load of the room loses nothing', async () => {
    // Field, 2026-10-07: the room mounted before the socket's first
    // `connected`, the store dropped the open, and the panel said
    // "Asking the device…" for ever.
    wsState.connectionId = null
    const { viewer, unmount } = await mountViewer()
    viewer.open('sess-cold')
    await flush()
    expect(sent.filter((s) => s.type === 'hive:view.open')).toEqual([])
    expect(viewer.status.value).toBe('opening')

    wsState.connectionId = 'conn-first'
    await flush()
    const opens = sent.filter((s) => s.type === 'hive:view.open')
    expect(opens.length).toBe(1)
    expect(opens[0].data.session_id).toBe('sess-cold')
    // …and it is the request the answer is routed by.
    serverSays('hive:view.ready', { ref: opens[0].data.ref, grant_id: 'g-cold', ice_servers: [], ttl_secs: 600 })
    await flush()
    expect(FakePeer.last).not.toBeNull()
    unmount()
  })

  it('asks again when a redial forgets a request still waiting for its answer', async () => {
    const { viewer, unmount } = await mountViewer()
    viewer.open('sess-redial')
    expect(sent.filter((s) => s.type === 'hive:view.open').length).toBe(1)
    wsState.connectionId = 'conn-2'
    await flush()
    const opens = sent.filter((s) => s.type === 'hive:view.open')
    expect(opens.length).toBe(2)
    expect(opens[1].data.ref).not.toBe(opens[0].data.ref)
    unmount()
  })

  it('asks hello again until the device answers, and gives up out loud', async () => {
    vi.useFakeTimers()
    try {
      const { viewer, unmount } = await mountViewer()
      viewer.open('sess-hello')
      const ref = sent.find((s) => s.type === 'hive:view.open')!.data.ref
      serverSays('hive:view.ready', { ref, grant_id: 'g-hello', ice_servers: [], ttl_secs: 600, may_prompt: true })
      await flush()
      const ch = FakePeer.last!.channel
      ch.open()
      await flush()
      // The first hello is lost on the device's side: nothing comes back.
      expect(ch.requests().filter((r) => r.op === 'hello').length).toBe(1)
      await vi.advanceTimersByTimeAsync(1500)
      await flush()
      expect(ch.requests().filter((r) => r.op === 'hello').length).toBe(2)
      ch.deliver({ op: 'hello', v: 1, tip: 0, live: true, state: 'idle', may_prompt: true })
      await flush()
      expect(ch.requests().at(-1)).toEqual({ op: 'page', after: 0, limit: 300 })
      unmount()

      // A device that never answers ends the viewer with a reason, not a hang.
      const second = await mountViewer()
      second.viewer.open('sess-silent')
      const ref2 = sent.filter((s) => s.type === 'hive:view.open').at(-1)!.data.ref
      serverSays('hive:view.ready', { ref: ref2, grant_id: 'g-silent', ice_servers: [], ttl_secs: 600 })
      await flush()
      FakePeer.last!.channel.open()
      await flush()
      await vi.advanceTimersByTimeAsync(4 * 1500 + 10)
      await flush()
      expect(second.viewer.status.value).toBe('closed')
      expect(second.viewer.reason.value).toBe('no_hello')
      second.unmount()
    } finally {
      vi.useRealTimers()
    }
  })

  it('knows which approvals are open, answers one by id, and forgets them with the peer', async () => {
    const { viewer, unmount } = await mountViewer()
    viewer.open('sess-appr')
    const ref = sent.find((s) => s.type === 'hive:view.open')!.data.ref
    serverSays('hive:view.ready', { ref, grant_id: 'g-appr', ice_servers: [], ttl_secs: 600, may_prompt: true })
    await flush()
    const ch = FakePeer.last!.channel
    ch.open()
    await flush()
    ch.deliver({
      op: 'hello',
      v: 1,
      tip: 0,
      live: true,
      state: 'awaiting_approval',
      may_prompt: true,
      may_answer: true,
      approvals: ['a1'],
    })
    await flush()
    expect(viewer.mayAnswer.value).toBe(true)
    expect(viewer.pendingApprovals.value).toEqual(['a1'])

    // A denial carries the driver's words; the answer is matched by id.
    const answered = viewer.answer('a1', 'deny', 'not now')
    await flush()
    const req = ch.requests().find((r) => r.op === 'answer')!
    expect(req).toEqual({ op: 'answer', id: req.id, approval: 'a1', decision: 'deny', message: 'not now' })
    ch.deliver({ op: 'answer', id: req.id, ok: true })
    await expect(answered).resolves.toEqual({ ok: true, error: undefined })

    // The device's list is the truth: what it no longer names is closed.
    ch.deliver({ op: 'approvals', pending: [] })
    await flush()
    expect(viewer.pendingApprovals.value).toEqual([])
    ch.deliver({ op: 'approvals', pending: ['a2'] })
    await flush()
    expect(viewer.pendingApprovals.value).toEqual(['a2'])

    // An answer still waiting when the peer goes is told so, and nothing is
    // answerable any more.
    const waiting = viewer.answer('a2', 'allow')
    await flush()
    expect(ch.requests().filter((r) => r.op === 'answer').at(-1)).toMatchObject({ approval: 'a2', decision: 'allow' })
    expect(ch.requests().filter((r) => r.op === 'answer').at(-1)).not.toHaveProperty('message')
    serverSays('hive:view.closed', { grant_id: 'g-appr', reason: 'expired' })
    await expect(waiting).resolves.toEqual({ ok: false, error: 'closed' })
    expect(viewer.pendingApprovals.value).toEqual([])
    unmount()
  })

  it('shows a session announcement once, not at every turn', async () => {
    const { withoutRepeatedInits } = await import('@/composables/useHiveViewer')
    const init = (seq: number, model = 'm') => ({
      seq,
      ts: seq,
      fence: 1,
      event: { kind: 'session_init', harness_session_id: 'h1', model, cwd: '/w', tools: [] },
    })
    const note = (seq: number) => ({ seq, ts: seq, fence: 1, event: { kind: 'note', text: `n${seq}` } })
    const shown = withoutRepeatedInits([init(1), note(2), init(3), note(4), init(5, 'other'), note(6)])
    expect(shown.map((e) => e.seq)).toEqual([1, 2, 4, 5, 6])
  })

  it('reports a refusal in the words it came with, and dials nothing', async () => {
    const { viewer, unmount } = await mountViewer()
    viewer.open('sess-3')
    const ref = sent.find((s) => s.type === 'hive:view.open')!.data.ref
    serverSays('hive:view.refused', { ref, session_id: 'sess-3', reason: 'no_session', message: 'store lost' })
    await flush()
    expect(viewer.status.value).toBe('refused')
    expect(viewer.reason.value).toBe('no_session: store lost')
    expect(FakePeer.last).toBeNull()
    unmount()
  })

  it('asks for a fresh grant when its part in the session changes (P1c)', async () => {
    const { viewer, unmount } = await mountViewer()
    viewer.open('sess-role')
    const first = sent.find((s) => s.type === 'hive:view.open')!.data
    serverSays('hive:view.ready', { ref: first.ref, grant_id: 'g-reader', ice_servers: [], ttl_secs: 600, may_prompt: false })
    await flush()
    const reader = FakePeer.last!
    reader.channel.open()
    await flush()
    reader.channel.deliver({ op: 'hello', v: 1, tip: 0, live: true, state: 'idle', may_prompt: false })
    await flush()
    expect(viewer.mayPrompt.value).toBe(false)

    // The owner made us a driver: the server ended the grant that said
    // "read only", and we ask again — the new grant says what is true now.
    serverSays('hive:view.closed', { grant_id: 'g-reader', reason: 'role_changed' })
    await flush()
    expect(reader.closed).toBe(true)
    const opens = sent.filter((s) => s.type === 'hive:view.open')
    expect(opens.length).toBe(2)
    expect(opens[1].data.session_id).toBe('sess-role')
    expect(opens[1].data.ref).not.toBe(first.ref)
    serverSays('hive:view.ready', { ref: opens[1].data.ref, grant_id: 'g-driver', ice_servers: [], ttl_secs: 600, may_prompt: true })
    await flush()
    expect(viewer.mayPrompt.value).toBe(true)

    // Any other end is not a cue to come back.
    serverSays('hive:view.closed', { grant_id: 'g-driver', reason: 'removed' })
    await flush()
    expect(sent.filter((s) => s.type === 'hive:view.open').length).toBe(2)
    expect(viewer.status.value).toBe('closed')
    unmount()
  })

  it("keeps the device's reason when it will not let a named driver act (P1c-2)", async () => {
    const { viewer, unmount } = await mountViewer()
    viewer.open('sess-unmapped')
    const ref = sent.find((s) => s.type === 'hive:view.open')!.data.ref
    serverSays('hive:view.ready', { ref, grant_id: 'g-unmapped', ice_servers: [], ttl_secs: 600, may_prompt: true })
    await flush()
    const ch = FakePeer.last!.channel
    ch.open()
    await flush()
    ch.deliver({
      op: 'hello',
      v: 1,
      tip: 0,
      live: true,
      state: 'idle',
      may_prompt: false,
      may_answer: false,
      driving_refused: "this device's hive_accounts maps you to no account",
    })
    await flush()
    expect(viewer.mayPrompt.value, "the device's word wins over the server's").toBe(false)
    expect(viewer.drivingRefused.value).toBe("this device's hive_accounts maps you to no account")
    unmount()
    expect(viewer.drivingRefused.value, 'forgotten with the peer').toBeNull()
  })

  it('closes the peer when the device ends the grant, and a new open replaces the old peer', async () => {
    const { viewer, unmount } = await mountViewer()
    viewer.open('sess-4')
    let ref = sent.find((s) => s.type === 'hive:view.open')!.data.ref
    serverSays('hive:view.ready', { ref, grant_id: 'g4', ice_servers: [], ttl_secs: 600 })
    await flush()
    const first = FakePeer.last!
    serverSays('hive:view.closed', { grant_id: 'g4', reason: 'expired' })
    await flush()
    expect(first.closed).toBe(true)
    expect(viewer.status.value).toBe('closed')
    expect(viewer.reason.value).toBe('expired')
    // The device's own end is not echoed back as a close.
    expect(sent.some((s) => s.type === 'hive:view.close')).toBe(false)

    viewer.open('sess-4')
    ref = sent.filter((s) => s.type === 'hive:view.open').at(-1)!.data.ref
    serverSays('hive:view.ready', { ref, grant_id: 'g5', ice_servers: [], ttl_secs: 600 })
    await flush()
    const second = FakePeer.last!
    expect(viewer.status.value).toBe('connecting')
    // Opening again (what a redialled socket triggers) closes the live peer
    // and says so to the server before asking anew.
    const opens = sent.filter((s) => s.type === 'hive:view.open').length
    viewer.open('sess-4')
    expect(second.closed).toBe(true)
    expect(sent.some((s) => s.type === 'hive:view.close' && s.data.grant_id === 'g5')).toBe(true)
    expect(sent.filter((s) => s.type === 'hive:view.open').length).toBe(opens + 1)
    unmount()
  })
  it('keeps a bounded page while it follows, the rest one page away (P1f-2)', async () => {
    const { viewer, unmount } = await mountViewer()
    viewer.open('sess-trim')
    const ref = sent.find((s) => s.type === 'hive:view.open')!.data.ref
    serverSays('hive:view.ready', { ref, grant_id: 'gt', ice_servers: [], ttl_secs: 600, may_prompt: true })
    await flush()
    const ch = FakePeer.last!.channel
    ch.open()
    await flush()
    ch.deliver({ op: 'hello', v: 1, tip: 3, live: true, state: 'idle', may_prompt: true })
    await flush()
    const ev = (seq: number) => ({ seq, ts: seq, fence: 1, event: { kind: 'note', text: `n${seq}` } })
    ch.deliver({ op: 'page', after: 0, events: [ev(1), ev(2), ev(3)], more: false, tip: 3 })
    await flush()
    ch.deliver({ op: 'events', events: [4, 5, 6, 7, 8, 9, 10].map(ev) })
    await flush()
    expect(viewer.hasEarlier.value).toBe(false)

    viewer.trimEarlier(4)
    expect(viewer.events.value.map((e: { seq: number }) => e.seq)).toEqual([7, 8, 9, 10])
    expect(viewer.hasEarlier.value).toBe(true)
    viewer.trimEarlier(4)
    expect(viewer.events.value).toHaveLength(4)

    // What was dropped is asked for again, exactly.
    const asked = viewer.loadEarlier()
    await flush()
    expect(ch.requests().filter((r) => r.op === 'page').pop()).toEqual({ op: 'page', after: 0, limit: 6 })
    ch.deliver({ op: 'page', after: 0, events: [1, 2, 3, 4, 5, 6].map(ev), more: false, tip: 10 })
    await asked
    expect(viewer.events.value.map((e: { seq: number }) => e.seq)).toEqual([1, 2, 3, 4, 5, 6, 7, 8, 9, 10])
    expect(viewer.hasEarlier.value).toBe(false)
    unmount()
  })
})
