// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * FR-90 P2i-3 — the browser's half of a search: the words of the agent
 * sessions you may read, searched on the archive replica that holds their
 * copies, over a data-only peer of its own.
 *
 * `hive:search.open {tenant_id, ref}` asks the server (P2i-2). It names the
 * sessions you may read whose copy the archive replica holds, gets the DEVICE
 * to confirm a grant naming them, and only then says `ready` with the ICE
 * servers minted for this peer. The peer then takes a viewer's frames by its
 * grant: our offer, ICE both ways, renewals, the close. Over its one channel
 * (`hive`, framed as a viewer's) we ask `hello` and `search {id, q, limit}`,
 * and the device answers `{op: search, id, ok, hits: [{session, seq,
 * snippet}]}` (P2i-1). The query and every hit travel between this tab and
 * the device alone: the server never sees either.
 *
 * Opened at the first search, and closed after `IDLE_CLOSE_MS` without one:
 * a device serves 8 searches at once, and a page left open must not hold a
 * seat for nothing. The next search opens it again.
 *
 * ⚠️ The peer is CLOSED on every way out, as a viewer's: unmount, close(),
 * idle, a grant the server or the device ends, a socket that redials. A
 * WebRTC peer that is merely dropped frees nothing.
 */
import { onBeforeUnmount, ref, shallowRef, watch } from 'vue'
import { useWsStore } from '@/stores/ws'
import { Reassembler, decodeJson, encodeJson, type FrameError } from '@/utils/hiveFraming'
import {
  newRef,
  offGrant,
  offRef,
  onGrant,
  onRef,
  routeHiveFrames,
  type HiveFrame as Frame,
} from '@/utils/hiveRouter'

/** One place a search found: the event `seq` of `session`, and the words
 *  around the match, marked `[` `]` (P2i-1). */
export interface SearchHit {
  session: string
  seq: number
  snippet: string
}

export type SearchStatus = 'idle' | 'opening' | 'connecting' | 'open' | 'closed' | 'refused'

/** The longest query the device takes, in characters
 *  (`search_limits::MAX_QUERY_CHARS`). */
export const MAX_QUERY_CHARS = 200
/** The hits one search asks for (`search_limits::DEFAULT_HITS`). */
const HITS = 20
/** `hello` is asked again this often, this many times, until it is answered. */
const HELLO_RETRY_MS = 1500
const HELLO_ATTEMPTS = 4
/** A search the device has not answered within this is given up on. */
const ANSWER_TIMEOUT_MS = 15_000
/** From the open until the device's hello: the server waits 10 s for the
 *  device's answer (`view_limits::GRANT_ACK_TIMEOUT_SECS`), then the peer
 *  must connect. */
const OPEN_TIMEOUT_MS = 30_000
/** The device takes one search every 250 ms a grant
 *  (`search_limits::MIN_INTERVAL_MS`): one refused `too_soon` is asked
 *  again once, this much later. */
const TOO_SOON_WAIT_MS = 300
/** An open peer with no search for this long is closed. */
const IDLE_CLOSE_MS = 120_000
/** After a refusal the server is not asked again for this long: a box typed
 *  into would otherwise spend a minute's 20 opens in seconds. */
const REFUSED_BACKOFF_MS = 10_000
/** What we accept from the device: at most 50 hits, each a short snippet. */
const INBOUND = { maxMessageBytes: 1024 * 1024, maxInFlight: 4 }

export interface SnippetPart {
  text: string
  mark: boolean
}

/**
 * A hit's snippet as plain and marked text, for `<mark>` spans drawn from
 * text, never from HTML. The device marks a match `[` `]` (FTS5's
 * `snippet()`); a bracket in the session's own words is the same character,
 * so `[INFO]` in a log reads as a match. That is cosmetic: the hit is still
 * the place the words were found.
 */
export function snippetParts(snippet: string): SnippetPart[] {
  const parts: SnippetPart[] = []
  let at = 0
  for (;;) {
    const open = snippet.indexOf('[', at)
    if (open < 0) break
    const close = snippet.indexOf(']', open + 1)
    if (close < 0) break
    if (open > at) parts.push({ text: snippet.slice(at, open), mark: false })
    if (close > open + 1) parts.push({ text: snippet.slice(open + 1, close), mark: true })
    at = close + 1
  }
  if (at < snippet.length) parts.push({ text: snippet.slice(at), mark: false })
  return parts
}

function asHit(h: unknown): SearchHit | null {
  if (!h || typeof h !== 'object') return null
  const { session, seq, snippet } = h as Record<string, unknown>
  if (typeof session !== 'string' || typeof snippet !== 'string') return null
  return { session, seq: Number(seq ?? 0), snippet }
}

export function useHiveSearch() {
  const ws = useWsStore()

  const status = ref<SearchStatus>('idle')
  /** Why the server or the device refused, or why the peer closed: a word. */
  const reason = ref<string | null>(null)
  /** The refusal's detail, in the words it came with. */
  const detail = ref<string | null>(null)
  /** How many sessions the grant names (the server's count), and how many of
   *  them the device holds and searches (its hello). */
  const named = ref<number | null>(null)
  const searched = ref<number | null>(null)
  const hits = shallowRef<SearchHit[]>([])
  /** The query `hits` answers: none found is said only for the query asked. */
  const answered = ref<string | null>(null)
  const searching = ref(false)
  /** Why the last search found nothing to show: the device's words. */
  const error = ref<string | null>(null)

  let tenant: string | null = null
  let reference: string | null = null
  let grantId: string | null = null
  let pc: RTCPeerConnection | null = null
  let dc: RTCDataChannel | null = null
  let renewTimer: ReturnType<typeof setInterval> | null = null
  let idleTimer: ReturnType<typeof setTimeout> | null = null
  let openTimer: ReturnType<typeof setTimeout> | null = null
  let gen = 0
  let nextId = 0
  let latest = 0
  let refusedAt = 0
  let reasm = new Reassembler(INBOUND)
  const pendingRemoteIce: RTCIceCandidateInit[] = []
  let remoteSet = false
  let offerSent = false
  const pendingLocalIce: RTCIceCandidateInit[] = []
  let helloWaiter: ((m: Frame) => void) | null = null
  const searchWaiters = new Map<string, (m: Frame) => void>()
  // Whoever waits for the peer: true once the device answered `hello`,
  // false on any way out.
  let readyWaiters: Array<(ok: boolean) => void> = []
  // An open asked for before the socket was up, sent once it is.
  let openPending = false

  function send(message: unknown): void {
    if (!dc || dc.readyState !== 'open') return
    for (const frame of encodeJson(nextId++, message)) dc.send(frame)
  }

  function askHelloOnce(): Promise<Frame> {
    return new Promise((resolve) => {
      helloWaiter = resolve
      send({ op: 'hello' })
    })
  }

  /** `hello`, asked again until the device answers, as a viewer's. */
  async function askHello(myGen: number): Promise<Frame | null> {
    for (let attempt = 0; attempt < HELLO_ATTEMPTS; attempt++) {
      const answer = await Promise.race([
        askHelloOnce(),
        new Promise<null>((resolve) => setTimeout(() => resolve(null), HELLO_RETRY_MS)),
      ])
      if (myGen !== gen) return null
      if (answer && answer.op === 'hello') return answer
    }
    return null
  }

  function askSearch(q: string): Promise<Frame> {
    const id = newRef()
    return new Promise((resolve) => {
      const timer = setTimeout(() => {
        searchWaiters.delete(id)
        resolve({ type: 'search', op: 'search', id, ok: false, error: 'no_answer' })
      }, ANSWER_TIMEOUT_MS)
      searchWaiters.set(id, (m) => {
        clearTimeout(timer)
        resolve(m)
      })
      send({ op: 'search', id, q, limit: HITS })
    })
  }

  function armIdle(): void {
    if (idleTimer) clearTimeout(idleTimer)
    idleTimer = setTimeout(() => {
      idleTimer = null
      // Nothing asked for a while: give the device its seat back.
      teardown(true)
      status.value = 'idle'
      reason.value = null
    }, IDLE_CLOSE_MS)
  }

  function teardown(sayClose: boolean): void {
    gen++
    if (renewTimer) clearInterval(renewTimer)
    renewTimer = null
    if (idleTimer) clearTimeout(idleTimer)
    idleTimer = null
    if (openTimer) clearTimeout(openTimer)
    openTimer = null
    if (reference) offRef(reference)
    if (grantId) {
      offGrant(grantId)
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
    // Resolve what is waiting, so no async step hangs on a peer that is gone.
    const hello = helloWaiter
    helloWaiter = null
    hello?.({ type: 'closed' })
    const searches = [...searchWaiters.values()]
    searchWaiters.clear()
    for (const resolve of searches) resolve({ type: 'search', op: 'search', ok: false, error: 'closed' })
    const ready = readyWaiters
    readyWaiters = []
    for (const resolve of ready) resolve(false)
  }

  function finish(to: SearchStatus, why: string | null, sayClose = true): void {
    teardown(sayClose)
    status.value = to
    reason.value = why
  }

  function onDeviceMessage(m: Frame): void {
    switch (m.op) {
      case 'hello':
        helloWaiter?.(m)
        helloWaiter = null
        break
      case 'search': {
        const resolve = typeof m.id === 'string' ? searchWaiters.get(m.id) : undefined
        if (typeof m.id === 'string') searchWaiters.delete(m.id)
        resolve?.(m)
        break
      }
      case 'error':
        console.warn('[hive] the archive replica answered an error:', m.error)
        break
      default:
        break
    }
  }

  async function onChannelOpen(myGen: number): Promise<void> {
    const hello = await askHello(myGen)
    if (myGen !== gen) return
    // A search grant's hello says so; anything else is no search peer.
    if (!hello || hello.search !== true) {
      finish('closed', hello ? 'not_a_search' : 'no_hello')
      return
    }
    searched.value = Number(hello.sessions ?? 0)
    status.value = 'open'
    if (openTimer) clearTimeout(openTimer)
    openTimer = null
    armIdle()
    const ready = readyWaiters
    readyWaiters = []
    for (const resolve of ready) resolve(true)
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
        // Its end, whatever the word: expired, removed from a room it named,
        // the device gone. The next search asks for a fresh grant, which
        // names what is true then.
        finish('closed', f.reason ?? 'closed', false)
        break
      case 'hive:view.renewed':
        break
    }
  }

  async function dial(ready: Frame, myGen: number): Promise<void> {
    grantId = ready.grant_id
    onGrant(ready.grant_id, (f) => void onFrame(f, myGen))
    named.value = Number(ready.sessions ?? 0)
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
      if (myGen === gen && (status.value === 'open' || status.value === 'connecting')) {
        finish('closed', 'device_left')
      }
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

  /** Ask the server for a search grant in `tenantId`. Replaces any open peer. */
  function open(tenantId: string): void {
    teardown(true)
    tenant = tenantId
    const myGen = gen
    reasm = new Reassembler(INBOUND)
    named.value = null
    searched.value = null
    reason.value = null
    detail.value = null
    status.value = 'opening'
    // Neither a ready nor a refusal, or a peer that never connects: a search
    // box must not spin for ever.
    openTimer = setTimeout(() => {
      if (myGen === gen) finish('closed', 'no_answer')
    }, OPEN_TIMEOUT_MS)
    routeHiveFrames(ws)
    reference = newRef()
    onRef(reference, (f) => {
      if (myGen !== gen) return
      if (reference) offRef(reference)
      if (f.type === 'hive:search.refused') {
        refusedAt = Date.now()
        finish('refused', f.reason ?? 'refused', false)
        detail.value = typeof f.message === 'string' ? f.message : null
        return
      }
      // A ref is ours alone; anything but our own answer is no grant.
      if (f.type !== 'hive:search.ready') return
      void dial(f, myGen).catch((e) => finish('closed', (e as Error).message))
    })
    // The store drops a frame it cannot send: ask once the socket is up.
    if (ws.connectionId) sendOpen()
    else openPending = true
  }

  function sendOpen(): void {
    if (!tenant || !reference) return
    openPending = false
    ws.send('hive:search.open', { tenant_id: tenant, ref: reference })
  }

  /** A peer that answers searches in `tenantId`: true once the device said
   *  hello, false when there is none (refused, or closed on the way). */
  function ready(tenantId: string): Promise<boolean> {
    if (tenant === tenantId && status.value === 'open') return Promise.resolve(true)
    const asking = tenant === tenantId && (status.value === 'opening' || status.value === 'connecting')
    if (!asking) {
      // Refused a moment ago: the same answer, without asking again.
      if (tenant === tenantId && status.value === 'refused' && Date.now() - refusedAt < REFUSED_BACKOFF_MS) {
        return Promise.resolve(false)
      }
      open(tenantId)
    }
    return new Promise((resolve) => readyWaiters.push(resolve))
  }

  async function attempt(tenantId: string, q: string): Promise<Frame> {
    if (!(await ready(tenantId))) {
      return { type: 'search', ok: false, error: status.value === 'refused' ? 'refused' : 'closed' }
    }
    armIdle()
    let answer = await askSearch(q)
    if (!answer.ok && typeof answer.error === 'string' && answer.error.startsWith('too_soon')) {
      await new Promise((resolve) => setTimeout(resolve, TOO_SOON_WAIT_MS))
      if (status.value !== 'open' || tenant !== tenantId) return { type: 'search', ok: false, error: 'closed' }
      answer = await askSearch(q)
    }
    return answer
  }

  /**
   * Search `tenantId`'s sessions you may read for `q`. Only the latest call's
   * answer lands in `hits`: an earlier one still on its way is dropped. An
   * empty `q` clears the hits and asks nothing.
   */
  async function run(tenantId: string, q: string): Promise<void> {
    const mine = ++latest
    const text = q.trim()
    if (!text) {
      hits.value = []
      answered.value = null
      error.value = null
      searching.value = false
      return
    }
    searching.value = true
    error.value = null
    let answer = await attempt(tenantId, text)
    // The peer went on the way (its grant ended, the socket redialled): once
    // more, on a fresh grant.
    if (mine === latest && answer.error === 'closed') answer = await attempt(tenantId, text)
    if (mine !== latest) return
    searching.value = false
    if (answer.ok) {
      hits.value = (Array.isArray(answer.hits) ? answer.hits : []).map(asHit).filter((h): h is SearchHit => h !== null)
      answered.value = text
      return
    }
    hits.value = []
    answered.value = null
    // A refusal is shown as one (`status`, `reason`); anything else in the
    // words it came with.
    if (answer.error !== 'refused') {
      error.value = answer.error === 'closed' ? (reason.value ?? 'closed') : String(answer.error ?? 'failed')
    }
  }

  function close(): void {
    latest++
    teardown(true)
    status.value = 'idle'
    reason.value = null
    detail.value = null
    searching.value = false
    hits.value = []
    answered.value = null
    error.value = null
  }

  watch(
    () => ws.connectionId,
    (now, before) => {
      if (!tenant) return
      const live = status.value === 'open' || status.value === 'connecting' || status.value === 'opening'
      // The socket went, or came back as another connection: the server ends
      // a grant with the connection that holds it, and forgets an open asked
      // on it. A redial goes through null (the store clears the id at its
      // close, then dials again), so the drop itself is the cue. A search on
      // its way asks again, on the new connection; an open never sent waits.
      if (live && !openPending && before && now !== before) {
        finish('closed', 'redialled', false)
        return
      }
      // The socket is up: send the open that had to wait for it.
      if (now && openPending) sendOpen()
    },
  )

  onBeforeUnmount(close)

  return { status, reason, detail, named, searched, hits, answered, searching, error, run, close }
}
