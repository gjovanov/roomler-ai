// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
import { defineStore } from 'pinia'
import { shallowRef, ref, reactive } from 'vue'
import { Device, type types as msTypes } from 'mediasoup-client'
import { useWsStore } from './ws'
import { buildViewReport, type TileVisibility } from '@/utils/viewReport'
import { cameraEncodings, captureSize, type CameraEncoding } from '@/utils/simulcast'

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
  // FR-30 — streamKey -> "their camera is off right now", from the peer's own
  // `media:producer_paused`. It cannot be derived from the received track: a
  // paused sender's track stays `live` and UNMUTED on this side (measured on
  // prod 2026-08-29), which is why "hide participants without video" could
  // never hide anyone.
  const remoteVideoPaused = reactive<Map<string, boolean>>(new Map())
  // FR-30 P3 — the same signal for audio. A remote's mute state was hardcoded
  // `false` in the layout, so the mic-off badge could only ever appear on your
  // own tile; the wire already carries it, kind-agnostic.
  const remoteAudioPaused = reactive<Map<string, boolean>>(new Map())
  // FR-127 P1 — the levers the server runs, from `media:transport_created`'s
  // `meeting` object: `view` — it pauses the video we do not show, so we
  // report what we show; `audioDtx` — produce the microphone with Opus DTX;
  // P2 — `simulcast`: the ladder the camera publishes, or null for one
  // encoding (the lever off, an older server, a ladder we could not read).
  // All off until a join answers, and off against an older server that
  // sends no `meeting`: then nothing is reported and audio and video are
  // produced as before, which is exactly what that server expects.
  const meeting = ref<{ view: boolean; audioDtx: boolean; simulcast: CameraEncoding[] | null }>({
    view: false,
    audioDtx: false,
    simulcast: null,
  })
  // P2 — whether the tiles' reports go out at all: the server reads them for
  // its pausing (`view`) and for its layer selection (`simulcast`), so either
  // lever asks for them; neither is worth a report in a browser whose tiles
  // cannot observe themselves.
  let reportsView = false
  const localStream = shallowRef<MediaStream | null>(null)
  const isMuted = ref(false)
  const isVideoOn = ref(true)
  const isScreenSharing = ref(false)

  // --- Device selection ---
  const availableDevices = ref<MediaDeviceInfo[]>([])
  const selectedAudioDeviceId = ref<string>('')
  const selectedVideoDeviceId = ref<string>('')

  async function enumerateDevices() {
    try {
      const devices = await navigator.mediaDevices.enumerateDevices()
      availableDevices.value = devices

      // Auto-select first audio input if none selected
      if (!selectedAudioDeviceId.value) {
        const firstAudio = devices.find(d => d.kind === 'audioinput' && d.deviceId)
        if (firstAudio) {
          selectedAudioDeviceId.value = firstAudio.deviceId
        }
      }

      // Auto-select first video input if none selected
      if (!selectedVideoDeviceId.value) {
        const firstVideo = devices.find(d => d.kind === 'videoinput' && d.deviceId)
        if (firstVideo) {
          selectedVideoDeviceId.value = firstVideo.deviceId
        }
      }
    } catch (err) {
      console.error('[conference] Failed to enumerate devices:', err)
    }
  }

  // --- Active speaker detection ---
  const audioLevels = ref<Map<string, number>>(new Map())
  const activeSpeakerKey = ref<string | null>(null)

  // Private internals
  let screenProducerId: string | null = null
  let consumeChain = Promise.resolve()
  const consumerStreamKey = new Map<string, string>()

  // FR-127 P1 — receive what is visible. The tiles on screen by stream key
  // (a count per key, not a flag: a layout switch may mount the new tile
  // before the old one unmounts), the video producer each stream key
  // consumes, and the `media:view` sender — debounced, with a `seq` that only
  // ever grows so the server can drop a report that arrives after a newer one.
  // `seq` is never reset: a re-join on this page continues it, and the
  // server's fresh participant accepts any first seq.
  const tilesOnScreen = new Map<string, { n: number; w: number; h: number; spotlit: boolean }>()
  const videoProducerByStreamKey = new Map<string, string>()
  let viewSeq = 0
  let viewTimer: ReturnType<typeof setTimeout> | null = null
  const VIEW_DEBOUNCE_MS = 100
  // Self-healing: a report the socket dropped (it sends only while OPEN) or a
  // forward the owner pod never got (best-effort on the bus) would leave the
  // view stale until the next tile change. So the current view goes out again
  // every VIEW_RESEND_MS while in a call, and when the tab comes back to the
  // foreground; `seq` keeps it idempotent and the server plans nothing for a
  // report that changes nothing (found in review).
  const VIEW_RESEND_MS = 15_000
  let viewResend: ReturnType<typeof setInterval> | null = null

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
    ws.onMediaMessage('media:producer_paused', handleProducerPaused)

    let capabilitiesPromise = ws.waitForMessage('media:router_capabilities')
    let transportPromise = ws.waitForMessage('media:transport_created')

    // FR-127 P1 — `caps: ["view"]`: this client reports what it renders, so
    // the server may pause the video it does not show. Declared only where
    // the tiles CAN report (an IntersectionObserver): a declared viewer that
    // never reported would have every camera paused. An older server reads
    // past the field and resumes everything, as before.
    const canReportView = typeof IntersectionObserver !== 'undefined'
    const joinData = canReportView ? { room_id: rid, caps: ['view'] } : { room_id: rid }
    ws.send('media:join', joinData)

    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    let capsMsg: any
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    let transportMsg: any
    try {
      capsMsg = await capabilitiesPromise
      transportMsg = await transportPromise
    } catch (e) {
      // Field 2026-08-04: a join sent into a half-dead socket ("Connection
      // reset without closing handshake" — mobile radio blips, pod rolls)
      // never reaches the server, so the capabilities wait times out while
      // the store's 3 s auto-reconnect quietly brings up a NEW socket (and
      // a NEW connection_id). Retry ONCE on the fresh socket: re-run the
      // HTTP call/join so the session is re-scoped to the new connection
      // (the server closes the dead connection's session on disconnect —
      // without the re-join the call could auto-end under us), then
      // re-send media:join.
      console.warn('[conference] join wait failed — retrying once after WS reconnect:', e)
      const deadline = Date.now() + 8000
      while (ws.status !== 'connected' && Date.now() < deadline) {
        await new Promise((r) => setTimeout(r, 250))
      }
      if (ws.status !== 'connected') throw e
      const { useRoomStore } = await import('./rooms')
      await useRoomStore()
        .joinCall(tid, rid)
        .catch(() => {})
      capabilitiesPromise = ws.waitForMessage('media:router_capabilities')
      transportPromise = ws.waitForMessage('media:transport_created')
      ws.send('media:join', joinData)
      capsMsg = await capabilitiesPromise
      transportMsg = await transportPromise
      console.warn('[conference] join retry succeeded on the reconnected socket')
    }

    const dev = new Device()
    await dev.load({ routerRtpCapabilities: capsMsg.rtp_capabilities })
    device.value = dev

    const forceRelay = !!transportMsg.force_relay
    // FR-127 P1 — which levers this server runs (absent on an older server:
    // all off, today's behaviour). P2 — the ladder is published whatever
    // this browser's tiles can do: the layers serve the OTHER viewers.
    const levers = transportMsg.meeting
    meeting.value = {
      view: canReportView && levers?.view === true,
      audioDtx: levers?.audio_dtx === true,
      simulcast: cameraEncodings(levers?.simulcast),
    }
    reportsView = canReportView && (levers?.view === true || meeting.value.simulcast !== null)
    // ⚠️ Drop entries with no usable URL before they reach RTCPeerConnection.
    // A server configured with a BLANK turn url used to advertise `urls: [""]`,
    // and the constructor rejects the whole thing —
    //   Failed to construct 'RTCPeerConnection': '' is not a valid URL
    // — so the call could not be joined at all, reported only by a snackbar
    // that fades. Fixed server-side too; this keeps an older server harmless.
    const iceServers = transportMsg.ice_servers?.length
      ? transportMsg.ice_servers
          .map((s: { urls: string[] | string; username: string; credential: string }) => ({
            urls: (Array.isArray(s.urls) ? s.urls : [s.urls]).filter(
              (u) => typeof u === 'string' && u.trim().length > 0,
            ),
            username: s.username,
            credential: s.credential,
          }))
          .filter((s: { urls: string[] }) => s.urls.length > 0)
      : undefined
    const iceTransportPolicy = forceRelay ? 'relay' : 'all'
    console.log('[conference] ICE config:', { iceServers, iceTransportPolicy, forceRelay })

    // Log server-provided ICE candidates
    for (const label of ['send_transport', 'recv_transport'] as const) {
      const tp = transportMsg[label]
      if (tp?.ice_candidates) {
        for (const c of tp.ice_candidates) {
          console.log(`[conference] ${label} ICE candidate:`, {
            protocol: c.protocol, address: c.address ?? c.ip,
            port: c.port, type: c.type, tcpType: c.tcpType,
          })
        }
      }
    }

    // Helper to dump ICE stats for a transport
    async function dumpTransportStats(transport: msTypes.Transport, label: string) {
      try {
        const stats = await transport.getStats()
        stats.forEach((report: Record<string, unknown>) => {
          if (
            report.type === 'candidate-pair' ||
            report.type === 'local-candidate' ||
            report.type === 'remote-candidate'
          ) {
            console.log(`[conference] ${label} stats:`, report)
          }
        })
      } catch { /* stats not available */ }
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
      console.log('[conference] sendTransport connectionState:', state)
      if (state === 'connecting' || state === 'failed' || state === 'disconnected') {
        // Dump stats after a brief delay to let candidates gather
        setTimeout(() => dumpTransportStats(st, 'sendTransport'), state === 'connecting' ? 3000 : 0)
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
        // FR-127 P2 — the camera's capture size rides the produce, so the
        // server's layer map is relative to what the camera opened at.
        const capture = appData?.capture as { w: number; h: number } | undefined
        ws.send('media:produce', {
          room_id: rid,
          kind,
          rtp_parameters: rtpParameters,
          source: appData?.source || (kind === 'audio' ? 'audio' : 'camera'),
          ...(capture ? { capture } : {}),
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
      if (state === 'connecting' || state === 'failed' || state === 'disconnected') {
        setTimeout(() => dumpTransportStats(rt, 'recvTransport'), state === 'connecting' ? 3000 : 0)
      }
    })

    rt.on('connect', ({ dtlsParameters }, callback) => {
      ws.send('media:connect_transport', {
        room_id: rid,
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
    // FR-127 P1 — tell the server what is on screen right now: nothing, on a
    // fresh page; the tiles still mounted, on a re-join. The server holds a
    // new view client's video paused until a report names it.
    scheduleView()
    startViewResend()
  }

  async function produceLocalMedia() {
    const stream = await navigator.mediaDevices.getUserMedia({
      audio: selectedAudioDeviceId.value ? { deviceId: { exact: selectedAudioDeviceId.value } } : true,
      video: selectedVideoDeviceId.value ? { deviceId: { exact: selectedVideoDeviceId.value } } : true,
    })
    localStream.value = stream

    if (!sendTransport.value) return

    const audioTrack = stream.getAudioTracks()[0]
    if (audioTrack) {
      const audioProducer = await sendTransport.value.produce({
        track: audioTrack,
        appData: { source: 'audio' },
        // FR-127 P1 — Opus DTX when the server asks for it: a microphone that
        // hears silence then sends about one packet every 400 ms instead of
        // fifty a second, and the SFU forwards exactly what it gets. Fifty
        // people unmuted and quiet cost more packets than one worker carries
        // without this (the P0 arithmetic).
        ...(meeting.value.audioDtx ? { codecOptions: { opusDtx: true } } : {}),
      })
      producers.set('audio', audioProducer)
    }

    const videoTrack = stream.getVideoTracks()[0]
    if (videoTrack) {
      // FR-127 P2 — the camera publishes the server's ladder (two VP8 layers
      // relative to the capture, three temporal layers each) when the server
      // announced one, and tells it the capture size the ladder is relative
      // to. A browser that refuses the ladder (a handler without simulcast,
      // an encoder that cannot scale) publishes one encoding, as before: the
      // layers are a budget lever, never a reason to have no camera.
      const capture = captureSize(videoTrack)
      const appData = { source: 'camera', ...(capture ? { capture } : {}) }
      const encodings = meeting.value.simulcast
      let videoProducer: msTypes.Producer
      try {
        videoProducer = await sendTransport.value.produce({
          track: videoTrack,
          appData,
          // A copy: mediasoup-client writes `rid`s into the array it is given.
          ...(encodings ? { encodings: encodings.map((e) => ({ ...e })) } : {}),
        })
      } catch (err) {
        if (!encodings) throw err
        console.warn('[conference] the simulcast ladder was refused; publishing one encoding:', err)
        videoProducer = await sendTransport.value.produce({ track: videoTrack, appData })
      }
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

    // FR-30 P4 — seed from the subscription. A pause that happened before we
    // joined has no event for us to hear, so without this the badge only ever
    // appears after the peer's NEXT toggle.
    const startsPaused = result.producer_paused === true

    consumers.set(consumer.id, consumer)

    const streamKey = source === 'screen' ? `${connectionId}:screen` : connectionId
    consumerStreamKey.set(consumer.id, streamKey)
    if (result.kind === 'video') {
      remoteVideoPaused.set(streamKey, startsPaused)
      // FR-127 P1 — the tile may be on screen already (its audio consumer put
      // the stream on the page first, and a tile does not re-mount when its
      // stream gains a track): report again, so this producer joins the view
      // and the server resumes it.
      videoProducerByStreamKey.set(streamKey, result.producer_id)
      scheduleView()
    } else {
      remoteAudioPaused.set(streamKey, startsPaused)
    }

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
    remoteVideoPaused.delete(connectionId)
    remoteVideoPaused.delete(`${connectionId}:screen`)
    remoteAudioPaused.delete(connectionId)
    remoteAudioPaused.delete(`${connectionId}:screen`)
    videoProducerByStreamKey.delete(connectionId)
    videoProducerByStreamKey.delete(`${connectionId}:screen`)
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
        if (key && videoProducerByStreamKey.get(key) === data.producer_id) {
          videoProducerByStreamKey.delete(key)
        }
        if (key && key.endsWith(':screen')) {
          remoteStreams.delete(key)
        }
        break
      }
    }
  }

  /** FR-30 — a peer told us their camera/mic went off (or came back). */
  function handleProducerPaused(data: {
    producer_id: string
    user_id: string
    paused: boolean
  }) {
    for (const [id, consumer] of consumers) {
      if (consumer.producerId !== data.producer_id) continue
      const key = consumerStreamKey.get(id)
      if (!key) break
      // Video decides who is SHOWN; audio only decorates the tile. Same wire,
      // two maps, so a muted-but-visible peer is not confused with a hidden one.
      if (consumer.kind === 'video') remoteVideoPaused.set(key, data.paused)
      else remoteAudioPaused.set(key, data.paused)
      break
    }
  }

  /** FR-30 — tell the room, so peers can hide/annotate our tile. */
  function signalProducerPaused(kind: 'audio' | 'video', paused: boolean) {
    const producer = producers.get(kind)
    if (!producer || !roomId.value) return
    const ws = useWsStore()
    ws.send('media:producer_pause', {
      room_id: roomId.value,
      producer_id: producer.id,
      paused,
    })
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
    signalProducerPaused('audio', isMuted.value)
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
    signalProducerPaused('video', !isVideoOn.value)
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
    remoteVideoPaused.clear()
    remoteAudioPaused.clear()
    device.value = null
    consumeChain = Promise.resolve()

    // FR-127 P1 — the view belongs to the call that just ended. `viewSeq`
    // stays: it only ever grows.
    stopViewResend()
    if (viewTimer) {
      clearTimeout(viewTimer)
      viewTimer = null
    }
    tilesOnScreen.clear()
    videoProducerByStreamKey.clear()
    meeting.value = { view: false, audioDtx: false, simulcast: null }
    reportsView = false

    // Remove WS listeners
    ws.offMediaMessage('media:new_producer')
    ws.offMediaMessage('media:peer_left')
    ws.offMediaMessage('media:producer_closed')
    ws.offMediaMessage('media:producer_paused')

    // Reset state
    isInCall.value = false
    isMuted.value = false
    isVideoOn.value = true
    isScreenSharing.value = false
    tenantId.value = null
    roomId.value = null
    roomName.value = null
  }

  // ============ FR-127 P1 — receive what is visible ============

  /**
   * A tile came on screen or left it (`VideoTile`'s `visible`, forwarded by
   * every layout with the tile's stream key). The set on screen is what the
   * next `media:view` carries.
   */
  function reportTileVisible(streamKey: string, report: TileVisibility) {
    const cur = tilesOnScreen.get(streamKey)
    if (report.resized) {
      // FR-127 P2 — a tile already on screen changed size: the size (and
      // whether it is spotlit) moves, the count of reporters does not; a
      // tile that is not on screen has nothing to resize.
      if (!cur || !report.visible) return
      tilesOnScreen.set(streamKey, { ...cur, w: report.w, h: report.h, spotlit: report.spotlit === true })
    } else if (report.visible) {
      tilesOnScreen.set(streamKey, {
        n: (cur?.n ?? 0) + 1,
        w: report.w,
        h: report.h,
        spotlit: report.spotlit === true,
      })
    } else if (cur) {
      if (cur.n <= 1) tilesOnScreen.delete(streamKey)
      else tilesOnScreen.set(streamKey, { ...cur, n: cur.n - 1 })
    }
    scheduleView()
  }

  /**
   * Send the view soon. One pending send at a time, armed by the first change
   * and reading the state as it is when it fires: a scroll keeps reporting
   * every 100 ms while it lasts, and a layout change that touches sixteen
   * tiles costs one message. Nothing is sent while the server has not asked
   * (neither `meeting.view` nor `meeting.simulcast`) or outside a call.
   */
  function scheduleView() {
    if (!reportsView || !roomId.value) return
    if (viewTimer) return
    viewTimer = setTimeout(() => {
      viewTimer = null
      sendView()
    }, VIEW_DEBOUNCE_MS)
  }

  function sendView() {
    if (!reportsView || !roomId.value) return
    const visible = new Map<string, TileVisibility>()
    for (const [key, t] of tilesOnScreen) {
      visible.set(key, { visible: t.n > 0, w: t.w, h: t.h, spotlit: t.spotlit })
    }
    const video = buildViewReport(visible, videoProducerByStreamKey)
    viewSeq += 1
    useWsStore().send('media:view', { room_id: roomId.value, seq: viewSeq, video })
  }

  function onVisibilityChange() {
    if (typeof document !== 'undefined' && document.visibilityState === 'visible') scheduleView()
  }

  function startViewResend() {
    stopViewResend()
    if (!reportsView) return
    viewResend = setInterval(() => sendView(), VIEW_RESEND_MS)
    if (typeof document !== 'undefined') {
      document.addEventListener('visibilitychange', onVisibilityChange)
    }
  }

  function stopViewResend() {
    if (viewResend) {
      clearInterval(viewResend)
      viewResend = null
    }
    if (typeof document !== 'undefined') {
      document.removeEventListener('visibilitychange', onVisibilityChange)
    }
  }

  /** Is this stream key on screen by at least one reporter? (The tests read it.) */
  function isTileOnScreen(streamKey: string): boolean {
    return (tilesOnScreen.get(streamKey)?.n ?? 0) > 0
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
    remoteVideoPaused,
    remoteAudioPaused,
    meeting,
    localStream,
    isMuted,
    isVideoOn,
    isScreenSharing,

    // Device selection
    availableDevices,
    selectedAudioDeviceId,
    selectedVideoDeviceId,

    // Active speaker
    audioLevels,
    activeSpeakerKey,

    // Methods
    enumerateDevices,
    joinRoom,
    produceLocalMedia,
    leaveRoom,
    toggleMute,
    toggleVideo,
    startScreenShare,
    stopScreenShare,
    startActiveSpeaker,
    stopActiveSpeaker,
    reportTileVisible,
    isTileOnScreen,
  }
})
