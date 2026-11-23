// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * #3563 — every remote participant in a call is heard, whatever the layout
 * renders.
 *
 * A call of six browsers (fake camera and microphone). The viewer's layout is
 * tiled with its tile cap at 4 (the minimum, the viewer's own tile included) and
 * "Hide participants without video" on; one peer turns their camera off. So the
 * viewer renders three remote tiles of five: one peer is past the cap and one is
 * hidden. Every one of the five must still be heard on the viewer's page.
 *
 * The first check does not depend on how the page plays sound: for every remote
 * audio track the viewer RECEIVES (read from its peer connections), the media
 * elements that play it audibly. Exactly one each. Before #3563 the only player
 * was the tile's `<video>`, so this fails there for the two peers without a
 * rendered tile — the red read on the current deploy. Then the call's own sinks
 * (`audio[data-stream-key]`, one per remote stream key with audio): five, one per
 * peer, each playing, unmuted, and carrying that peer's fake audio (Chrome's fake
 * microphone beeps; an analyser on each sink's stream catches a beep).
 *
 * Runs where the conference specs run: in the app pod's `pwrunner` sidecar
 * against a deployment (`.claude/rules/testing.md`, E2E). Nothing clicks the
 * viewer's page after it joins, so a sink the browser refused to play aloud is
 * still muted when it is read.
 */
import { test, expect, type Browser, type BrowserContext, type Page } from '@playwright/test'
import {
  uniqueUser,
  registerUserViaApi,
  createTenantViaApi,
  createRoomViaApi,
  startCallViaApi,
  addTenantMemberViaApi,
  loginViaUi,
} from './fixtures/test-helpers'
import { recordPeerConnections, trackLocalCaptures } from './fixtures/call-media'

const PEERS = 5
/** The viewer's own tile is one of the four. */
const TILE_CAP = 4
/** Peak absolute sample (-1..1) a beep must exceed. Silence reads 0. */
const SIGNAL_FLOOR = 0.01
/** How long each sink is listened to: the fake microphone beeps about twice a second. */
const LISTEN_MS = 4000

interface User {
  creds: ReturnType<typeof uniqueUser>
  id: string
}

/** One element that plays a remote audio track out loud. */
interface Player {
  tag: string
  streamKey: string | null
  userId: string | null
}

/** A remote audio track the viewer receives, and who plays it. */
interface RemoteVoice {
  trackId: string
  players: Player[]
}

/** One of the call's sinks as the viewer's page holds it. */
interface Sink {
  streamKey: string | null
  userId: string | null
  paused: boolean
  muted: boolean
  volume: number
  audioTracks: number
  peak: number
}

/**
 * Every live audio track the page RECEIVES, and the media elements playing it
 * audibly (not paused, not muted, volume above 0). Read from the peer
 * connections, not from the elements, so a track that no element plays is
 * counted too.
 */
async function remoteVoices(page: Page): Promise<RemoteVoice[]> {
  return await page.evaluate(() => {
    const w = window as unknown as {
      __selfhost_pcs?: RTCPeerConnection[]
      __selfhost_local_track_ids?: string[]
    }
    const local = new Set(w.__selfhost_local_track_ids ?? [])
    const received = new Map<string, MediaStreamTrack>()
    for (const pc of w.__selfhost_pcs ?? []) {
      if (pc.connectionState === 'closed') continue
      for (const t of pc.getTransceivers()) {
        const track = t.receiver.track
        // mediasoup's receiving transceivers are recvonly; a sending one also
        // has a receiver track, which never carries anything.
        if (t.currentDirection !== 'recvonly' && t.currentDirection !== 'sendrecv') continue
        if (track.kind !== 'audio' || track.readyState !== 'live' || local.has(track.id)) continue
        received.set(track.id, track)
      }
    }
    const elements = Array.from(document.querySelectorAll('audio, video')) as HTMLMediaElement[]
    return [...received.keys()].map((trackId) => ({
      trackId,
      players: elements
        .filter((el) => {
          const s = el.srcObject as MediaStream | null
          return !!s && !el.paused && !el.muted && el.volume > 0 && s.getAudioTracks().some((a) => a.id === trackId)
        })
        .map((el) => ({
          tag: el.tagName,
          streamKey: el.getAttribute('data-stream-key'),
          userId: el.getAttribute('data-user-id'),
        })),
    }))
  })
}

/** The call's sinks, each listened to for `ms`: the peak sample it carried. */
async function listenToSinks(page: Page, ms: number): Promise<{ state: string; sinks: Sink[] }> {
  return await page.evaluate(async (listenMs) => {
    const sinks = Array.from(document.querySelectorAll('audio[data-stream-key]')) as HTMLAudioElement[]
    const ctx = new AudioContext()
    // The viewer clicked "Join Meeting", so the page may start audio.
    await ctx.resume().catch(() => {})
    const probes = sinks.map((el) => {
      const s = el.srcObject as MediaStream | null
      const tracks = s ? s.getAudioTracks() : []
      if (tracks.length === 0) return { el, analyser: null as AnalyserNode | null, tracks: 0 }
      // Read the sink's own stream; never connected to the output.
      const analyser = ctx.createAnalyser()
      analyser.fftSize = 2048
      ctx.createMediaStreamSource(new MediaStream(tracks)).connect(analyser)
      return { el, analyser, tracks: tracks.length }
    })
    const peaks = probes.map(() => 0)
    const buf = new Float32Array(2048)
    const end = performance.now() + listenMs
    // A beep lasts tens of milliseconds and the analyser keeps the last ~43 ms,
    // so reading every 10 ms misses none.
    while (performance.now() < end) {
      probes.forEach((p, i) => {
        if (!p.analyser) return
        p.analyser.getFloatTimeDomainData(buf)
        for (const v of buf) if (Math.abs(v) > peaks[i]) peaks[i] = Math.abs(v)
      })
      await new Promise((r) => setTimeout(r, 10))
    }
    const state = ctx.state
    await ctx.close()
    return {
      state,
      sinks: probes.map((p, i) => ({
        streamKey: p.el.getAttribute('data-stream-key'),
        userId: p.el.getAttribute('data-user-id'),
        paused: p.el.paused,
        muted: p.el.muted,
        volume: p.el.volume,
        audioTracks: p.tracks,
        peak: peaks[i],
      })),
    }
  }, ms)
}

/** The remote tiles the viewer's layout renders, by the name on each (a tile
 *  shows the participant's display name, or an id prefix until the page has
 *  read the call's participants). */
async function renderedRemoteTiles(page: Page): Promise<string[]> {
  return await page.evaluate(() =>
    Array.from(document.querySelectorAll('.tiled-layout video[data-stream-key]'))
      .filter((v) => v.getAttribute('data-stream-key') !== 'local')
      .map((v) => v.closest('.v-card')?.querySelector('.v-card-text')?.textContent?.trim() ?? ''),
  )
}

async function joinCall(page: Page, url: string) {
  // Join once the page's socket has its connection id, as a person does
  // (conference-room-switch.spec.ts says why).
  const socketUp = page.waitForEvent('console', {
    predicate: (m) => m.text().startsWith('[WS] received: connected'),
    timeout: 20_000,
  })
  await page.goto(url)
  await socketUp
  const join = page.getByRole('button', { name: 'Join Meeting' })
  await expect(join).toBeVisible({ timeout: 15_000 })
  await join.click()
  await expect(page.getByText('(You)').first()).toBeVisible({ timeout: 20_000 })
}

test.describe('Every remote participant is heard, whatever the layout renders (#3563)', () => {
  let viewer: User
  let peers: User[]
  let tenantId: string
  let roomId: string
  let ownerToken: string

  test.beforeEach(async () => {
    const register = async (): Promise<User> => {
      const creds = uniqueUser()
      const r = await registerUserViaApi(creds)
      if (!ownerToken) ownerToken = r.access_token
      return { creds, id: r.user.id }
    }
    ownerToken = ''
    viewer = await register()
    peers = []
    for (let i = 0; i < PEERS; i++) peers.push(await register())

    const tenant = await createTenantViaApi(ownerToken, 'Every Remote Heard Org', `heard-${Date.now()}`)
    tenantId = tenant.id
    for (const p of peers) await addTenantMemberViaApi(ownerToken, tenantId, p.id)
    roomId = (await createRoomViaApi(ownerToken, tenantId, 'Every Remote Heard')).id
    await startCallViaApi(ownerToken, tenantId, roomId)
  })

  test('six browsers, a tile cap of 4 and one camera-off peer hidden: all five remotes are heard', async ({
    browser,
  }) => {
    test.setTimeout(240_000)
    const callUrl = `/tenant/${tenantId}/room/${roomId}/call`
    const contexts: BrowserContext[] = []
    const open = async (b: Browser) => {
      const ctx = await b.newContext({ permissions: ['camera', 'microphone'] })
      contexts.push(ctx)
      return ctx
    }

    try {
      // The viewer: tiled, the cap at its minimum, and "Hide participants
      // without video" on — set before the app reads its layout preferences.
      const viewerCtx = await open(browser)
      await recordPeerConnections(viewerCtx)
      await trackLocalCaptures(viewerCtx)
      await viewerCtx.addInitScript(
        ({ cap }) => {
          try {
            window.localStorage.setItem(
              'roomler:layout-prefs',
              JSON.stringify({
                mode: 'tiled',
                tiledMaxTiles: cap,
                selfViewMode: 'in-grid-cropped',
                hideNonVideo: true,
                pinnedStreamKeys: [],
              }),
            )
          } catch {
            /* no storage on this document */
          }
        },
        { cap: TILE_CAP },
      )
      // The peers join first. The viewer joins last: the call page reads the
      // participants' names when it joins, and the tiles' names are how the
      // camera-off peer is told apart below. Every sink is still created
      // after the viewer's click, as each consumer arrives.
      const peerPages: Page[] = []
      for (const p of peers) {
        const page = await (await open(browser)).newPage()
        await loginViaUi(page, p.creds.username, p.creds.password)
        await joinCall(page, callUrl)
        peerPages.push(page)
      }
      const viewerPage = await viewerCtx.newPage()
      await loginViaUi(viewerPage, viewer.creds.username, viewer.creds.password)
      await joinCall(viewerPage, callUrl)

      // The viewer receives every peer's microphone.
      await expect
        .poll(async () => (await remoteVoices(viewerPage)).length, {
          message: 'remote audio tracks the viewer receives',
          timeout: 60_000,
        })
        .toBe(PEERS)

      // One peer turns their camera off: the viewer hides them.
      const hidden = peers[PEERS - 1]
      const hiddenPage = peerPages[PEERS - 1]
      await hiddenPage.locator('button:has(.mdi-video)').first().click()
      await expect(hiddenPage.locator('.mdi-video-off').first()).toBeVisible({ timeout: 5_000 })
      const peerNames = peers.map((p) => p.creds.displayName)
      await expect
        .poll(
          async () => {
            const tiles = await renderedRemoteTiles(viewerPage)
            return {
              tiles: tiles.length,
              // Names, not id prefixes: ids made in one second share theirs.
              named: tiles.every((name) => peerNames.includes(name)),
              showsHidden: tiles.includes(hidden.creds.displayName),
            }
          },
          { message: 'the viewer renders three named remote tiles, none of them the camera-off peer', timeout: 30_000 },
        )
        .toEqual({ tiles: TILE_CAP - 1, named: true, showsHidden: false })

      // Every voice the viewer receives is played aloud by exactly one element.
      const voices = await remoteVoices(viewerPage)
      expect(voices, 'remote audio tracks the viewer receives').toHaveLength(PEERS)
      const unheard = voices.filter((v) => v.players.length === 0)
      expect(
        unheard,
        `${unheard.length} of ${PEERS} remote voices are played by no element on the viewer's page`,
      ).toEqual([])
      const twice = voices.filter((v) => v.players.length > 1)
      expect(twice, 'a remote voice played by more than one element').toEqual([])
      for (const v of voices) {
        expect(v.players[0].tag, 'the one player of a remote voice is a call sink, not a tile').toBe('AUDIO')
        expect(v.players[0].streamKey, 'the sink names its stream key').not.toBeNull()
      }

      // The call's sinks: one per peer, each playing, unmuted, with signal.
      const heard = await listenToSinks(viewerPage, LISTEN_MS)
      expect(heard.state, "the viewer's AudioContext runs").toBe('running')
      expect(
        heard.sinks.map((s) => s.userId).sort(),
        'one sink per remote peer, by user id',
      ).toEqual(peers.map((p) => p.id).sort())
      for (const s of heard.sinks) {
        const who = `the sink of ${s.userId} (${s.streamKey})`
        expect(s.paused, `${who} is playing`).toBe(false)
        expect(s.muted, `${who} is unmuted`).toBe(false)
        expect(s.volume, `${who} has volume`).toBeGreaterThan(0)
        expect(s.audioTracks, `${who} holds that peer's microphone`).toBe(1)
        expect(s.peak, `${who} carries the peer's fake audio`).toBeGreaterThan(SIGNAL_FLOOR)
      }
    } finally {
      for (const ctx of contexts) await ctx.close().catch(() => {})
    }
  })
})
