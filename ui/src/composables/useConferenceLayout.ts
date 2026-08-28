// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
import { ref, computed, watch, type Ref } from 'vue'

export type LayoutMode = 'auto' | 'tiled' | 'spotlight' | 'sidebar'
export type SelfViewMode = 'in-grid-cropped' | 'in-grid-uncropped' | 'floating-uncropped'

export interface LayoutParticipant {
  streamKey: string
  userId: string
  displayName: string
  stream: MediaStream | null
  isMuted: boolean
  isLocal: boolean
  isScreenShare: boolean
  isPinned: boolean
  audioLevel: number
}

export interface LayoutPreferences {
  mode: LayoutMode
  tiledMaxTiles: number
  selfViewMode: SelfViewMode
  hideNonVideo: boolean
  pinnedStreamKeys: string[]
}

export interface ResolvedLayout {
  effectiveMode: 'tiled' | 'spotlight' | 'sidebar'
  primary: LayoutParticipant[]
  secondary: LayoutParticipant[]
  selfViewFloating: boolean
}

const STORAGE_KEY = 'roomler:layout-prefs'
const MAX_PINS = 6

function loadPrefs(): LayoutPreferences {
  try {
    const stored = localStorage.getItem(STORAGE_KEY)
    if (stored) {
      const parsed = JSON.parse(stored)
      return {
        mode: parsed.mode || 'auto',
        tiledMaxTiles: parsed.tiledMaxTiles ?? 16,
        selfViewMode: parsed.selfViewMode || 'in-grid-cropped',
        hideNonVideo: parsed.hideNonVideo ?? false,
        pinnedStreamKeys: parsed.pinnedStreamKeys || [],
      }
    }
  } catch {}
  return {
    mode: 'auto',
    tiledMaxTiles: 16,
    selfViewMode: 'in-grid-cropped',
    hideNonVideo: false,
    pinnedStreamKeys: [],
  }
}

function savePrefs(prefs: LayoutPreferences) {
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify(prefs))
  } catch {}
}

interface RemoteStream {
  userId: string
  connectionId: string
  stream: MediaStream
  kind: string
  source: string
}

export function useConferenceLayout(
  localStream: Ref<MediaStream | null>,
  remoteStreams: Map<string, RemoteStream>,
  isMuted: Ref<boolean>,
  audioLevels: Ref<Map<string, number>>,
  activeSpeakerKey: Ref<string | null>,
  getDisplayName: (userId: string) => string,
  localDisplayName: Ref<string>,
) {
  const prefs = ref<LayoutPreferences>(loadPrefs())

  // Persist on change
  watch(prefs, (v) => savePrefs(v), { deep: true })

  function hasLiveVideoTrack(stream: MediaStream | null): boolean {
    if (!stream) return false
    return stream.getVideoTracks().some((t) => t.enabled && t.readyState === 'live')
  }

  const participants = computed<LayoutParticipant[]>(() => {
    const list: LayoutParticipant[] = []

    // Local participant
    if (localStream.value) {
      list.push({
        streamKey: 'local',
        userId: 'local',
        displayName: localDisplayName.value,
        stream: localStream.value,
        isMuted: isMuted.value,
        isLocal: true,
        isScreenShare: false,
        isPinned: prefs.value.pinnedStreamKeys.includes('local'),
        audioLevel: 0,
      })
    }

    // Remote participants
    for (const [streamKey, remote] of remoteStreams) {
      const isScreen = streamKey.endsWith(':screen') || remote.source === 'screen'
      const name = isScreen
        ? getDisplayName(remote.userId) + ' (Screen)'
        : getDisplayName(remote.userId)
      list.push({
        streamKey,
        userId: remote.userId,
        displayName: name,
        stream: remote.stream,
        isMuted: false,
        isLocal: false,
        isScreenShare: isScreen,
        isPinned: prefs.value.pinnedStreamKeys.includes(streamKey),
        audioLevel: audioLevels.value.get(streamKey) ?? 0,
      })
    }

    return list
  })

  const hasScreenShare = computed(() =>
    participants.value.some((p) => p.isScreenShare),
  )

  const pinnedParticipants = computed(() =>
    participants.value.filter((p) => p.isPinned),
  )

  function filterHidden(list: LayoutParticipant[]): LayoutParticipant[] {
    if (!prefs.value.hideNonVideo) return list
    return list.filter(
      (p) => p.isPinned || p.isLocal || hasLiveVideoTrack(p.stream),
    )
  }

  function sortParticipants(list: LayoutParticipant[]): LayoutParticipant[] {
    return [...list].sort((a, b) => {
      // Pinned first
      if (a.isPinned !== b.isPinned) return a.isPinned ? -1 : 1
      // Active speaker next
      const aActive = a.streamKey === activeSpeakerKey.value
      const bActive = b.streamKey === activeSpeakerKey.value
      if (aActive !== bActive) return aActive ? -1 : 1
      // Alphabetical
      return a.displayName.localeCompare(b.displayName)
    })
  }

  const layout = computed<ResolvedLayout>(() => {
    const mode = prefs.value.mode
    const all = filterHidden(participants.value)
    const sorted = sortParticipants(all)
    const selfFloating = prefs.value.selfViewMode === 'floating-uncropped'

    // Determine effective mode
    let effectiveMode: 'tiled' | 'spotlight' | 'sidebar'
    if (mode === 'auto') {
      if (hasScreenShare.value) {
        effectiveMode = 'sidebar'
      } else if (pinnedParticipants.value.length > 0) {
        effectiveMode = 'spotlight'
      } else if (sorted.length <= 2) {
        effectiveMode = 'spotlight'
      } else {
        effectiveMode = 'tiled'
      }
    } else {
      effectiveMode = mode
    }

    // Split into primary/secondary based on effective mode
    let primary: LayoutParticipant[] = []
    let secondary: LayoutParticipant[] = []

    if (effectiveMode === 'tiled') {
      // In tiled mode, everyone in primary (up to max tiles)
      const gridParticipants = selfFloating
        ? sorted.filter((p) => !p.isLocal)
        : sorted
      primary = gridParticipants.slice(0, prefs.value.tiledMaxTiles)
      secondary = [] // no filmstrip in tiled
    } else if (effectiveMode === 'spotlight') {
      // Pinned or active speaker in primary, rest in secondary
      const pinned = sorted.filter((p) => p.isPinned)
      if (pinned.length > 0) {
        primary = pinned
      } else if (activeSpeakerKey.value) {
        const speaker = sorted.find((p) => p.streamKey === activeSpeakerKey.value)
        primary = speaker ? [speaker] : sorted.slice(0, 1)
      } else {
        // No pinned, no active speaker — first non-local participant
        const nonLocal = sorted.filter((p) => !p.isLocal)
        primary = nonLocal.length > 0 ? [nonLocal[0]] : sorted.slice(0, 1)
      }
      const primaryKeys = new Set(primary.map((p) => p.streamKey))
      secondary = sorted.filter((p) => !primaryKeys.has(p.streamKey))
      if (selfFloating) {
        secondary = secondary.filter((p) => !p.isLocal)
      }
    } else {
      // sidebar: screen share or pinned in primary, rest in sidebar
      const screenShares = sorted.filter((p) => p.isScreenShare)
      if (screenShares.length > 0) {
        primary = screenShares
      } else {
        const pinned = sorted.filter((p) => p.isPinned)
        primary = pinned.length > 0 ? pinned : sorted.slice(0, 1)
      }
      const primaryKeys = new Set(primary.map((p) => p.streamKey))
      secondary = sorted.filter((p) => !primaryKeys.has(p.streamKey))
      if (selfFloating) {
        secondary = secondary.filter((p) => !p.isLocal)
      }
    }

    return {
      effectiveMode,
      primary,
      secondary,
      selfViewFloating: selfFloating,
    }
  })

  // Self-view participant for floating overlay
  const selfParticipant = computed(() =>
    participants.value.find((p) => p.isLocal) ?? null,
  )

  function togglePin(streamKey: string): boolean {
    const idx = prefs.value.pinnedStreamKeys.indexOf(streamKey)
    if (idx >= 0) {
      prefs.value.pinnedStreamKeys.splice(idx, 1)
      return true
    }
    if (prefs.value.pinnedStreamKeys.length >= MAX_PINS) {
      return false
    }
    prefs.value.pinnedStreamKeys.push(streamKey)
    return true
  }

  function setMode(mode: LayoutMode) {
    prefs.value.mode = mode
  }

  function setMaxTiles(n: number) {
    prefs.value.tiledMaxTiles = Math.max(4, Math.min(49, n))
  }

  function setSelfViewMode(mode: SelfViewMode) {
    prefs.value.selfViewMode = mode
  }

  function setHideNonVideo(v: boolean) {
    prefs.value.hideNonVideo = v
  }

  return {
    prefs,
    participants,
    layout,
    selfParticipant,
    togglePin,
    setMode,
    setMaxTiles,
    setSelfViewMode,
    setHideNonVideo,
  }
}
