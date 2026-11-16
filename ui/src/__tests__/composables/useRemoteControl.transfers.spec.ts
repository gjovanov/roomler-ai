// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// The viewer's file transfers, end to end through the real composable.
//
// #3725 — a folder dropped on the stage landed one level too deep. The walk
// named every file `<folder>/<folder>/…`, and the device placed exactly the
// `rel_path` it was given.
//
// #3726 — a Transfers row moved after it had settled. A late `files:progress`
// put a cancelled row back to running, the percentage went backwards in a
// row's first seconds (13 → 1 → 22 → 4 → 25 % in the field), a person's Cancel
// read "Upload failed: cancelled by operator", and a download's Cancel left its
// row running.
//
// The peer is a fake `RTCPeerConnection`. Its `files` channel counts what the
// page has queued and the transport has not taken (`bufferedAmount`), so the
// upload pump stops at its 4 MiB mark as it does in a browser; `drain` is the
// transport taking bytes. The device's replies are delivered by hand.
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { ref, watch } from 'vue'
import { createPinia, setActivePinia } from 'pinia'

vi.mock('@/api/client', () => ({ api: { get: vi.fn(async () => ({ ice_servers: [] })) } }))

import {
  drawerPasteFiles,
  isTransferCancelled,
  moveTransfer,
  uploadOutcomeMessage,
  useRemoteControl,
  type TransferMove,
  type TransferRow,
} from '@/composables/useRemoteControl'
import { useWsStore } from '@/stores/ws'

const AGENT = '69f0000000000000000000c3'
const TENANT = '69f0000000000000000000a1'
const S1 = '69f0000000000000000000d1'
const PERMS = 'VIEW | INPUT | CLIPBOARD | FILES | RECORD'
const KiB = 1024
const MiB = 1024 * KiB
/** The pump's slice, and the mark it stops queuing at (`innerPump`). */
const CHUNK = 16 * KiB

class FakeChannel {
  readyState: 'connecting' | 'open' | 'closed' = 'connecting'
  binaryType = 'blob'
  /** Bytes the page has queued and the transport has not taken yet. */
  bufferedAmount = 0
  onopen: (() => void) | null = null
  onclose: (() => void) | null = null
  onmessage: ((ev: { data: unknown }) => void) | null = null
  sent: unknown[] = []
  constructor(readonly label: string) {}
  send(d: unknown) {
    this.sent.push(d)
    // A chunk's ArrayBuffer comes from the Blob's own realm, so no `instanceof`.
    if (typeof d !== 'string') this.bufferedAmount += (d as { byteLength?: number }).byteLength ?? 0
  }
  open() {
    this.readyState = 'open'
    this.onopen?.()
  }
  close() {
    if (this.readyState === 'closed') return
    this.readyState = 'closed'
    this.onclose?.()
  }
  addEventListener() {}
  removeEventListener() {}
}

class FakePc {
  static all: FakePc[] = []
  connectionState = 'new'
  closed = false
  bytes = 0
  channels: FakeChannel[] = []
  onconnectionstatechange: (() => void) | null = null
  ontrack: unknown = null
  onicecandidate: ((ev: { candidate: unknown }) => void) | null = null
  constructor(public config: unknown) {
    FakePc.all.push(this)
  }
  createDataChannel(label: string) {
    const c = new FakeChannel(label)
    this.channels.push(c)
    return c
  }
  addTransceiver() {
    return {}
  }
  getConfiguration() {
    return this.config
  }
  setConfiguration(c: unknown) {
    this.config = c
  }
  /** RTP bytes that grow on every read: media is flowing, so the media
   *  watchdog never re-creates the session on its own. */
  async getStats() {
    this.bytes += 10_000
    return new Map([['in', { type: 'inbound-rtp', kind: 'video', bytesReceived: this.bytes, timestamp: Date.now() }]])
  }
  async createOffer() {
    return { type: 'offer', sdp: 'v=0\r\n' }
  }
  async setLocalDescription() {}
  async setRemoteDescription() {}
  async addIceCandidate() {}
  close() {
    this.closed = true
    this.connectionState = 'closed'
  }
  setState(s: string) {
    this.connectionState = s
    this.onconnectionstatechange?.()
  }
  addEventListener() {}
  removeEventListener() {}
  getReceivers() {
    return []
  }
  getTransceivers() {
    return []
  }
  getSenders() {
    return []
  }
}

type Msg = Record<string, any>
type Row = { id: string; kind: string; name: string; bytes: number; total: number | null; status: string; error?: string }

/** One page and its device. `files` is the device's `AgentCaps.files`. */
function makeRig(files: string[] = ['upload', 'download', 'download-folder', 'resume']) {
  setActivePinia(createPinia())
  const ws = useWsStore()
  const handlers = new Map<string, Set<(m: any) => void>>()
  ws.sendRaw = () => {}
  ws.onRcMessage = (t: string, h: (m: any) => void) => {
    let s = handlers.get(t)
    if (!s) {
      s = new Set()
      handlers.set(t, s)
    }
    s.add(h)
    return () => {
      handlers.get(t)?.delete(h)
    }
  }
  ws.forceRedial = vi.fn()
  ws.setTenantAffinity = vi.fn()
  ws.getDialedTid = () => TENANT
  ws.status = 'connected'
  globalThis.fetch = vi.fn(async () => ({})) as unknown as typeof fetch
  const agent = ref({ id: AGENT, capabilities: { files } } as never)
  const rc = useRemoteControl(agent, { userId: () => null })
  const pc = () => FakePc.all[FakePc.all.length - 1]!
  const filesCh = () => pc().channels.find((c) => c.label === 'files')!
  return {
    rc,
    deliver(m: Msg) {
      for (const h of [...(handlers.get(m.t) ?? [])]) h(m)
    },
    pc,
    control: () => pc().channels.find((c) => c.label === 'control')!,
    files: filesCh,
    /** A control message from the device on the `files` channel. */
    fromDevice(m: Msg) {
      filesCh().onmessage?.({ data: JSON.stringify(m) })
    },
    /** A download's binary chunk from the device. */
    bytesFromDevice(data: ArrayBuffer | Blob) {
      filesCh().onmessage?.({ data })
    },
    /** What the page sent the device on the `files` channel, by type. */
    wire(t: string): Msg[] {
      return filesCh()
        .sent.filter((s): s is string => typeof s === 'string')
        .map((s) => JSON.parse(s) as Msg)
        .filter((m) => m.t === t)
    },
    /** The transport takes `n` queued bytes off the channel. */
    drain(n: number) {
      const ch = filesCh()
      ch.bufferedAmount = Math.max(0, ch.bufferedAmount - n)
    },
    row(id: string): Row | undefined {
      return (rc.transfers.value as Row[]).find((t) => t.id === id)
    },
    /** Every state a row takes from now on, in order, repeats dropped. */
    track() {
      const states: Row[] = []
      watch(
        rc.transfers,
        (list) => {
          for (const t of list as Row[]) states.push({ ...t })
        },
        { flush: 'sync' },
      )
      return (id: string): Row[] =>
        states
          .filter((t) => t.id === id)
          .filter((t, i, all) => i === 0 || JSON.stringify(t) !== JSON.stringify(all[i - 1]))
    },
  }
}
type Rig = ReturnType<typeof makeRig>

async function flush(ms = 0) {
  await vi.advanceTimersByTimeAsync(ms)
}

/** A live session, its `files` channel open. */
async function live(r: Rig) {
  await r.rc.connect(AGENT, PERMS, false, TENANT)
  r.deliver({ t: 'rc:session.created', session_id: S1, agent_id: AGENT })
  r.deliver({ t: 'rc:ready', session_id: S1 })
  await flush()
  r.deliver({ t: 'rc:sdp.answer', session_id: S1, sdp: 'v=0\r\n' })
  await flush()
  r.pc().setState('connected')
  r.control().open()
  r.files().open()
  await flush(1_000)
  expect(r.rc.phase.value).toBe('connected')
}

/** Start one upload and let its pump run until it waits at its mark. */
async function startUpload(r: Rig, size: number, name = 'big.bin') {
  const outcome = r.rc.uploadFiles([new File([new Uint8Array(size)], name)])
  await flush()
  const begin = r.wire('files:begin').at(-1)!
  return { id: begin.id as string, outcome }
}

/** The percentages a row read, as the Transfers panel rounds them. */
const pcts = (rows: Row[]) =>
  rows.filter((t) => t.status === 'running' && t.total).map((t) => Math.round((t.bytes / (t.total as number)) * 100))
/** Where a sequence turns: its first value, every peak and dip, its last. */
function turns(xs: number[]): number[] {
  const out: number[] = []
  for (const x of xs) {
    if (out.length > 0 && out[out.length - 1] === x) continue
    if (out.length >= 2 && Math.sign(x - out[out.length - 1]) === Math.sign(out[out.length - 1] - out[out.length - 2])) {
      out[out.length - 1] = x
    } else out.push(x)
  }
  return out
}
const backwards = (xs: number[]) => xs.filter((x, i) => i > 0 && x < xs[i - 1])

// ---- the folder walk's entries, as `webkitGetAsEntry()` hands them over ----
function fileEntry(name: string, size = 3): FileSystemFileEntry {
  return {
    name,
    isFile: true,
    isDirectory: false,
    file: (ok: (f: File) => void) => ok(new File([new Uint8Array(size)], name)),
  } as unknown as FileSystemFileEntry
}
function dirEntry(name: string, children: FileSystemEntry[]): FileSystemDirectoryEntry {
  return {
    name,
    isFile: false,
    isDirectory: true,
    createReader: () => {
      // `readEntries` pages; the walk asks until a page comes back empty.
      const pages = [children.slice(0, 1), children.slice(1)].filter((p) => p.length > 0)
      return { readEntries: (ok: (e: FileSystemEntry[]) => void) => ok(pages.shift() ?? []) }
    },
  } as unknown as FileSystemDirectoryEntry
}

beforeEach(() => {
  vi.useFakeTimers()
  vi.setSystemTime(1_800_000_000_000)
  // No loopback probe: the page would fetch 127.0.0.1 for a relay.
  localStorage.setItem('roomler-rc-local-relay', '0')
  ;(globalThis as unknown as { RTCPeerConnection: unknown }).RTCPeerConnection = FakePc
  FakePc.all = []
})
afterEach(() => {
  delete (window as unknown as { showSaveFilePicker?: unknown }).showSaveFilePicker
  vi.useRealTimers()
  localStorage.clear()
})

describe('#3725 — a dropped folder names itself once', () => {
  it('a folder with a file and a subfolder gives <folder>/<file> and <folder>/<sub>/<file>', async () => {
    const r = makeRig()
    const drop = dirEntry('l2fold', [fileEntry('l2-one.bin'), dirEntry('sub', [fileEntry('l2-two.bin')])])
    // Both drops (the stage and the files drawer) walk with the entry's own name.
    const out = await r.rc.walkFolderEntry(drop, drop.name)
    expect(out.map((e) => e.relPath)).toEqual(['l2fold/l2-one.bin', 'l2fold/sub/l2-two.bin'])
    expect(out.map((e) => e.file.name)).toEqual(['l2-one.bin', 'l2-two.bin'])
  })

  it('every depth carries the folder name once', async () => {
    const r = makeRig()
    const drop = dirEntry('a', [dirEntry('b', [dirEntry('c', [fileEntry('deep.txt')])]), fileEntry('top.txt')])
    const out = await r.rc.walkFolderEntry(drop, drop.name)
    expect(out.map((e) => e.relPath).sort()).toEqual(['a/b/c/deep.txt', 'a/top.txt'])
  })

  it('the walked path is what `files:begin` carries as `rel_path`, for a drop on the stage and on the drawer', async () => {
    const r = makeRig()
    await live(r)
    const drop = dirEntry('l2fold', [fileEntry('l2-one.bin'), dirEntry('sub', [fileEntry('l2-two.bin')])])
    const walked = await r.rc.walkFolderEntry(drop, drop.name)
    // The stage: no destination, the device's default folder.
    const stage = r.rc.uploadFiles(walked)
    await flush()
    for (const f of walked) {
      const begin = r.wire('files:begin').at(-1)!
      expect(begin).toMatchObject({ name: f.file.name, rel_path: f.relPath })
      expect(begin).not.toHaveProperty('dest_path')
      r.fromDevice({ t: 'files:complete', id: begin.id, path: `/home/u/Downloads/${f.relPath}`, bytes: 3 })
      await flush()
    }
    await stage
    expect(r.wire('files:begin').map((b) => b.rel_path)).toEqual(['l2fold/l2-one.bin', 'l2fold/sub/l2-two.bin'])
    // The drawer: the same names, under the folder the drawer shows.
    const drawer = r.rc.uploadFiles(walked, { destPath: '/home/u/Desktop' })
    await flush()
    const begin = r.wire('files:begin').at(-1)!
    expect(begin).toMatchObject({ rel_path: 'l2fold/l2-one.bin', dest_path: '/home/u/Desktop' })
    r.rc.cancelTransfer(begin.id)
    await flush()
    r.rc.cancelTransfer(r.wire('files:begin').at(-1)!.id)
    await drawer
  })

  it('an empty subfolder sends nothing, as before: no message makes a folder on the device', async () => {
    const r = makeRig()
    const drop = dirEntry('l2fold', [dirEntry('empty', []), fileEntry('x.bin')])
    const out = await r.rc.walkFolderEntry(drop, drop.name)
    expect(out).toHaveLength(1)
    expect(out.some((e) => e.relPath.includes('empty'))).toBe(false)
    // A drop of nothing but empty folders walks to nothing (the view uploads nothing).
    expect(await r.rc.walkFolderEntry(dirEntry('hollow', [dirEntry('e', [])]), 'hollow')).toEqual([])
  })

  it('dotfiles and dot-folders stay out, as before', async () => {
    const r = makeRig()
    const drop = dirEntry('proj', [fileEntry('.env'), dirEntry('.git', [fileEntry('HEAD')]), fileEntry('a.txt')])
    const out = await r.rc.walkFolderEntry(drop, drop.name)
    expect(out.map((e) => e.file.name)).toEqual(['a.txt'])
  })

  it('a paste carries flat files: its upload names no folder (control)', async () => {
    const r = makeRig()
    await live(r)
    const pasted = new File([new Uint8Array(10)], 'pasted.txt')
    const ev = { clipboardData: { files: [pasted] }, defaultPrevented: false, target: null } as unknown as ClipboardEvent
    void r.rc.uploadFiles(drawerPasteFiles(ev, true)!)
    await flush()
    const begin = r.wire('files:begin').at(-1)!
    expect(begin.name).toBe('pasted.txt')
    expect(begin).not.toHaveProperty('rel_path')
  })
})

describe('#3726 — an upload row', () => {
  it('shows what the device has acknowledged, never bytes still queued in the browser', async () => {
    const r = makeRig()
    await live(r)
    const { id } = await startUpload(r, 31 * MiB)
    // The pump queued past its mark and waits there; the device has said nothing yet.
    expect(r.files().bufferedAmount).toBeGreaterThan(4 * MiB)
    expect(r.row(id)).toMatchObject({ status: 'running', bytes: 0 })
    r.fromDevice({ t: 'files:progress', id, bytes: 256 * KiB })
    expect(r.row(id)).toMatchObject({ status: 'running', bytes: 256 * KiB })
  })

  it('never moves backwards while the pump queues and the device acknowledges (the field read 13 → 1 → 22 → 4 → 25 %)', async () => {
    const r = makeRig()
    await live(r)
    const seen = r.track()
    const { id } = await startUpload(r, 31 * MiB)
    r.fromDevice({ t: 'files:progress', id, bytes: 256 * KiB })
    r.drain(180 * CHUNK)
    await flush(20)
    r.fromDevice({ t: 'files:progress', id, bytes: 1280 * KiB })
    r.drain(56 * CHUNK)
    await flush(20)
    const read = pcts(seen(id))
    expect(backwards(read), `the bar read ${turns(read).join(' → ')} %`).toEqual([])
    expect(r.row(id)).toMatchObject({ status: 'running', bytes: 1280 * KiB })
  })

  it('a late progress after the Cancel leaves it cancelled, and the pump sends no more', async () => {
    const r = makeRig()
    await live(r)
    const seen = r.track()
    const { id } = await startUpload(r, 16 * MiB)
    r.fromDevice({ t: 'files:accepted', id, path: '/home/u/Downloads/big.bin' })
    r.fromDevice({ t: 'files:progress', id, bytes: 3 * MiB })
    r.rc.cancelTransfer(id)
    expect(r.row(id)).toMatchObject({ status: 'cancelled' })
    // What was in flight at the Cancel is still written, and acknowledged.
    r.fromDevice({ t: 'files:progress', id, bytes: 3 * MiB + 256 * KiB })
    r.fromDevice({ t: 'files:progress', id, bytes: 3 * MiB + 512 * KiB })
    r.fromDevice({ t: 'files:accepted', id, path: '/home/u/Downloads/big.bin' })
    expect(r.row(id)).toMatchObject({ status: 'cancelled' })
    const states = seen(id)
    const at = states.findIndex((t) => t.status === 'cancelled')
    expect(states.slice(at).every((t) => t.status === 'cancelled')).toBe(true)
    r.drain(16 * MiB)
    await flush(100)
    expect(r.wire('files:end')).toEqual([])
  })

  it('an error after the person’s Cancel still reads Cancelled, and the upload reports a cancel, not a failure', async () => {
    const r = makeRig()
    await live(r)
    const { id, outcome } = await startUpload(r, 16 * MiB)
    r.rc.cancelTransfer(id)
    r.fromDevice({ t: 'files:error', id, message: 'chunk failed: no active transfer' })
    expect(r.row(id)).toMatchObject({ status: 'cancelled' })
    const results = await outcome
    expect(uploadOutcomeMessage(results, 1)).toEqual({ level: 'info', text: 'Upload cancelled: big.bin' })
  })

  it('complete, then a late progress or accept, stays complete until it leaves the list', async () => {
    const r = makeRig()
    await live(r)
    const { id, outcome } = await startUpload(r, 2 * MiB)
    // Under the mark: the whole file is queued, and its end with it.
    expect(r.wire('files:end')).toEqual([{ t: 'files:end', id }])
    r.fromDevice({ t: 'files:progress', id, bytes: 1 * MiB })
    r.fromDevice({ t: 'files:complete', id, path: '/home/u/Downloads/big.bin', bytes: 2 * MiB })
    expect(await outcome).toEqual([{ ok: true, name: 'big.bin', path: '/home/u/Downloads/big.bin', bytes: 2 * MiB }])
    r.fromDevice({ t: 'files:progress', id, bytes: Math.round(1.76 * MiB) })
    r.fromDevice({ t: 'files:accepted', id, path: '/home/u/Downloads/big.bin' })
    expect(r.row(id)).toMatchObject({ status: 'complete', bytes: 2 * MiB })
    await flush(9_000)
    expect(r.row(id)).toMatchObject({ status: 'complete' })
    await flush(1_100)
    expect(r.row(id)).toBeUndefined()
  })

  it('a failure the person did not choose keeps its failure wording (control)', async () => {
    const r = makeRig()
    await live(r)
    const { id, outcome } = await startUpload(r, 1 * MiB)
    r.fromDevice({ t: 'files:error', id, message: 'disk full' })
    expect(r.row(id)).toMatchObject({ status: 'error', error: 'disk full' })
    expect(uploadOutcomeMessage(await outcome, 1)).toEqual({ level: 'error', text: 'Upload failed: disk full' })
  })

  it('a late progress after a failure leaves it failed', async () => {
    const r = makeRig()
    await live(r)
    const { id } = await startUpload(r, 16 * MiB)
    r.fromDevice({ t: 'files:progress', id, bytes: 1 * MiB })
    r.fromDevice({ t: 'files:error', id, message: 'disk full' })
    r.fromDevice({ t: 'files:progress', id, bytes: 1280 * KiB })
    r.fromDevice({ t: 'files:accepted', id, path: '/home/u/Downloads/big.bin' })
    expect(r.row(id)).toMatchObject({ status: 'error', error: 'disk full' })
  })

  it('a row whose channel dropped runs again through the resume path, from the device’s accepted offset (control)', async () => {
    const r = makeRig()
    await live(r)
    const seen = r.track()
    const { id, outcome } = await startUpload(r, 16 * MiB)
    r.fromDevice({ t: 'files:progress', id, bytes: 2 * MiB })
    r.files().close()
    expect(r.row(id)).toMatchObject({ status: 'reconnecting' })
    await flush(20) // the pump finds the channel gone
    r.files().open()
    r.drain(16 * MiB)
    await flush(1_000) // the ladder's first wait, then the channel is seen open
    expect(r.wire('files:resume')).toEqual([{ t: 'files:resume', id, offset: 2 * MiB }])
    // The device kept less than it had acknowledged: the row starts again where the device's copy ends.
    r.fromDevice({ t: 'files:resumed', id, accepted_offset: 1792 * KiB })
    await flush()
    const states = seen(id)
    const back = states.findIndex((t, i) => i > 0 && t.status === 'running' && states[i - 1].status === 'reconnecting')
    expect(back).toBeGreaterThan(0)
    expect(states[back]).toMatchObject({ status: 'running', bytes: 1792 * KiB })
    // …and it completes.
    for (let i = 0; i < 20 && r.wire('files:end').length === 0; i++) {
      r.drain(16 * MiB)
      await flush(20)
    }
    expect(r.wire('files:end')).toEqual([{ t: 'files:end', id }])
    r.fromDevice({ t: 'files:complete', id, path: '/home/u/Downloads/big.bin', bytes: 16 * MiB })
    expect(await outcome).toMatchObject([{ ok: true, bytes: 16 * MiB }])
    expect(r.row(id)).toMatchObject({ status: 'complete' })
  })

  it('a Cancel during the resume handshake stays cancelled when the device answers late', async () => {
    const r = makeRig()
    await live(r)
    const { id, outcome } = await startUpload(r, 16 * MiB)
    r.fromDevice({ t: 'files:progress', id, bytes: 2 * MiB })
    r.files().close()
    await flush(20)
    r.files().open()
    r.drain(16 * MiB)
    await flush(1_000)
    expect(r.wire('files:resume')).toHaveLength(1)
    r.rc.cancelTransfer(id)
    expect(r.row(id)).toMatchObject({ status: 'cancelled' })
    r.fromDevice({ t: 'files:resumed', id, accepted_offset: 2 * MiB })
    await flush(100)
    expect(r.row(id)).toMatchObject({ status: 'cancelled' })
    expect(uploadOutcomeMessage(await outcome, 1).level).toBe('info')
  })
})

describe('#3726 — a download row', () => {
  /** A save target as `showSaveFilePicker` hands it over, counting its calls. */
  function saveTarget() {
    const t = { writes: 0, closed: 0, aborted: 0 }
    ;(window as unknown as { showSaveFilePicker: unknown }).showSaveFilePicker = async () => ({
      createWritable: async () => ({
        write: async () => {
          t.writes += 1
        },
        close: async () => {
          t.closed += 1
        },
        abort: async () => {
          t.aborted += 1
        },
      }),
    })
    return t
  }

  for (const [what, start, ask] of [
    ['a file', (r: Rig) => r.rc.downloadFile('/home/u/big.iso', 'big.iso'), 'files:get'],
    ['a folder', (r: Rig) => r.rc.downloadFolder('/home/u/pics', 'pics.zip'), 'files:get-folder'],
  ] as const) {
    it(`${what}: the Cancel settles it as Cancelled, closes the save target, and drops what arrives late`, async () => {
      const r = makeRig()
      await live(r)
      const save = saveTarget()
      let settled: unknown = 'pending'
      start(r).then(
        (v) => (settled = v),
        (e) => (settled = e),
      )
      await flush()
      const id = r.wire(ask).at(-1)!.id as string
      r.fromDevice({ t: 'files:offer', id, name: 'big', size: what === 'a file' ? 8 * MiB : null })
      r.bytesFromDevice(new ArrayBuffer(64 * KiB))
      r.bytesFromDevice(new ArrayBuffer(64 * KiB))
      expect(save.writes).toBe(2)
      expect(r.row(id)).toMatchObject({ status: 'running', bytes: 128 * KiB })
      r.rc.cancelTransfer(id)
      expect(r.wire('files:cancel')).toEqual([{ t: 'files:cancel', id }])
      expect(r.row(id)).toMatchObject({ status: 'cancelled' })
      await flush()
      expect(save.aborted).toBe(1)
      expect(settled).toBeInstanceOf(Error)
      expect((settled as Error).message).toMatch(/cancel/i)
      // The view words it as a cancel, never as "Download failed".
      expect(isTransferCancelled(settled)).toBe(true)
      // What the device had sent before it stopped: dropped, never written, never thrown.
      expect(() => r.bytesFromDevice(new ArrayBuffer(64 * KiB))).not.toThrow()
      expect(() => r.bytesFromDevice(new Blob([new Uint8Array(64 * KiB)]))).not.toThrow()
      r.fromDevice({ t: 'files:progress', id, bytes: 256 * KiB })
      r.fromDevice({ t: 'files:eof', id, bytes: 8 * MiB })
      r.fromDevice({ t: 'files:error', id, message: 'stopped' })
      await flush()
      expect(save.writes).toBe(2)
      expect(save.closed).toBe(0)
      expect(save.aborted).toBe(1)
      expect(r.row(id)).toMatchObject({ status: 'cancelled' })
    })
  }

  it('never moves backwards: its bytes are what the browser received, not the device’s count', async () => {
    const r = makeRig()
    await live(r)
    const seen = r.track()
    void r.rc.downloadFile('/home/u/a.bin', 'a.bin').catch(() => {})
    await flush()
    const id = r.wire('files:get').at(-1)!.id as string
    r.fromDevice({ t: 'files:offer', id, name: 'a.bin', size: 1 * MiB })
    // Chunks that arrive as Blobs are read in a later turn; the device's
    // progress for them is read at once.
    for (let i = 0; i < 4; i++) r.bytesFromDevice(new Blob([new Uint8Array(64 * KiB)]))
    r.fromDevice({ t: 'files:progress', id, bytes: 256 * KiB })
    await flush()
    const read = seen(id)
      .filter((t) => t.status === 'running')
      .map((t) => t.bytes)
    expect(backwards(read), `the row read ${read.join(' → ')} bytes`).toEqual([])
    expect(r.row(id)).toMatchObject({ status: 'running', bytes: 256 * KiB })
  })
})

describe('#3726 — moveTransfer, the one place a row moves', () => {
  const row = (status: TransferRow['status'], bytes = 1 * MiB, error?: string): TransferRow => ({
    id: 'up-1',
    kind: 'upload',
    name: 'big.bin',
    bytes,
    total: 16 * MiB,
    status,
    ...(error === undefined ? {} : { error }),
  })
  const every: TransferMove[] = [
    { t: 'start' },
    { t: 'start', name: 'other', total: 1 },
    { t: 'bytes', bytes: 8 * MiB },
    { t: 'reconnect', detail: 'attempt 2' },
    { t: 'resume', offset: 2 * MiB },
    { t: 'complete', bytes: 16 * MiB },
    { t: 'fail', error: 'disk full' },
    { t: 'cancel' },
  ]

  it('complete and cancelled are final: nothing moves them', () => {
    for (const status of ['complete', 'cancelled'] as const) {
      const settled = row(status)
      for (const move of every) expect(moveTransfer(settled, move), `${status} + ${move.t}`).toBe(settled)
    }
  })

  it('a failed row moves only through the resume path (control: it still resumes to running)', () => {
    const failed = row('error', 3 * MiB, 'files channel closed')
    for (const move of every.filter((m) => m.t !== 'resume')) {
      expect(moveTransfer(failed, move), `error + ${move.t}`).toBe(failed)
    }
    expect(moveTransfer(failed, { t: 'resume', offset: 2 * MiB })).toEqual({
      ...failed,
      status: 'running',
      bytes: 2 * MiB,
      error: undefined,
    })
  })

  it('bytes never move back while it runs, and move nothing unless it runs', () => {
    const running = row('running', 4 * MiB)
    expect(moveTransfer(running, { t: 'bytes', bytes: 1 * MiB })).toBe(running)
    expect(moveTransfer(running, { t: 'bytes', bytes: 4 * MiB })).toBe(running)
    expect(moveTransfer(running, { t: 'bytes', bytes: 5 * MiB })).toMatchObject({ status: 'running', bytes: 5 * MiB })
    for (const status of ['queued', 'reconnecting'] as const) {
      const r = row(status)
      expect(moveTransfer(r, { t: 'bytes', bytes: 8 * MiB }), status).toBe(r)
    }
  })

  it('a resume reads the device’s accepted offset, the one step back, and clears the reconnect detail', () => {
    const waiting = row('reconnecting', 4 * MiB, 'attempt 2')
    expect(moveTransfer(waiting, { t: 'resume', offset: 3 * MiB })).toEqual({
      ...waiting,
      status: 'running',
      bytes: 3 * MiB,
      error: undefined,
    })
  })

  it('start runs a queued or reconnecting row, and a download’s offer names it', () => {
    expect(moveTransfer(row('queued', 0), { t: 'start' })).toMatchObject({ status: 'running', bytes: 0 })
    expect(moveTransfer(row('reconnecting', 0, 'waiting for reconnect'), { t: 'start' })).toMatchObject({
      status: 'running',
      error: undefined,
    })
    const running = row('running')
    expect(moveTransfer(running, { t: 'start' })).toBe(running)
    expect(moveTransfer(row('queued', 0), { t: 'start', name: 'pics.zip', total: null })).toMatchObject({
      status: 'running',
      name: 'pics.zip',
      total: null,
    })
  })

  it('a live row takes its Cancel, its failure and its completion', () => {
    for (const status of ['queued', 'running', 'reconnecting'] as const) {
      expect(moveTransfer(row(status), { t: 'cancel' }), status).toMatchObject({ status: 'cancelled' })
      expect(moveTransfer(row(status), { t: 'fail', error: 'disk full' }), status).toMatchObject({
        status: 'error',
        error: 'disk full',
      })
      expect(moveTransfer(row(status), { t: 'complete', bytes: 16 * MiB }), status).toMatchObject({
        status: 'complete',
        bytes: 16 * MiB,
      })
    }
  })
})
