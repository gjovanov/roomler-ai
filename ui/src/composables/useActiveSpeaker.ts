// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
import { ref, watch, type Ref } from 'vue'

interface RemoteStream {
  userId: string
  connectionId: string
  stream: MediaStream
  kind: string
  source: string
}

interface AnalyserEntry {
  analyser: AnalyserNode
  source: MediaStreamAudioSourceNode
  dataArray: Uint8Array<ArrayBuffer>
}

export function useActiveSpeaker(remoteStreams: Map<string, RemoteStream>) {
  const audioLevels = ref<Map<string, number>>(new Map())
  const activeSpeakerKey = ref<string | null>(null)

  let audioContext: AudioContext | null = null
  let pollInterval: ReturnType<typeof setInterval> | null = null
  const analysers = new Map<string, AnalyserEntry>()

  // Hold time: keep active speaker for 1.5s to prevent flicker
  let lastSpeakerKey: string | null = null
  let lastSpeakerTime = 0
  const HOLD_MS = 1500
  const POLL_MS = 200
  const SPEAK_THRESHOLD = 0.08

  function getOrCreateAudioContext(): AudioContext {
    if (!audioContext) {
      audioContext = new AudioContext()
    }
    return audioContext
  }

  function addStream(streamKey: string, stream: MediaStream) {
    if (analysers.has(streamKey)) return
    const audioTracks = stream.getAudioTracks()
    if (audioTracks.length === 0) return

    const ctx = getOrCreateAudioContext()
    const source = ctx.createMediaStreamSource(stream)
    const analyser = ctx.createAnalyser()
    analyser.fftSize = 256
    analyser.smoothingTimeConstant = 0.5
    source.connect(analyser)

    const dataArray = new Uint8Array(analyser.frequencyBinCount) as Uint8Array<ArrayBuffer>
    analysers.set(streamKey, { analyser, source, dataArray })
  }

  function removeStream(streamKey: string) {
    const entry = analysers.get(streamKey)
    if (entry) {
      entry.source.disconnect()
      analysers.delete(streamKey)
    }
    audioLevels.value.delete(streamKey)
  }

  function syncStreams() {
    // Add new streams
    for (const [key, remote] of remoteStreams) {
      // Skip screen shares — they don't speak
      if (key.endsWith(':screen')) continue
      if (!analysers.has(key)) {
        addStream(key, remote.stream)
      }
    }
    // Remove stale analysers
    for (const key of analysers.keys()) {
      if (!remoteStreams.has(key)) {
        removeStream(key)
      }
    }
  }

  function poll() {
    const now = Date.now()
    let maxLevel = 0
    let maxKey: string | null = null
    const levels = new Map<string, number>()

    for (const [key, entry] of analysers) {
      entry.analyser.getByteFrequencyData(entry.dataArray)
      // RMS of frequency data, normalized to 0-1
      let sum = 0
      for (let i = 0; i < entry.dataArray.length; i++) {
        const v = entry.dataArray[i] / 255
        sum += v * v
      }
      const rms = Math.sqrt(sum / entry.dataArray.length)
      levels.set(key, rms)

      if (rms > maxLevel && rms > SPEAK_THRESHOLD) {
        maxLevel = rms
        maxKey = key
      }
    }

    audioLevels.value = levels

    // Active speaker with hold time
    if (maxKey) {
      lastSpeakerKey = maxKey
      lastSpeakerTime = now
      activeSpeakerKey.value = maxKey
    } else if (lastSpeakerKey && now - lastSpeakerTime > HOLD_MS) {
      activeSpeakerKey.value = null
      lastSpeakerKey = null
    }
  }

  function start() {
    if (pollInterval) return
    syncStreams()
    pollInterval = setInterval(() => {
      syncStreams()
      poll()
    }, POLL_MS)
  }

  function stop() {
    if (pollInterval) {
      clearInterval(pollInterval)
      pollInterval = null
    }
    for (const [key] of analysers) {
      removeStream(key)
    }
    analysers.clear()
    audioLevels.value = new Map()
    activeSpeakerKey.value = null
    lastSpeakerKey = null

    if (audioContext) {
      audioContext.close().catch(() => {})
      audioContext = null
    }
  }

  return {
    audioLevels,
    activeSpeakerKey,
    start,
    stop,
  }
}
