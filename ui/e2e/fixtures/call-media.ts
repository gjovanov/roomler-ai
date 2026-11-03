// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * What a call actually carried, read from the browser: which `<video>`s hold a
 * REMOTE track, how many frames those decoded, and which ICE pair each
 * RTCPeerConnection selected. Shared by the self-host call specs.
 *
 * ⚠️ Count remote elements only. The local self-view is also a `<video>`, and
 * it advances with no network at all, so a counter taken across every element
 * passes a call in which not one remote frame arrived.
 *
 * ⚠️ A remote track being `live` proves nothing either: mediasoup signalling
 * creates it before a single packet flows. Only its frame counter moves with
 * media.
 */
import type { BrowserContext, Page } from '@playwright/test'

export interface VideoProgress {
  frames: number
  time: number
  videos: number
}

/** One live RTCPeerConnection's selected ICE pair and what crossed it. */
export interface PeerPath {
  state: string
  localType: string | null
  relayProtocol: string | null
  localAddress: string | null
  localUrl: string | null
  remoteAddress: string | null
  remotePort: number | null
  bytesSent: number
  bytesReceived: number
  framesDecoded: number
}

/** Record the ids of tracks this page captured, so remote ones are tellable. */
export async function trackLocalCaptures(context: BrowserContext): Promise<void> {
  await context.addInitScript(() => {
    const w = window as unknown as { __selfhost_local_track_ids: string[] }
    w.__selfhost_local_track_ids = []
    const md = navigator.mediaDevices
    if (!md?.getUserMedia) return
    const orig = md.getUserMedia.bind(md)
    md.getUserMedia = async (c?: MediaStreamConstraints) => {
      const s = await orig(c)
      for (const t of s.getTracks()) w.__selfhost_local_track_ids.push(t.id)
      return s
    }
  })
}

/**
 * Keep every RTCPeerConnection the page makes. mediasoup-client owns them and
 * the app exposes no debug hook, and `getStats` on the connection itself is the
 * only place that names the candidate pair actually carrying the media.
 */
export async function recordPeerConnections(context: BrowserContext): Promise<void> {
  await context.addInitScript(() => {
    const w = window as unknown as { __selfhost_pcs: RTCPeerConnection[] }
    w.__selfhost_pcs = []
    const Native = window.RTCPeerConnection
    window.RTCPeerConnection = class extends Native {
      constructor(config?: RTCConfiguration) {
        super(config)
        w.__selfhost_pcs.push(this)
      }
    }
  })
}

/**
 * A REMOTE track is arriving: at least one `<video>` whose MediaStream carries
 * a live video track that this page did not produce.
 */
export async function remoteVideoTracks(page: Page): Promise<number> {
  return await page.evaluate(() => {
    const w = window as unknown as { __selfhost_local_track_ids?: string[] }
    const local = new Set(w.__selfhost_local_track_ids ?? [])
    let n = 0
    for (const v of Array.from(document.querySelectorAll('video'))) {
      const s = (v as HTMLVideoElement).srcObject as MediaStream | null
      if (!s) continue
      for (const t of s.getVideoTracks()) {
        if (t.readyState === 'live' && !local.has(t.id)) n++
      }
    }
    return n
  })
}

/** Decoded-frame progress across the `<video>`s that hold a remote track. */
export async function remoteVideoProgress(page: Page): Promise<VideoProgress> {
  return await page.evaluate(() => {
    const w = window as unknown as { __selfhost_local_track_ids?: string[] }
    const local = new Set(w.__selfhost_local_track_ids ?? [])
    let frames = 0
    let time = 0
    let videos = 0
    for (const v of Array.from(document.querySelectorAll('video'))) {
      const el = v as HTMLVideoElement
      const s = el.srcObject as MediaStream | null
      if (!s?.getVideoTracks().some((t) => t.readyState === 'live' && !local.has(t.id))) continue
      videos++
      const f = el.getVideoPlaybackQuality().totalVideoFrames
      if (f > frames) frames = f
      if (el.currentTime > time) time = el.currentTime
    }
    return { frames, time, videos }
  })
}

/** The selected ICE pair of every peer connection that has not been closed. */
export async function peerPaths(page: Page): Promise<PeerPath[]> {
  return await page.evaluate(async () => {
    type Report = Record<string, unknown>
    const w = window as unknown as { __selfhost_pcs?: RTCPeerConnection[] }
    const paths: PeerPath[] = []
    for (const pc of w.__selfhost_pcs ?? []) {
      if (pc.connectionState === 'closed') continue
      const stats = await pc.getStats()
      let pairId: string | undefined
      let framesDecoded = 0
      stats.forEach((r: Report) => {
        if (r.type === 'transport' && r.selectedCandidatePairId) {
          pairId = r.selectedCandidatePairId as string
        }
        if (r.type === 'inbound-rtp' && r.kind === 'video') {
          framesDecoded += (r.framesDecoded as number) ?? 0
        }
      })
      const pair = pairId ? (stats.get(pairId) as Report) : undefined
      const local = pair ? (stats.get(pair.localCandidateId as string) as Report) : undefined
      const remote = pair ? (stats.get(pair.remoteCandidateId as string) as Report) : undefined
      paths.push({
        state: pc.connectionState,
        localType: (local?.candidateType as string) ?? null,
        relayProtocol: (local?.relayProtocol as string) ?? null,
        localAddress: (local?.address as string) ?? null,
        localUrl: (local?.url as string) ?? null,
        remoteAddress: (remote?.address as string) ?? null,
        remotePort: (remote?.port as number) ?? null,
        bytesSent: (pair?.bytesSent as number) ?? 0,
        bytesReceived: (pair?.bytesReceived as number) ?? 0,
        framesDecoded,
      })
    }
    return paths
  })
}
