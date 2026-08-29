import { describe, it, expect, beforeEach, vi } from 'vitest'
import { ref, effectScope, nextTick } from 'vue'
import { reactive } from 'vue'
import {
  useConferenceLayout,
  resolveEffectiveMode,
  pickPrimaryFallback,
  type LayoutParticipant,
} from '@/composables/useConferenceLayout'

/**
 * FR-25. The layout controls were reported as "not working properly"; three
 * separate defects lived in this composable and none of them had a test.
 * These lock the rules that were previously buried inside one computed.
 */

// A MediaStream stand-in: jsdom has none, and the whole point of the
// hide-non-video fix is that track state changes must be observable.
class FakeTrack extends EventTarget {
  kind = 'video'
  enabled = true
  readyState: 'live' | 'ended' = 'live'
  end() {
    this.readyState = 'ended'
    this.dispatchEvent(new Event('ended'))
  }
}
class FakeStream extends EventTarget {
  tracks: FakeTrack[]
  constructor(withVideo = true) {
    super()
    this.tracks = withVideo ? [new FakeTrack()] : []
  }
  getVideoTracks() {
    return this.tracks
  }
  addVideo() {
    const t = new FakeTrack()
    this.tracks.push(t)
    this.dispatchEvent(new Event('addtrack'))
    return t
  }
}
const asStream = (s: FakeStream) => s as unknown as MediaStream

function participant(over: Partial<LayoutParticipant> = {}): LayoutParticipant {
  return {
    streamKey: 'k',
    userId: 'u',
    displayName: 'Someone',
    stream: null,
    isMuted: false,
    isLocal: false,
    isScreenShare: false,
    isPinned: false,
    videoPaused: false,
    audioLevel: 0,
    ...over,
  }
}

beforeEach(() => {
  localStorage.clear()
})

describe('resolveEffectiveMode', () => {
  const base = { hasScreenShare: false, pinnedCount: 0, participantCount: 5 }

  it('passes an explicit mode straight through — the picker is not a suggestion', () => {
    for (const m of ['tiled', 'spotlight', 'sidebar'] as const) {
      expect(resolveEffectiveMode(m, { ...base, hasScreenShare: true })).toBe(m)
    }
  })

  it('auto ranks screen share over a pin, a pin over headcount', () => {
    expect(resolveEffectiveMode('auto', { ...base, hasScreenShare: true, pinnedCount: 2 })).toBe(
      'sidebar',
    )
    expect(resolveEffectiveMode('auto', { ...base, pinnedCount: 1 })).toBe('spotlight')
    expect(resolveEffectiveMode('auto', { ...base, participantCount: 2 })).toBe('spotlight')
    expect(resolveEffectiveMode('auto', { ...base, participantCount: 3 })).toBe('tiled')
  })
})

describe('pickPrimaryFallback', () => {
  const me = participant({ streamKey: 'local', isLocal: true, displayName: 'Aaron (me)' })
  const remote = participant({ streamKey: 'r1', displayName: 'Zoe' })

  it('never spotlights YOU while a remote exists — even when you sort first', () => {
    // The old code took sorted[0], which is alphabetical: an operator whose
    // name sorted first got a full-screen view of themselves.
    expect(pickPrimaryFallback([me, remote], null)).toEqual([remote])
  })

  it('prefers the active speaker, but only a remote one', () => {
    const other = participant({ streamKey: 'r2', displayName: 'Bo' })
    expect(pickPrimaryFallback([me, remote, other], 'r2')).toEqual([other])
    // Local speaker → fall through to the first remote rather than self.
    expect(pickPrimaryFallback([me, remote], 'local')).toEqual([remote])
  })

  it('falls back to self only when alone', () => {
    expect(pickPrimaryFallback([me], null)).toEqual([me])
    expect(pickPrimaryFallback([], null)).toEqual([])
  })
})

describe('useConferenceLayout', () => {
  function mount(opts: {
    local?: FakeStream | null
    remotes?: Array<[string, FakeStream]>
    paused?: Map<string, boolean>
  } = {}) {
    const scope = effectScope()
    let api!: ReturnType<typeof useConferenceLayout>
    const localStream = ref<MediaStream | null>(opts.local ? asStream(opts.local) : null)
    const remoteStreams = reactive(new Map())
    for (const [key, s] of opts.remotes ?? []) {
      remoteStreams.set(key, {
        userId: key,
        connectionId: key,
        stream: asStream(s),
        kind: 'video',
        source: key.endsWith(':screen') ? 'screen' : 'webcam',
      })
    }
    scope.run(() => {
      api = useConferenceLayout(
        localStream,
        remoteStreams as Map<string, never>,
        ref(false),
        ref(new Map()),
        ref(null),
        (id: string) => id,
        ref('Me'),
        opts.paused,
      )
    })
    return { api, scope, remoteStreams, paused: opts.paused }
  }

  it('hide-non-video converges when a camera turns on LATER (the stale-filter bug)', async () => {
    const cam = new FakeStream(false) // joined with the camera off
    const { api } = mount({ local: new FakeStream(), remotes: [['r1', cam]] })
    api.setHideNonVideo(true)
    api.setMode('tiled')
    await nextTick()
    expect(api.layout.value.primary.map((p) => p.streamKey)).not.toContain('r1')

    // Native track state is invisible to Vue: before FR-25 this filter was
    // decided once and the tile never came back.
    cam.addVideo()
    await nextTick()
    expect(api.layout.value.primary.map((p) => p.streamKey)).toContain('r1')
  })

  it('drops a participant again when their only track ends', async () => {
    const cam = new FakeStream(true)
    const { api } = mount({ local: new FakeStream(), remotes: [['r1', cam]] })
    api.setHideNonVideo(true)
    api.setMode('tiled')
    await nextTick()
    expect(api.layout.value.primary.map((p) => p.streamKey)).toContain('r1')

    cam.tracks[0].end()
    await nextTick()
    expect(api.layout.value.primary.map((p) => p.streamKey)).not.toContain('r1')
  })

  it('sidebar without a screen share spotlights the remote, not you', async () => {
    const { api } = mount({ local: new FakeStream(), remotes: [['r1', new FakeStream()]] })
    api.setMode('sidebar')
    await nextTick()
    expect(api.layout.value.primary.map((p) => p.streamKey)).toEqual(['r1'])
  })

  it('auto lands on sidebar for a screen share and puts it in primary', async () => {
    const { api } = mount({
      local: new FakeStream(),
      remotes: [
        ['r1', new FakeStream()],
        ['r1:screen', new FakeStream()],
      ],
    })
    await nextTick()
    expect(api.layout.value.effectiveMode).toBe('sidebar')
    expect(api.layout.value.primary.map((p) => p.streamKey)).toEqual(['r1:screen'])
  })

  it('toggleSpotlight pins one tile, then restores the previous mode', async () => {
    const { api } = mount({ local: new FakeStream(), remotes: [['r1', new FakeStream()]] })
    api.setMode('tiled')

    expect(api.toggleSpotlight('r1')).toBe(true)
    expect(api.prefs.value.mode).toBe('spotlight')
    expect(api.prefs.value.pinnedStreamKeys).toEqual(['r1'])

    // A second double-click really undoes the first — it does not strand
    // the operator in spotlight.
    expect(api.toggleSpotlight('r1')).toBe(false)
    expect(api.prefs.value.mode).toBe('tiled')
    expect(api.prefs.value.pinnedStreamKeys).toEqual([])
  })

  it('spotlighting a different tile replaces the pin instead of stacking', () => {
    const { api } = mount({ local: new FakeStream(), remotes: [['r1', new FakeStream()]] })
    api.toggleSpotlight('r1')
    api.toggleSpotlight('local')
    expect(api.prefs.value.pinnedStreamKeys).toEqual(['local'])
    expect(api.prefs.value.mode).toBe('spotlight')
  })

  it('floating self-view takes the local tile out of the grid', async () => {
    const { api } = mount({ local: new FakeStream(), remotes: [['r1', new FakeStream()]] })
    api.setMode('tiled')
    api.setSelfViewMode('floating-uncropped')
    await nextTick()
    expect(api.layout.value.selfViewFloating).toBe(true)
    expect(api.layout.value.primary.map((p) => p.streamKey)).not.toContain('local')
    expect(api.selfParticipant.value?.streamKey).toBe('local')
  })

  it('stops listening to track events when the scope is disposed', async () => {
    const cam = new FakeStream(true)
    const spy = vi.spyOn(cam.tracks[0], 'removeEventListener')
    const { api, scope } = mount({ remotes: [['r1', cam]] })
    void api.layout.value // realise the participants computed
    scope.stop()
    expect(spy).toHaveBeenCalled()
  })

  // FR-30 (#884) — the peer's OWN signal is the only thing that can answer
  // "is their camera off": a paused sender's track stays `live` and unmuted on
  // this side (measured on prod 2026-08-29), so no amount of track inspection
  // substitutes for it. FR-25 made the filter re-run; it could not make it
  // right.
  it('hides a participant whose camera the PEER says is off, even with a live track', () => {
    const remote = new FakeStream(true)
    const paused = reactive(new Map<string, boolean>())
    const { api } = mount({ local: new FakeStream(true), remotes: [['them', remote]], paused })
    api.prefs.value.hideNonVideo = true

    // Live track, nothing said yet: shown.
    expect(api.participants.value.map((p) => p.streamKey)).toContain('them')

    paused.set('them', true)
    expect([...api.layout.value.primary, ...api.layout.value.secondary].map((p) => p.streamKey)).not.toContain('them')

    paused.set('them', false)
    expect([...api.layout.value.primary, ...api.layout.value.secondary].map((p) => p.streamKey)).toContain('them')
  })

  it('never hides YOU on your own camera-off — you can already see that', () => {
    const paused = reactive(new Map<string, boolean>())
    const { api } = mount({ local: new FakeStream(true), remotes: [], paused })
    api.prefs.value.hideNonVideo = true
    paused.set('local', true)
    expect([...api.layout.value.primary, ...api.layout.value.secondary].map((p) => p.streamKey)).toContain('local')
  })
})
