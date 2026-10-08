// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * FR-90 P0d-3 — the browser's half of the viewer peer: read an agent
 * session's transcript straight from the device that holds it.
 *
 * The handshake rides the user WebSocket (`hive:view.*`): the server checks
 * that we may read the session, gets the DEVICE to confirm a grant, and only
 * then says `ready` with the ICE servers minted for this peer (FR-83). The
 * transcript then flows over one data-only WebRTC peer and never through the
 * server.
 *
 * Over the peer's one DataChannel (`hive`), JSON messages travel in binary
 * frames (`utils/hiveFraming.ts` — an SCTP message over 64 KiB is silently
 * lost). We ask: `hello`, `page {after, limit}`, `follow {after}`,
 * `prompt {id, text}`; the device answers and pushes `events` and `state`.
 *
 * ⚠️ The peer is CLOSED on every way out — unmount, close(), a grant the
 * server or the device ends, a socket that redials. A WebRTC peer that is
 * merely dropped frees nothing; leaked peers once ate a host's whole
 * ephemeral port range.
 */
import { onBeforeUnmount, ref, shallowRef, watch } from 'vue'
import { useWsStore } from '@/stores/ws'
import { Reassembler, decodeJson, encodeJson, type FrameError } from '@/utils/hiveFraming'

/** One transcript event — `crates/hive-node/src/event.rs`. */
export type TranscriptEvent =
  | { kind: 'session_init'; harness_session_id: string; model?: string | null; cwd?: string | null; tools?: string[] }
  | { kind: 'user_message'; author?: string | null; text: string }
  | { kind: 'assistant_text'; text: string }
  | { kind: 'thinking'; summary: string }
  | { kind: 'tool_use'; id: string; name: string; input: unknown }
  | { kind: 'tool_result'; tool_use_id: string; ok: boolean; output: string; truncated?: boolean }
  | {
      kind: 'turn'
      ok: boolean
      subtype?: string | null
      num_turns?: number | null
      duration_ms?: number | null
      cost_usd?: number | null
    }
  | { kind: 'compaction'; trigger?: string | null; pre_tokens?: number | null }
  | { kind: 'note'; text: string }
  /** A kind this build does not know: shown as such, never dropped. */
  | { kind: string; [field: string]: unknown }

export interface HiveEvent {
  seq: number
  ts: number
  fence: number
  event: TranscriptEvent
}

export type ViewerStatus = 'idle' | 'opening' | 'connecting' | 'open' | 'closed' | 'refused'

/** The latest this many events on open; earlier ones on request. */
const HISTORY = 300
/** `hello` is asked again this often, this many times, until it is answered. */
const HELLO_RETRY_MS = 1500
const HELLO_ATTEMPTS = 4
/** What we accept from the device: pages are ≤ 1 MiB plus framing. */
const INBOUND = { maxMessageBytes: 4 * 1024 * 1024, maxInFlight: 4 }

// ─── One router for every viewer: the ws store holds ONE handler per type ───

// eslint-disable-next-line @typescript-eslint/no-explicit-any
type Frame = { type: string } & Record<string, any>
const byRef = new Map<string, (f: Frame) => void>()
const byGrant = new Map<string, (f: Frame) => void>()

function routeFrames(ws: ReturnType<typeof useWsStore>): void {
  // Re-registered on every open: `disconnect()` clears the store's handlers.
  for (const type of ['hive:view.ready', 'hive:view.refused']) {
    ws.onMediaMessage(type, (data) => {
      const handler = data?.ref ? byRef.get(data.ref) : undefined
      handler?.({ type, ...data })
    })
  }
  for (const type of ['hive:view.answer', 'hive:view.ice', 'hive:view.renewed', 'hive:view.closed']) {
    ws.onMediaMessage(type, (data) => {
      const handler = data?.grant_id ? byGrant.get(data.grant_id) : undefined
      handler?.({ type, ...data })
    })
  }
}

function newRef(): string {
  return Math.random().toString(36).slice(2, 12)
}

/** The events worth showing: Claude Code on stream-json announces its
 *  session again at EVERY prompt, so a `session_init` that repeats the one
 *  before it (same harness session, model and folder) is a turn boundary,
 *  not news. A changed one — a resume elsewhere, another model — still shows. */
export function withoutRepeatedInits(events: HiveEvent[]): HiveEvent[] {
  let last: string | null = null
  return events.filter((e) => {
    if (e.event.kind !== 'session_init') return true
    const ev = e.event as { harness_session_id?: unknown; model?: unknown; cwd?: unknown }
    const key = JSON.stringify([ev.harness_session_id, ev.model, ev.cwd])
    const repeat = key === last
    last = key
    return !repeat
  })
}

export function useHiveViewer() {
  const ws = useWsStore()

  const status = ref<ViewerStatus>('idle')
  const reason = ref<string | null>(null)
  const events = shallowRef<HiveEvent[]>([])
  const runState = ref<string | null>(null)
  const mayPrompt = ref(false)
  const live = ref(false)
  const hasEarlier = ref(false)

  let sessionId: string | null = null
  let reference: string | null = null
  let grantId: string | null = null
  let pc: RTCPeerConnection | null = null
  let dc: RTCDataChannel | null = null
  let renewTimer: ReturnType<typeof setInterval> | null = null
  let gen = 0
  let nextId = 0
  let reasm = new Reassembler(INBOUND)
  const pendingRemoteIce: RTCIceCandidateInit[] = []
  let remoteSet = false
  let offerSent = false
  const pendingLocalIce: RTCIceCandidateInit[] = []
  // One request in flight per kind: `hello` and `page` are asked one at a time.
  let helloWaiter: ((m: Frame) => void) | null = null
  let pageWaiter: ((m: Frame) => void) | null = null
  const promptWaiters = new Map<string, (m: Frame) => void>()
  // An open asked for before the socket was up, sent once it is.
  let openPending = false

  function send(message: unknown): void {
    if (!dc || dc.readyState !== 'open') return
    for (const frame of encodeJson(nextId++, message)) dc.send(frame)
  }

  function ask(kind: 'hello' | 'page', message: unknown): Promise<Frame> {
    return new Promise((resolve) => {
      if (kind === 'hello') helloWaiter = resolve
      else pageWaiter = resolve
      send(message)
    })
  }

  /** `hello`, asked again until the device answers. A device that wired its
   *  channel a moment after it opened dropped the very first frame — field,
   *  2026-10-07: the history was never asked for and a whole turn never
   *  reached the panel. Fixed on the device too; a viewer must still not
   *  hang on one lost frame, and `hello` is idempotent. */
  async function askHello(myGen: number): Promise<Frame | null> {
    for (let attempt = 0; attempt < HELLO_ATTEMPTS; attempt++) {
      const answer = await Promise.race([
        ask('hello', { op: 'hello' }),
        new Promise<null>((resolve) => setTimeout(() => resolve(null), HELLO_RETRY_MS)),
      ])
      if (myGen !== gen) return null
      if (answer && answer.op === 'hello') return answer
    }
    return null
  }

  function teardown(sayClose: boolean): void {
    gen++
    if (renewTimer) clearInterval(renewTimer)
    renewTimer = null
    if (reference) byRef.delete(reference)
    if (grantId) {
      byGrant.delete(grantId)
      if (sayClose) ws.send('hive:view.close', { grant_id: grantId })
    }
    try {
      dc?.close()
    } catch {
      /* already closed */
    }
    try {
      pc?.close()
    } catch {
      /* already closed */
    }
    dc = null
    pc = null
    grantId = null
    reference = null
    openPending = false
    remoteSet = false
    offerSent = false
    pendingRemoteIce.length = 0
    pendingLocalIce.length = 0
    // Resolve what is waiting, so no async step hangs on a peer that is gone
    // (each checks its generation and stops).
    const waiting = [helloWaiter, pageWaiter]
    helloWaiter = null
    pageWaiter = null
    for (const resolve of waiting) resolve?.({ type: 'closed' })
    for (const resolve of promptWaiters.values()) resolve({ type: 'prompt', ok: false, error: 'closed' })
    promptWaiters.clear()
  }

  function merge(incoming: HiveEvent[], where: 'append' | 'prepend'): void {
    if (incoming.length === 0) return
    const have = events.value
    if (where === 'append') {
      const last = have.length ? have[have.length - 1].seq : 0
      const fresh = incoming.filter((e) => e.seq > last)
      if (fresh.length) events.value = [...have, ...fresh]
    } else {
      const first = have.length ? have[0].seq : Infinity
      const older = incoming.filter((e) => e.seq < first)
      if (older.length) events.value = [...older, ...have]
    }
  }

  function onDeviceMessage(m: Frame): void {
    switch (m.op) {
      case 'hello':
        helloWaiter?.(m)
        helloWaiter = null
        break
      case 'page':
        pageWaiter?.(m)
        pageWaiter = null
        break
      case 'events':
        merge((m.events ?? []) as HiveEvent[], 'append')
        break
      case 'state':
        runState.value = m.state ?? null
        break
      case 'prompt': {
        const resolve = promptWaiters.get(m.id)
        promptWaiters.delete(m.id)
        resolve?.(m)
        break
      }
      case 'error':
        console.warn('[hive] the device answered an error:', m.error)
        break
      default:
        break
    }
  }

  async function onChannelOpen(myGen: number): Promise<void> {
    status.value = 'open'
    const hello = await askHello(myGen)
    if (myGen !== gen) return
    if (!hello) {
      finish('closed', 'no_hello')
      return
    }
    mayPrompt.value = !!hello.may_prompt
    live.value = !!hello.live
    runState.value = hello.state ?? null
    const tip = Number(hello.tip ?? 0)
    const after = Math.max(0, tip - HISTORY)
    const page = await ask('page', { op: 'page', after, limit: HISTORY })
    if (myGen !== gen) return
    events.value = []
    merge((page.events ?? []) as HiveEvent[], 'append')
    hasEarlier.value = after > 0
    const last = events.value.length ? events.value[events.value.length - 1].seq : after
    send({ op: 'follow', after: last })
  }

  async function onFrame(f: Frame, myGen: number): Promise<void> {
    if (myGen !== gen || !pc) return
    switch (f.type) {
      case 'hive:view.answer':
        try {
          await pc.setRemoteDescription({ type: 'answer', sdp: f.sdp })
          remoteSet = true
          for (const c of pendingRemoteIce) await pc.addIceCandidate(c).catch(() => {})
          pendingRemoteIce.length = 0
        } catch (e) {
          finish('closed', `answer refused: ${(e as Error).message}`)
        }
        break
      case 'hive:view.ice':
        if (!f.candidate) return
        if (!remoteSet) pendingRemoteIce.push(f.candidate)
        else await pc.addIceCandidate(f.candidate).catch(() => {})
        break
      case 'hive:view.closed':
        finish('closed', f.reason ?? 'closed', false)
        break
      case 'hive:view.renewed':
        break
    }
  }

  function finish(to: ViewerStatus, why: string | null, sayClose = true): void {
    teardown(sayClose)
    status.value = to
    reason.value = why
  }

  async function dial(ready: Frame, myGen: number): Promise<void> {
    grantId = ready.grant_id
    byGrant.set(ready.grant_id, (f) => void onFrame(f, myGen))
    mayPrompt.value = !!ready.may_prompt
    status.value = 'connecting'
    const ttl = Number(ready.ttl_secs ?? 600)
    renewTimer = setInterval(() => ws.send('hive:view.renew', { grant_id: grantId }), (ttl * 1000) / 2)

    pc = new RTCPeerConnection({ iceServers: ready.ice_servers ?? [], bundlePolicy: 'max-bundle' })
    pc.onicecandidate = (ev) => {
      if (!ev.candidate || myGen !== gen) return
      const c = ev.candidate.toJSON()
      if (!offerSent) pendingLocalIce.push(c)
      else ws.send('hive:view.ice', { grant_id: grantId, candidate: c })
    }
    pc.onconnectionstatechange = () => {
      if (myGen === gen && pc?.connectionState === 'failed') finish('closed', 'peer_failed')
    }
    dc = pc.createDataChannel('hive')
    dc.binaryType = 'arraybuffer'
    dc.onopen = () => void onChannelOpen(myGen)
    dc.onclose = () => {
      if (myGen === gen && status.value === 'open') finish('closed', 'device_left')
    }
    dc.onmessage = (ev) => {
      if (!(ev.data instanceof ArrayBuffer)) return
      const got: Uint8Array | null | FrameError = reasm.push(new Uint8Array(ev.data))
      if (got instanceof Uint8Array) {
        const m = decodeJson(got)
        if (m && typeof m === 'object') onDeviceMessage(m as Frame)
      }
    }
    const offer = await pc.createOffer()
    if (myGen !== gen || !pc) return
    await pc.setLocalDescription(offer)
    if (myGen !== gen) return
    ws.send('hive:view.offer', { grant_id: grantId, sdp: offer.sdp })
    offerSent = true
    for (const c of pendingLocalIce) ws.send('hive:view.ice', { grant_id: grantId, candidate: c })
    pendingLocalIce.length = 0
  }

  /** Open the session's viewer peer. Replaces any open one. */
  function open(session: string): void {
    teardown(true)
    sessionId = session
    const myGen = gen
    reasm = new Reassembler(INBOUND)
    events.value = []
    hasEarlier.value = false
    runState.value = null
    reason.value = null
    status.value = 'opening'
    routeFrames(ws)
    reference = newRef()
    byRef.set(reference, (f) => {
      if (myGen !== gen) return
      if (reference) byRef.delete(reference)
      if (f.type === 'hive:view.refused') {
        finish('refused', f.reason ?? 'refused', false)
        if (f.message) reason.value = `${f.reason}: ${f.message}`
        return
      }
      void dial(f, myGen).catch((e) => finish('closed', (e as Error).message))
    })
    // The socket may not be up yet — a cold load of the room mounts this
    // before the socket's first `connected` — and the store DROPS a frame it
    // cannot send. Ask once it is (field, 2026-10-07: "Asking the device…"
    // for ever).
    if (ws.connectionId) sendOpen()
    else openPending = true
  }

  function sendOpen(): void {
    if (!sessionId || !reference) return
    openPending = false
    ws.send('hive:view.open', { session_id: sessionId, ref: reference })
  }

  /** The events before the first one shown. */
  async function loadEarlier(): Promise<void> {
    const first = events.value.length ? events.value[0].seq : 0
    if (first <= 1 || status.value !== 'open') {
      hasEarlier.value = false
      return
    }
    const after = Math.max(0, first - 1 - HISTORY)
    const page = await ask('page', { op: 'page', after, limit: first - 1 - after })
    merge((page.events ?? []) as HiveEvent[], 'prepend')
    hasEarlier.value = after > 0
  }

  /** Ask the agent — only a viewer that may drive gets `ok`. */
  function prompt(text: string): Promise<{ ok: boolean; error?: string }> {
    if (status.value !== 'open') return Promise.resolve({ ok: false, error: 'not connected' })
    const id = newRef()
    return new Promise((resolve) => {
      promptWaiters.set(id, (m) => resolve({ ok: !!m.ok, error: m.error }))
      send({ op: 'prompt', id, text })
    })
  }

  function close(): void {
    teardown(true)
    status.value = 'closed'
    reason.value = null
  }

  watch(
    () => ws.connectionId,
    (now, before) => {
      if (!now || !sessionId) return
      // The socket is up: send the open that had to wait for it.
      if (openPending) {
        sendOpen()
        return
      }
      // The socket redialled: the server ended our grant — or forgot our
      // request — with the old connection, so ask again on the new one.
      if (
        before &&
        now !== before &&
        (status.value === 'open' || status.value === 'connecting' || status.value === 'opening')
      ) {
        open(sessionId)
      }
    },
  )

  onBeforeUnmount(close)

  return { status, reason, events, runState, mayPrompt, live, hasEarlier, open, close, prompt, loadEarlier }
}
