// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// FR-135 R3 — the data-channel worker on both arms of `roomler-rc-dc-worker`,
// in a real Chromium: frames encoded by `VideoEncoder` on one peer connection,
// sent over a looped `video-bytes` channel framed as the agent frames them,
// and decoded by the real `rc-dc-worker.ts` behind the other (`dcworker.ts`).
//
// What only a browser can say, and so what this proves:
//   - the browser takes the channel's transfer, in the task that made it, and
//     the page is left a closed husk;
//   - the moved channel's open, messages and close reach the worker, and every
//     frame decodes with the main thread out of the path;
//   - with the channel in the worker there is no page hop: `fwd` counts
//     nothing while every arrival is still counted, which is how a read tells
//     the two arms apart (§11.14.1);
//   - a refused transfer leaves the channel on the page, whole, and the relay
//     carries it.
// What it does not: whether the move is FASTER. That is S11's question, on a
// real session at 60 Mbit/s or more (FR-135 §8.2), and why the default is off.
import { test, expect, type Page } from '@playwright/test'
import type { HarnessArm, HarnessResult } from './dcworker'

// Two peer connections in one page find each other by their host candidates;
// Chromium hides those behind mDNS names unless told not to.
test.use({ launchOptions: { args: ['--disable-features=WebRtcHideLocalIpsWithMdns'] } })

async function open(page: Page) {
  await page.goto('/e2e-harness/dcworker.html')
  await page.waitForFunction(() => window.__dcHarness?.ready === true)
}

async function canRun(page: Page): Promise<boolean> {
  return page.evaluate(async () => {
    if (typeof RTCPeerConnection !== 'function' || typeof VideoEncoder !== 'function') return false
    for (const codec of ['vp09.00.10.08', 'avc1.42001f', 'av01.0.04M.08']) {
      const dec = await VideoDecoder.isConfigSupported({ codec }).catch(() => null)
      const enc = await VideoEncoder.isConfigSupported({ codec, width: 320, height: 180, bitrate: 2_000_000, framerate: 30 }).catch(
        () => null,
      )
      if (dec?.supported && enc?.supported) return true
    }
    return false
  })
}

async function run(page: Page, arm: HarnessArm): Promise<HarnessResult> {
  const errors: string[] = []
  page.on('pageerror', (e) => errors.push(e.message))
  const result = await page.evaluate((a) => window.__dcHarness.run(a), arm)
  expect(errors, 'the page threw').toEqual([])
  expect(result.errors, 'the worker or the encoder failed').toEqual([])
  // The picture the agent sent, not the decoder's buffer: Chromium's software
  // VP9 decoder reports a coded width 64 px wider than the picture.
  expect(result.firstFrame, 'the painted size').toEqual({ width: 320, height: 180 })
  return result
}

const sum = (xs: number[]) => xs.reduce((a, b) => a + b, 0)

test.describe('FR-135 R3 — the data-channel worker, on both arms', () => {
  test.beforeEach(async ({ page }) => {
    await open(page)
    test.skip(!(await canRun(page)), 'no WebRTC, or no codec this browser both encodes and decodes')
  })

  test('the relay: the page forwards each message, and the hop is timed', async ({ page }) => {
    const r = await run(page, 'page')
    expect(r.refused).toBe('off')
    expect(r.pageChannelAfterMove).not.toBe('closed')
    expect(r.framesSent).toBeGreaterThan(30)
    expect(r.framesDecoded).toBe(r.framesSent)
    expect(r.windows.length).toBeGreaterThan(0)
    expect(sum(r.windows.map((w) => w.fwd)), 'a page hop per message').toBeGreaterThan(0)
    expect(r.channelEvents).toEqual(['open', 'closed'])
  })

  test('the channel in the worker: the browser takes it, and every frame decodes off it', async ({ page }) => {
    const r = await run(page, 'worker')
    expect(r.refused, 'this Chromium refused to transfer an RTCDataChannel').toBeNull()
    expect(r.pageChannelAfterMove, 'the page keeps a closed husk').toBe('closed')
    expect(r.framesSent).toBeGreaterThan(30)
    expect(r.framesDecoded).toBe(r.framesSent)
    expect(r.windows.length).toBeGreaterThan(0)
    expect(sum(r.windows.map((w) => w.fwd)), 'no page hop').toBe(0)
    expect(sum(r.windows.map((w) => w.arrGap)), 'every arrival counted').toBeGreaterThan(20)
    expect(r.windows.some((w) => typeof w.queueArrMs === 'number'), 'the arrival reading').toBe(true)
    expect(r.channelEvents, "the far end's close reaches the worker").toEqual(['open', 'closed'])
  })

  test('a refused transfer: the channel stays on the page, whole, and the relay carries it', async ({ page }) => {
    const r = await run(page, 'refused')
    expect(r.refused).toMatch(/^DataCloneError/)
    expect(r.pageChannelAfterMove).not.toBe('closed')
    expect(r.framesDecoded).toBe(r.framesSent)
    expect(sum(r.windows.map((w) => w.fwd))).toBeGreaterThan(0)
    expect(r.channelEvents).toEqual(['open', 'closed'])
  })
})
