import { defineStore } from 'pinia'
import { shallowRef, ref, reactive } from 'vue'
import { Device, type types as msTypes } from 'mediasoup-client'
import { useWsStore } from './ws'

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

export const useConferenceStore = defineStore('conference', () => {
  // --- Session metadata ---
  const isInCall = ref(false)
  const tenantId = ref<string | null>(null)
  const roomId = ref<string | null>(null)
  const roomName = ref<string | null>(null)

  // --- MediaSoup state (shallowRef for non-reactive complex objects) ---
  const device = shallowRef<Device | null>(null)
  const sendTransport = shallowRef<msTypes.Transport | null>(null)
  const recvTransport = shallowRef<msTypes.Transport | null>(null)
  const producers = reactive<Map<string, msTypes.Producer>>(new Map())
  const consumers = reactive<Map<string, msTypes.Consumer>>(new Map())
  const remoteStreams = reactive<Map<string, RemoteStream>>(new Map())
  const localStream = shallowRef<MediaStream | null>(null)
  const isMuted = ref(false)
  const isVideoOn = ref(true)
  const isScreenSharing = ref(false)

  // --- Active speaker detection ---
  const audioLevels = ref<Map<string, number>>(new Map())
  const activeSpeakerKey = ref<string | null>(null)

  // Private internals
  let screenProducerId: string | null = null
  let consumeChain = Promise.resolve()
  const consumerStreamKey = new Map<string, string>()

  // Active speaker internals
  let audioContext: AudioContext | null = null
  let pollInterval: ReturnType<typeof setInterval> | null = null
  const analysers = new Map<string, AnalyserEntry>()
  let lastSpeakerKey: string | null = null
  let lastSpeakerTime = 0
  const HOLD_MS = 1500
  const POLL_MS = 200
  const SPEAK_THRESHOLD = 0.08

  // ============ MediaSoup Methods ============

  async function joinRoom(tid: string, rid: string, rName: string) {
    const ws = useWsStore()

    tenantId.value = tid
    roomId.value = rid
    roomName.value = rName

    // Register handlers FIRST — buffer new_producer messages
    const pendingProducers: Array<{
      producer_id: string
      user_id: string
      connection_id?: string
      kind: string
      source?: string
    }> = []
    ws.onMediaMessage('media:new_producer', (data) => {
      pendingProducers.push(data)
    })
    ws.onMediaMessage('media:peer_left', handlePeerLeft)
    ws.onMediaMessage('media:producer_closed', handleProducerClosed)

    const capabilitiesPromise = ws.waitForMessage('media:router_capabilities')
    const transportPromise = ws.waitForMessage('media:transport_created')

    ws.send('media:join', { room_id: rid })

    const capsMsg = await capabilitiesPromise
    const transportMsg = await transportPromise

    const dev = new Device()
    await dev.load({ routerRtpCapabilities: capsMsg.rtp_capabilities })
    device.value = dev

    const forceRelay = !!transportMsg.force_relay
    const iceServers = transportMsg.ice_servers?.length
      ? transportMsg.ice_servers.map(
          (s: { urls: string[]; username: string; credential: string }) => ({
            urls: s.urls,
            username: s.username,
            credential: s.credential,
          }),
        )
      : undefined
    const iceTransportPolicy = forceRelay ? 'relay' : 'all'

    // Create send transport
    const sendParams = transportMsg.send_transport
    const st = dev.createSendTransport({
      id: sendParams.id,
      iceParameters: sendParams.ice_parameters,
      iceCandidates: sendParams.ice_candidates,
      dtlsParameters: sendParams.dtls_parameters,
      iceServers,
      iceTransportPolicy,
    })

    st.on('connectionstatechange', async (state: string) => {
      console.log('[conference] sendTransport connectionState:', state)
      if (state === 'failed') {
        try {
          const stats = await st.getStats()
          stats.forEach((report: Record<string, unknown>) => {
            if (
              report.type === 'candidate-pair' ||
              report.type === 'local-candidate' ||
              report.type === 'remote-candidate'
            ) {
              console.log('[conference] sendTransport stats:', report)
            }
          })
        } catch {
          /* stats not available */
        }
      }
    })

    st.on('connect', ({ dtlsParameters }, callback) => {
      ws.send('media:connect_transport', {
        room_id: rid,
        transport_id: st.id,
        dtls_parameters: dtlsParameters,
      })
      callback()
    })

    st.on('produce', async ({ kind, rtpParameters, appData }, callback, errback) => {
      try {
        const resultPromise = ws.waitForMessage('media:produce_result')
        ws.send('media:produce', {
          room_id: rid,
          kind,
          rtp_parameters: rtpParameters,
          source: appData?.source || (kind === 'audio' ? 'audio' : 'camera'),
        })
        const result = await resultPromise
        callback({ id: result.id })
      } catch (err) {
        errback(err as Error)
      }
    })

    sendTransport.value = st

    // Create recv transport
    const recvParams = transportMsg.recv_transport
    const rt = dev.createRecvTransport({
      id: recvParams.id,
      iceParameters: recvParams.ice_parameters,
      iceCandidates: recvParams.ice_candidates,
      dtlsParameters: recvParams.dtls_parameters,
      iceServers,
      iceTransportPolicy,
    })

    rt.on('connectionstatechange', async (state: string) => {
      console.log('[conference] recvTransport connectionState:', state)
      if (state === 'failed') {
        try {
          const stats = await rt.getStats()
          stats.forEach((report: Record<string, unknown>) => {
            if (
              report.type === 'candidate-pair' ||
              report.type === 'local-candidate' ||
              report.type === 'remote-candidate'
            ) {
              console.log('[conference] recvTransport stats:', report)
            }
          })
        } catch {
          /* stats not available */
        }
      }
    })

    rt.on('connect', ({ dtlsParameters }, callback) => {
      ws.send('media:connect_transport', {
        room_id: rt.id,
        transport_id: rt.id,
        dtls_parameters: dtlsParameters,
      })
      callback()
    })

    recvTransport.value = rt

    // Replace buffering handler with the real one
    ws.onMediaMessage('media:new_producer', handleNewProducer)

    // Process buffered producers
    for (const p of pendingProducers) {
      handleNewProducer(p)
    }

    isInCall.value = true
  }

  async function produceLocalMedia() {
    const stream = await navigator.mediaDevices.getUserMedia({
      audio: true,
      video: true,
    })
    localStream.value = stream

    if (!sendTransport.value) return

    const audioTrack = stream.getAudioTracks()[0]
    if (audioTrack) {
      const audioProducer = await sendTransport.value.produce({
        track: audioTrack,
        appData: { source: 'audio' },
      })
      producers.set('audio', audioProducer)
    }

    const videoTrack = stream.getVideoTracks()[0]
    if (videoTrack) {
      const videoProducer = await sendTransport.value.produce({
        track: videoTrack,
        appData: { source: 'camera' },
      })
      producers.set('video', videoProducer)
    }

    return stream
  }

  async function consumeProducer(
    producerId: string,
    userId: string,
    connectionId: string,
    source: string,
  ) {
    if (!recvTransport.value || !device.value) return

    const ws = useWsStore()
    const resultPromise = ws.waitForMessage('media:consumer_created')
    ws.send('media:consume', {
      room_id: roomId.value,
      producer_id: producerId,
      rtp_capabilities: device.value.rtpCapabilities,
    })

    const result = await resultPromise

    const consumer = await recvTransport.value.consume({
      id: result.id,
      producerId: result.producer_id,
      kind: result.kind as msTypes.MediaKind,
      rtpParameters: result.rtp_parameters,
    })

    consumers.set(consumer.id, consumer)

    const streamKey = source === 'screen' ? `${connectionId}:screen` : connectionId
    consumerStreamKey.set(consumer.id, streamKey)

    const existing = remoteStreams.get(streamKey)
    const tracks = existing ? [...existing.stream.getTracks(), consumer.track] : [consumer.track]
    const newStream = new MediaStream(tracks)
    remoteStreams.set(streamKey, {
      userId,
      connectionId,
      stream: newStream,
      kind: result.kind,
      source,
    })
  }

  function handleNewProducer(data: {
    producer_id: string
    user_id: string
    connection_id?: string
    kind: string
    source?: string
  }) {
    const source = data.source || (data.kind === 'audio' ? 'audio' : 'camera')
    const connectionId = data.connection_id || data.user_id
    consumeChain = consumeChain
      .then(() => consumeProducer(data.producer_id, data.user_id, connectionId, source))
      .catch((err) => console.error('[conference] consumeProducer failed:', err))
  }

  function handlePeerLeft(data: { user_id: string; connection_id?: string }) {
    const connectionId = data.connection_id || data.user_id
    remoteStreams.delete(connectionId)
    remoteStreams.delete(`${connectionId}:screen`)
    for (const [id, consumer] of consumers) {
      const key = consumerStreamKey.get(id)
      if (key === connectionId || key === `${connectionId}:screen`) {
        consumer.close()
        consumers.delete(id)
        consumerStreamKey.delete(id)
      }
    }
  }

  function handleProducerClosed(data: { producer_id: string; user_id: string }) {
    for (const [id, consumer] of consumers) {
      if (consumer.producerId === data.producer_id) {
        consumer.close()
        consumers.delete(id)
        const key = consumerStreamKey.get(id)
        consumerStreamKey.delete(id)
        if (key && key.endsWith(':screen')) {
          remoteStreams.delete(key)
        }
        break
      }
    }
  }

  function toggleMute() {
    isMuted.value = !isMuted.value
    const audioProducer = producers.get('audio')
    if (audioProducer) {
      if (isMuted.value) audioProducer.pause()
      else audioProducer.resume()
    }
    if (localStream.value) {
      localStream.value.getAudioTracks().forEach((t) => {
        t.enabled = !isMuted.value
      })
    }
  }

  function toggleVideo() {
    isVideoOn.value = !isVideoOn.value
    const videoProducer = producers.get('video')
    if (videoProducer) {
      if (!isVideoOn.value) videoProducer.pause()
      else videoProducer.resume()
    }
    if (localStream.value) {
      localStream.value.getVideoTracks().forEach((t) => {
        t.enabled = isVideoOn.value
      })
    }
  }

  async function startScreenShare() {
    if (!sendTransport.value) return

    const screenStream = await navigator.mediaDevices.getDisplayMedia({ video: true })
    const screenTrack = screenStream.getVideoTracks()[0]
    if (!screenTrack) return

    const screenProducer = await sendTransport.value.produce({
      track: screenTrack,
      appData: { source: 'screen' },
    })
    screenProducerId = screenProducer.id
    producers.set('screen', screenProducer)
    isScreenSharing.value = true

    screenTrack.onended = () => {
      stopScreenShare()
    }
  }

  function stopScreenShare() {
    const screenProducer = producers.get('screen')
    if (screenProducer) {
      screenProducer.close()
      producers.delete('screen')

      if (roomId.value && screenProducerId) {
        const ws = useWsStore()
        ws.send('media:producer_close', {
          room_id: roomId.value,
          producer_id: screenProducerId,
        })
      }
    }
    screenProducerId = null
    isScreenSharing.value = false
  }

  /** Leave the call — only called on explicit hangup, NOT on unmount */
  function leaveRoom() {
    const ws = useWsStore()

    if (roomId.value) {
      ws.send('media:leave', { room_id: roomId.value })
    }

    // Stop active speaker detection
    stopActiveSpeaker()

    // Close all producers
    for (const [, producer] of producers) {
      producer.close()
    }
    producers.clear()

    // Close all consumers
    for (const [, consumer] of consumers) {
      consumer.close()
    }
    consumers.clear()
    consumerStreamKey.clear()

    // Close transports
    sendTransport.value?.close()
    recvTransport.value?.close()
    sendTransport.value = null
    recvTransport.value = null

    // Stop local stream
    if (localStream.value) {
      localStream.value.getTracks().forEach((t) => t.stop())
      localStream.value = null
    }

    remoteStreams.clear()
    device.value = null
    consumeChain = Promise.resolve()

    // Remove WS listeners
    ws.offMediaMessage('media:new_producer')
    ws.offMediaMessage('media:peer_left')
    ws.offMediaMessage('media:producer_closed')

    // Reset state
    isInCall.value = false
    isMuted.value = false
    isVideoOn.value = true
    isScreenSharing.value = false
    tenantId.value = null
    roomId.value = null
    roomName.value = null
  }

  // ============ Active Speaker Detection ============

  function getOrCreateAudioContext(): AudioContext {
    if (!audioContext) {
      audioContext = new AudioContext()
    }
    return audioContext
  }

  function addSpeakerStream(streamKey: string, stream: MediaStream) {
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

  function removeSpeakerStream(streamKey: string) {
    const entry = analysers.get(streamKey)
    if (entry) {
      entry.source.disconnect()
      analysers.delete(streamKey)
    }
    audioLevels.value.delete(streamKey)
  }

  function syncSpeakerStreams() {
    for (const [key, remote] of remoteStreams) {
      if (key.endsWith(':screen')) continue
      if (!analysers.has(key)) {
        addSpeakerStream(key, remote.stream)
      }
    }
    for (const key of analysers.keys()) {
      if (!remoteStreams.has(key)) {
        removeSpeakerStream(key)
      }
    }
  }

  function pollSpeaker() {
    const now = Date.now()
    let maxLevel = 0
    let maxKey: string | null = null
    const levels = new Map<string, number>()

    for (const [key, entry] of analysers) {
      entry.analyser.getByteFrequencyData(entry.dataArray)
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

    if (maxKey) {
      lastSpeakerKey = maxKey
      lastSpeakerTime = now
      activeSpeakerKey.value = maxKey
    } else if (lastSpeakerKey && now - lastSpeakerTime > HOLD_MS) {
      activeSpeakerKey.value = null
      lastSpeakerKey = null
    }
  }

  function startActiveSpeaker() {
    if (pollInterval) return
    syncSpeakerStreams()
    pollInterval = setInterval(() => {
      syncSpeakerStreams()
      pollSpeaker()
    }, POLL_MS)
  }

  function stopActiveSpeaker() {
    if (pollInterval) {
      clearInterval(pollInterval)
      pollInterval = null
    }
    for (const [key] of analysers) {
      removeSpeakerStream(key)
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
    // Session metadata
    isInCall,
    tenantId,
    roomId,
    roomName,

    // MediaSoup state
    device,
    sendTransport,
    recvTransport,
    producers,
    consumers,
    remoteStreams,
    localStream,
    isMuted,
    isVideoOn,
    isScreenSharing,

    // Active speaker
    audioLevels,
    activeSpeakerKey,

    // Methods
    joinRoom,
    produceLocalMedia,
    leaveRoom,
    toggleMute,
    toggleVideo,
    startScreenShare,
    stopScreenShare,
    startActiveSpeaker,
    stopActiveSpeaker,
  }
})
