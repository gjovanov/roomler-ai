// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// FR-135 R3 — the data-channel decode worker in a REAL browser, on both arms of
// `roomler-rc-dc-worker`: the page relaying each channel message to the worker
// (the default, and the fallback), and the channel moved into the worker
// itself. jsdom has no RTCPeerConnection, no transferable channel and no
// WebCodecs, so the unit specs cannot tell whether a browser takes the
// transfer, whether the moved channel's events reach the worker, or whether
// frames still decode once the main thread is out of the path.
//
// The page runs two peer connections looped in one window. The "viewer" side
// opens `video-bytes` and starts the real `rc-dc-worker.ts`, exactly as
// `startDcPath` does: the channel goes through the real
// `moveVideoChannelToWorker` when the arm asks for it, and the relay below is
// `startDcPath`'s. The "agent" side encodes a moving picture with
// `VideoEncoder` and sends it framed as the agent frames it (13-byte header,
// 16 KiB messages). No agent, no server.
//
// `window.__dcHarness.run(arm)` is the spec's handle; it resolves with what
// the worker said.
import { moveVideoChannelToWorker } from '@/workers/rc-video-channel'

export type HarnessArm = 'page' | 'worker' | 'refused'

export interface HarnessWindow {
  fwd: number
  arrGap: number
  queueMs: number | null
  queueArrMs: number | null
  framesDecodedTotal: number
}

export interface HarnessResult {
  arm: HarnessArm
  codec: string
  /** Why the channel stayed on the page, or null when the worker has it. */
  refused: string | null
  /** The page's own channel object right after the move: a moved channel
   *  leaves a closed husk behind. */
  pageChannelAfterMove: string
  framesSent: number
  framesDecoded: number
  /** The size the worker reported painting (`first-frame`): the picture, not
   *  a decoder's padded buffer. */
  firstFrame: { width: number; height: number } | null
  windows: HarnessWindow[]
  /** What the viewer side heard about the channel: `open`, `closed`. */
  channelEvents: string[]
  errors: string[]
}

const HEADER_BYTES = 13
const CHUNK_BYTES = 16 * 1024
const W = 320
const H = 180

/** The first codec this browser both encodes and decodes. */
async function pickCodec(): Promise<string | null> {
  for (const codec of ['vp09.00.10.08', 'avc1.42001f', 'av01.0.04M.08']) {
    const dec = await VideoDecoder.isConfigSupported({ codec }).catch(() => null)
    if (!dec?.supported) continue
    const enc = await VideoEncoder.isConfigSupported({ codec, width: W, height: H, bitrate: 2_000_000, framerate: 30 }).catch(
      () => null,
    )
    if (enc?.supported) return codec
  }
  return null
}

/** One encoded frame as `frame_video_bytes` writes it: u32 size, u8 flags
 *  (bit 0: key), u64 timestamp in µs, payload. */
function frameBytes(payload: Uint8Array, key: boolean, timestampUs: number): Uint8Array {
  const out = new Uint8Array(HEADER_BYTES + payload.byteLength)
  const view = new DataView(out.buffer)
  view.setUint32(0, payload.byteLength, true)
  view.setUint8(4, key ? 1 : 0)
  view.setUint32(5, timestampUs % 2 ** 32, true)
  view.setUint32(9, Math.floor(timestampUs / 2 ** 32), true)
  out.set(payload, HEADER_BYTES)
  return out
}

/** A picture that moves and carries some detail, so frames span messages. */
function picture(i: number): VideoFrame {
  const c = new OffscreenCanvas(W, H)
  const g = c.getContext('2d')!
  g.fillStyle = `hsl(${(i * 37) % 360} 70% 50%)`
  g.fillRect(0, 0, W, H)
  for (let k = 0; k < 400; k++) {
    g.fillStyle = (k * 7 + i) % 2 ? '#fff' : '#000'
    g.fillRect((k * 53 + i * 11) % W, (k * 29 + i * 5) % H, 6, 6)
  }
  return new VideoFrame(c, { timestamp: i * 50_000 })
}

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms))

async function waitFor(what: string, ok: () => boolean, timeoutMs: number): Promise<void> {
  const until = performance.now() + timeoutMs
  while (!ok()) {
    if (performance.now() > until) throw new Error(`timed out waiting for ${what}`)
    await sleep(20)
  }
}

async function run(arm: HarnessArm, frames = 50, fps = 20): Promise<HarnessResult> {
  const codec = await pickCodec()
  if (!codec) throw new Error('no codec this browser both encodes and decodes')
  const result: HarnessResult = {
    arm,
    codec,
    refused: null,
    pageChannelAfterMove: '',
    framesSent: 0,
    framesDecoded: 0,
    firstFrame: null,
    windows: [],
    channelEvents: [],
    errors: [],
  }

  const worker = new Worker(new URL('../src/workers/rc-dc-worker.ts', import.meta.url), { type: 'module' })
  worker.onmessage = (ev: MessageEvent) => {
    const m = ev.data as Record<string, unknown>
    if (m.type === 'first-frame') {
      result.framesDecoded = Math.max(result.framesDecoded, 1)
      result.firstFrame ??= { width: m.width as number, height: m.height as number }
    } else if (m.type === 'frame-decoded') result.framesDecoded = Math.max(result.framesDecoded, m.count as number)
    else if (m.type === 'video-channel-open') result.channelEvents.push('open')
    else if (m.type === 'video-channel-closed') result.channelEvents.push('closed')
    else if (m.type === 'decoder-error' || m.type === 'decode-error' || m.type === 'decoder-configure-error') {
      result.errors.push(`${m.type}: ${String(m.error)}`)
    } else if (m.type === 'stats') {
      const media = m.media as Record<string, number[]>
      const sum = (h: number[] | undefined) => (h ?? []).reduce((a, b) => a + b, 0)
      result.windows.push({
        fwd: sum(media.fwd),
        arrGap: sum(media.arrGap),
        queueMs: m.queueMs as number | null,
        queueArrMs: m.queueArrMs as number | null,
        framesDecodedTotal: m.framesDecodedTotal as number,
      })
    }
  }
  const synthetic = new OffscreenCanvas(2, 2)
  worker.postMessage({ type: 'init-canvas', canvas: synthetic, codec, perFrameMsg: true }, [synthetic])

  const viewerPc = new RTCPeerConnection()
  const agentPc = new RTCPeerConnection()
  viewerPc.onicecandidate = (e) => e.candidate && void agentPc.addIceCandidate(e.candidate)
  agentPc.onicecandidate = (e) => e.candidate && void viewerPc.addIceCandidate(e.candidate)
  let agentDc: RTCDataChannel | null = null
  agentPc.ondatachannel = (e) => {
    if (e.channel.label === 'video-bytes') agentDc = e.channel
  }

  // The viewer's half, as `startDcPath` does it: the channel, then in the
  // same task the move, and the relay where the move is refused or not asked.
  const dc = viewerPc.createDataChannel('video-bytes', { ordered: true })
  if (arm === 'page') {
    result.refused = 'off'
  } else {
    const target =
      arm === 'refused'
        ? {
            // A browser that cannot transfer an RTCDataChannel.
            postMessage: () => {
              throw new DOMException('RTCDataChannel object could not be cloned.', 'DataCloneError')
            },
          }
        : worker
    result.refused = moveVideoChannelToWorker(target, dc)
  }
  result.pageChannelAfterMove = dc.readyState
  if (result.refused !== null) {
    dc.binaryType = 'arraybuffer'
    dc.onmessage = (ev) => {
      if (!(ev.data instanceof ArrayBuffer)) return
      worker.postMessage({ type: 'chunk', bytes: ev.data, sentAt: performance.timeOrigin + performance.now() }, [ev.data])
    }
    dc.onopen = () => result.channelEvents.push('open')
    dc.onclose = () => result.channelEvents.push('closed')
  }

  const offer = await viewerPc.createOffer()
  await viewerPc.setLocalDescription(offer)
  await agentPc.setRemoteDescription(offer)
  const answer = await agentPc.createAnswer()
  await agentPc.setLocalDescription(answer)
  await viewerPc.setRemoteDescription(answer)
  await waitFor('the channel to open', () => agentDc?.readyState === 'open' && result.channelEvents.includes('open'), 10_000)

  const encoder = new VideoEncoder({
    output: (chunk) => {
      const payload = new Uint8Array(chunk.byteLength)
      chunk.copyTo(payload)
      // The agent's clock at framing, in µs, as `agent_epoch_us()` stamps it.
      const wire = frameBytes(payload, chunk.type === 'key', Math.round(performance.now() * 1000))
      for (let at = 0; at < wire.byteLength; at += CHUNK_BYTES) agentDc!.send(wire.slice(at, at + CHUNK_BYTES))
      result.framesSent++
    },
    error: (err) => result.errors.push(`encoder: ${err.message}`),
  })
  encoder.configure({ codec, width: W, height: H, bitrate: 2_000_000, framerate: fps, latencyMode: 'realtime' })
  for (let i = 0; i < frames; i++) {
    const f = picture(i)
    encoder.encode(f, { keyFrame: i === 0 })
    f.close()
    await sleep(1000 / fps)
  }
  await encoder.flush()
  encoder.close()
  await waitFor('every frame decoded', () => result.framesDecoded >= result.framesSent, 10_000)

  // The far end closes the channel: the viewer side must hear it, on either arm.
  agentDc!.close()
  await waitFor('the close', () => result.channelEvents.includes('closed'), 10_000)

  worker.postMessage({ type: 'close' })
  worker.terminate()
  viewerPc.close()
  agentPc.close()
  return result
}

declare global {
  interface Window {
    __dcHarness: { run: typeof run; ready: boolean }
  }
}

window.__dcHarness = { run, ready: true }
