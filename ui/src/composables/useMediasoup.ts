import { ref, reactive, onUnmounted } from 'vue'
import { Device, type types as msTypes } from 'mediasoup-client'
import { useWsStore } from '@/stores/ws'

interface RemoteStream {
  userId: string
  connectionId: string
  stream: MediaStream
  kind: string
  source: string
}

export function useMediasoup() {
  const device = ref<Device | null>(null)
  const sendTransport = ref<msTypes.Transport | null>(null)
  const recvTransport = ref<msTypes.Transport | null>(null)
  const producers = reactive<Map<string, msTypes.Producer>>(new Map())
  const consumers = reactive<Map<string, msTypes.Consumer>>(new Map())
  const remoteStreams = reactive<Map<string, RemoteStream>>(new Map())
  const localStream = ref<MediaStream | null>(null)
  const isMuted = ref(false)
  const isVideoOn = ref(true)
  const isScreenSharing = ref(false)
  const conferenceId = ref<string | null>(null)

  let screenProducerId: string | null = null
  // Serialize consumeProducer calls — pendingWaiters only holds one waiter per message type,
  // so concurrent consume requests cause the earlier waiter to be overwritten and timeout.
  let consumeChain = Promise.resolve()
  // Track consumer → stream key for cleanup on producer_closed
  const consumerStreamKey = new Map<string, string>()

  const ws = useWsStore()

  /** Join a mediasoup room: loads device, creates transports */
  async function joinRoom(confId: string) {
    conferenceId.value = confId
    console.log('[mediasoup] joinRoom:', confId)

    // Register handlers FIRST so no messages are dropped while awaiting transports.
    // Buffer any new_producer messages that arrive before transports are ready.
    const pendingProducers: Array<{ producer_id: string; user_id: string; connection_id?: string; kind: string; source?: string }> = []
    ws.onMediaMessage('media:new_producer', (data) => {
      console.log('[mediasoup] buffering new_producer:', data)
      pendingProducers.push(data)
    })
    ws.onMediaMessage('media:peer_left', handlePeerLeft)
    ws.onMediaMessage('media:producer_closed', handleProducerClosed)

    // Send media:join and wait for router_capabilities + transport_created
    const capabilitiesPromise = ws.waitForMessage('media:router_capabilities')
    const transportPromise = ws.waitForMessage('media:transport_created')

    ws.send('media:join', { conference_id: confId })

    const capsMsg = await capabilitiesPromise
    console.log('[mediasoup] got router_capabilities')
    const transportMsg = await transportPromise
    console.log('[mediasoup] got transport_created, send:', transportMsg.send_transport?.id, 'recv:', transportMsg.recv_transport?.id)

    // Load device with router RTP capabilities
    const dev = new Device()
    await dev.load({ routerRtpCapabilities: capsMsg.rtp_capabilities })
    device.value = dev
    console.log('[mediasoup] device loaded')

    // Build iceServers from TURN config (if provided by server)
    const iceServers = transportMsg.ice_servers?.length
      ? transportMsg.ice_servers.map((s: { urls: string[]; username: string; credential: string }) => ({
          urls: s.urls,
          username: s.username,
          credential: s.credential,
        }))
      : undefined
    const iceTransportPolicy = transportMsg.force_relay ? 'relay' : 'all'
    console.log('[mediasoup] ICE config:', { iceServers, iceTransportPolicy, force_relay: transportMsg.force_relay })

    // Log ICE candidate details from server
    for (const label of ['send_transport', 'recv_transport'] as const) {
      const tp = transportMsg[label]
      if (tp?.ice_candidates) {
        for (const c of tp.ice_candidates) {
          console.log(`[mediasoup] ${label} ICE candidate:`, {
            protocol: c.protocol,
            address: c.address ?? c.ip,
            port: c.port,
            tcpType: c.tcpType ?? 'n/a',
            type: c.type,
          })
        }
      }
    }

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
      console.log('[mediasoup] sendTransport connectionState:', state)
      if (state === 'failed') {
        try {
          const stats = await st.getStats()
          stats.forEach((report: Record<string, unknown>) => {
            if (report.type === 'candidate-pair' || report.type === 'local-candidate' || report.type === 'remote-candidate') {
              console.log('[mediasoup] sendTransport stats:', report)
            }
          })
        } catch { /* stats not available */ }
      }
    })

    st.on('connect', ({ dtlsParameters }, callback, errback) => {
      ws.send('media:connect_transport', {
        conference_id: confId,
        transport_id: st.id,
        dtls_parameters: dtlsParameters,
      })
      // Assume success — server will send error if it fails
      callback()
    })

    st.on('produce', async ({ kind, rtpParameters, appData }, callback, errback) => {
      try {
        const resultPromise = ws.waitForMessage('media:produce_result')
        ws.send('media:produce', {
          conference_id: confId,
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
      console.log('[mediasoup] recvTransport connectionState:', state)
      if (state === 'failed') {
        try {
          const stats = await rt.getStats()
          stats.forEach((report: Record<string, unknown>) => {
            if (report.type === 'candidate-pair' || report.type === 'local-candidate' || report.type === 'remote-candidate') {
              console.log('[mediasoup] recvTransport stats:', report)
            }
          })
        } catch { /* stats not available */ }
      }
    })

    rt.on('connect', ({ dtlsParameters }, callback, errback) => {
      ws.send('media:connect_transport', {
        conference_id: confId,
        transport_id: rt.id,
        dtls_parameters: dtlsParameters,
      })
      callback()
    })

    recvTransport.value = rt
    console.log('[mediasoup] transports created, buffered producers:', pendingProducers.length)

    // Replace the buffering handler with the real one now that transports are ready
    ws.onMediaMessage('media:new_producer', handleNewProducer)

    // Process any producers that arrived while we were setting up transports
    for (const p of pendingProducers) {
      handleNewProducer(p)
    }
  }

  /** Produce local audio + video tracks */
  async function produceLocalMedia() {
    console.log('[mediasoup] producing local media...')
    const stream = await navigator.mediaDevices.getUserMedia({
      audio: true,
      video: true,
    })
    localStream.value = stream
    console.log('[mediasoup] getUserMedia OK, audio tracks:', stream.getAudioTracks().length, 'video tracks:', stream.getVideoTracks().length)

    if (!sendTransport.value) {
      console.warn('[mediasoup] no sendTransport, cannot produce')
      return
    }

    // Produce audio
    const audioTrack = stream.getAudioTracks()[0]
    if (audioTrack) {
      const audioProducer = await sendTransport.value.produce({ track: audioTrack, appData: { source: 'audio' } })
      producers.set('audio', audioProducer)
      console.log('[mediasoup] audio producer created:', audioProducer.id)
    }

    // Produce video
    const videoTrack = stream.getVideoTracks()[0]
    if (videoTrack) {
      const videoProducer = await sendTransport.value.produce({ track: videoTrack, appData: { source: 'camera' } })
      producers.set('video', videoProducer)
      console.log('[mediasoup] video producer created:', videoProducer.id)
    }

    return stream
  }

  /** Consume a remote producer */
  async function consumeProducer(producerId: string, userId: string, connectionId: string, source: string) {
    if (!recvTransport.value || !device.value) {
      console.warn('[mediasoup] consumeProducer: missing recvTransport or device')
      return
    }

    console.log('[mediasoup] consumeProducer:', producerId, 'from user:', userId, 'conn:', connectionId, 'source:', source)
    const resultPromise = ws.waitForMessage('media:consumer_created')
    ws.send('media:consume', {
      conference_id: conferenceId.value,
      producer_id: producerId,
      rtp_capabilities: device.value.rtpCapabilities,
    })

    const result = await resultPromise
    console.log('[mediasoup] consumer_created response:', result.id, result.kind, 'source:', source)

    const consumer = await recvTransport.value.consume({
      id: result.id,
      producerId: result.producer_id,
      kind: result.kind as msTypes.MediaKind,
      rtpParameters: result.rtp_parameters,
    })

    consumers.set(consumer.id, consumer)
    console.log('[mediasoup] consumer track:', consumer.track.kind, 'readyState:', consumer.track.readyState, 'enabled:', consumer.track.enabled, 'muted:', consumer.track.muted)

    // Key by connectionId so same user from multiple tabs gets separate streams.
    // Screen shares get their own key: `${connectionId}:screen`.
    const streamKey = source === 'screen' ? `${connectionId}:screen` : connectionId

    // Track consumer → stream key for cleanup
    consumerStreamKey.set(consumer.id, streamKey)

    // Attach consumer track to remote stream.
    // Always create a new MediaStream so that Vue reactivity picks up the change
    // (addTrack on an existing stream is a DOM mutation invisible to Vue).
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
    console.log('[mediasoup] remoteStreams updated for', streamKey, '— total tracks:', newStream.getTracks().length, 'video:', newStream.getVideoTracks().length)
  }

  /** Handle new_producer from WS — queued to avoid concurrent waitForMessage collisions */
  function handleNewProducer(data: { producer_id: string; user_id: string; connection_id?: string; kind: string; source?: string }) {
    const source = data.source || (data.kind === 'audio' ? 'audio' : 'camera')
    const connectionId = data.connection_id || data.user_id // fallback for backwards compat
    console.log('[mediasoup] handleNewProducer:', data.kind, 'source:', source, 'from', data.user_id, 'conn:', connectionId, 'producer:', data.producer_id)
    consumeChain = consumeChain
      .then(() => consumeProducer(data.producer_id, data.user_id, connectionId, source))
      .catch((err) => console.error('[mediasoup] consumeProducer failed:', err))
  }

  /** Handle peer_left from WS */
  function handlePeerLeft(data: { user_id: string; connection_id?: string }) {
    const connectionId = data.connection_id || data.user_id
    console.log('[mediasoup] handlePeerLeft: user:', data.user_id, 'conn:', connectionId)
    // Remove remote streams keyed by this connectionId (camera + screen)
    remoteStreams.delete(connectionId)
    remoteStreams.delete(`${connectionId}:screen`)
    // Close associated consumers whose stream key matches this connection
    for (const [id, consumer] of consumers) {
      const key = consumerStreamKey.get(id)
      if (key === connectionId || key === `${connectionId}:screen`) {
        consumer.close()
        consumers.delete(id)
        consumerStreamKey.delete(id)
      }
    }
  }

  /** Handle producer_closed from WS */
  function handleProducerClosed(data: { producer_id: string; user_id: string }) {
    for (const [id, consumer] of consumers) {
      if (consumer.producerId === data.producer_id) {
        consumer.close()
        consumers.delete(id)
        // Clean up the stream entry if it was a screen share (single-track stream)
        const key = consumerStreamKey.get(id)
        consumerStreamKey.delete(id)
        if (key && key.endsWith(':screen')) {
          remoteStreams.delete(key)
        }
        break
      }
    }
  }

  /** Toggle audio mute */
  function toggleMute() {
    isMuted.value = !isMuted.value
    const audioProducer = producers.get('audio')
    if (audioProducer) {
      if (isMuted.value) {
        audioProducer.pause()
      } else {
        audioProducer.resume()
      }
    }
    // Also toggle local stream track
    if (localStream.value) {
      localStream.value.getAudioTracks().forEach((t) => {
        t.enabled = !isMuted.value
      })
    }
  }

  /** Toggle video on/off */
  function toggleVideo() {
    isVideoOn.value = !isVideoOn.value
    const videoProducer = producers.get('video')
    if (videoProducer) {
      if (!isVideoOn.value) {
        videoProducer.pause()
      } else {
        videoProducer.resume()
      }
    }
    if (localStream.value) {
      localStream.value.getVideoTracks().forEach((t) => {
        t.enabled = isVideoOn.value
      })
    }
  }

  /** Start screen sharing */
  async function startScreenShare() {
    if (!sendTransport.value) return

    const screenStream = await navigator.mediaDevices.getDisplayMedia({
      video: true,
    })
    const screenTrack = screenStream.getVideoTracks()[0]
    if (!screenTrack) return

    const screenProducer = await sendTransport.value.produce({
      track: screenTrack,
      appData: { source: 'screen' },
    })
    screenProducerId = screenProducer.id
    producers.set('screen', screenProducer)
    isScreenSharing.value = true

    // When user stops sharing via browser UI
    screenTrack.onended = () => {
      stopScreenShare()
    }
  }

  /** Stop screen sharing */
  function stopScreenShare() {
    const screenProducer = producers.get('screen')
    if (screenProducer) {
      screenProducer.close()
      producers.delete('screen')

      // Notify server
      if (conferenceId.value && screenProducerId) {
        ws.send('media:producer_close', {
          conference_id: conferenceId.value,
          producer_id: screenProducerId,
        })
      }
    }
    screenProducerId = null
    isScreenSharing.value = false
  }

  /** Leave the mediasoup room */
  function leaveRoom() {
    if (conferenceId.value) {
      ws.send('media:leave', { conference_id: conferenceId.value })
    }

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
    conferenceId.value = null
    isMuted.value = false
    isVideoOn.value = true
    isScreenSharing.value = false
    consumeChain = Promise.resolve()

    // Remove WS listeners
    ws.offMediaMessage('media:new_producer')
    ws.offMediaMessage('media:peer_left')
    ws.offMediaMessage('media:producer_closed')
  }

  onUnmounted(() => {
    leaveRoom()
  })

  return {
    device,
    localStream,
    remoteStreams,
    producers,
    consumers,
    isMuted,
    isVideoOn,
    isScreenSharing,
    conferenceId,
    joinRoom,
    produceLocalMedia,
    leaveRoom,
    toggleMute,
    toggleVideo,
    startScreenShare,
    stopScreenShare,
  }
}
