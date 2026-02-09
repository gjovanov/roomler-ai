import { ref, reactive, onUnmounted } from 'vue'
import { Device, type types as msTypes } from 'mediasoup-client'
import { useWsStore } from '@/stores/ws'

interface RemoteStream {
  userId: string
  stream: MediaStream
  kind: string
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

  const ws = useWsStore()

  /** Join a mediasoup room: loads device, creates transports */
  async function joinRoom(confId: string) {
    conferenceId.value = confId

    // Register handlers FIRST so no messages are dropped while awaiting transports.
    // Buffer any new_producer messages that arrive before transports are ready.
    const pendingProducers: Array<{ producer_id: string; user_id: string; kind: string }> = []
    ws.onMediaMessage('media:new_producer', (data) => pendingProducers.push(data))
    ws.onMediaMessage('media:peer_left', handlePeerLeft)
    ws.onMediaMessage('media:producer_closed', handleProducerClosed)

    // Send media:join and wait for router_capabilities + transport_created
    const capabilitiesPromise = ws.waitForMessage('media:router_capabilities')
    const transportPromise = ws.waitForMessage('media:transport_created')

    ws.send('media:join', { conference_id: confId })

    const capsMsg = await capabilitiesPromise
    const transportMsg = await transportPromise

    // Load device with router RTP capabilities
    const dev = new Device()
    await dev.load({ routerRtpCapabilities: capsMsg.rtp_capabilities })
    device.value = dev

    // Create send transport
    const sendParams = transportMsg.send_transport
    const st = dev.createSendTransport({
      id: sendParams.id,
      iceParameters: sendParams.ice_parameters,
      iceCandidates: sendParams.ice_candidates,
      dtlsParameters: sendParams.dtls_parameters,
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

    st.on('produce', async ({ kind, rtpParameters }, callback, errback) => {
      try {
        const resultPromise = ws.waitForMessage('media:produce_result')
        ws.send('media:produce', {
          conference_id: confId,
          kind,
          rtp_parameters: rtpParameters,
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

    // Replace the buffering handler with the real one now that transports are ready
    ws.onMediaMessage('media:new_producer', handleNewProducer)

    // Process any producers that arrived while we were setting up transports
    for (const p of pendingProducers) {
      handleNewProducer(p)
    }
  }

  /** Produce local audio + video tracks */
  async function produceLocalMedia() {
    const stream = await navigator.mediaDevices.getUserMedia({
      audio: true,
      video: true,
    })
    localStream.value = stream

    if (!sendTransport.value) return

    // Produce audio
    const audioTrack = stream.getAudioTracks()[0]
    if (audioTrack) {
      const audioProducer = await sendTransport.value.produce({ track: audioTrack })
      producers.set('audio', audioProducer)
    }

    // Produce video
    const videoTrack = stream.getVideoTracks()[0]
    if (videoTrack) {
      const videoProducer = await sendTransport.value.produce({ track: videoTrack })
      producers.set('video', videoProducer)
    }

    return stream
  }

  /** Consume a remote producer */
  async function consumeProducer(producerId: string, userId: string) {
    if (!recvTransport.value || !device.value) return

    const resultPromise = ws.waitForMessage('media:consumer_created')
    ws.send('media:consume', {
      conference_id: conferenceId.value,
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

    // Attach consumer track to remote stream
    const existing = remoteStreams.get(userId)
    if (existing) {
      existing.stream.addTrack(consumer.track)
    } else {
      const stream = new MediaStream([consumer.track])
      remoteStreams.set(userId, {
        userId,
        stream,
        kind: result.kind,
      })
    }
  }

  /** Handle new_producer from WS */
  function handleNewProducer(data: { producer_id: string; user_id: string; kind: string }) {
    consumeProducer(data.producer_id, data.user_id)
  }

  /** Handle peer_left from WS */
  function handlePeerLeft(data: { user_id: string }) {
    // Remove remote stream
    remoteStreams.delete(data.user_id)
    // Close associated consumers
    for (const [id, consumer] of consumers) {
      // Consumer doesn't directly expose userId, so we clean up by checking closed state
      if (consumer.closed) {
        consumers.delete(id)
      }
    }
  }

  /** Handle producer_closed from WS */
  function handleProducerClosed(data: { producer_id: string; user_id: string }) {
    for (const [id, consumer] of consumers) {
      if (consumer.producerId === data.producer_id) {
        consumer.close()
        consumers.delete(id)
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

    const screenProducer = await sendTransport.value.produce({ track: screenTrack })
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
