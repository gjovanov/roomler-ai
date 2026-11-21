// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * #4121: a folder listing too large for one message reaches the drawer in
 * pages. The agent answers a listing that fits as one whole `files:dir-list`,
 * as before; one that does not, asked with `paged: true`, as N frames on the
 * request's `req_id`, each carrying `page` (from 0), `pages` and `total`, and
 * the last one `capped` when the folder held more than the agent lists.
 *
 * Driven against the module the composable uses (`remoteDirListing.ts`), with
 * a fake files channel and fake timers.
 */
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { createDirListing, createLatest, type DirEntry } from '@/composables/remoteDirListing'

/** A frame as the agent sends it. */
type Frame = {
  t: string
  req_id: string
  path?: string
  parent?: string | null
  entries?: DirEntry[]
  message?: string
  page?: number
  pages?: number
  total?: number
  capped?: boolean
}

type Outcome<T> = { state: 'pending' | 'resolved' | 'rejected'; value?: T; error?: Error }

function track<T>(p: Promise<T>): Outcome<T> {
  const out: Outcome<T> = { state: 'pending' }
  p.then(
    (v) => {
      out.state = 'resolved'
      out.value = v
    },
    (e: unknown) => {
      out.state = 'rejected'
      out.error = e instanceof Error ? e : new Error(String(e))
    },
  )
  return out
}

const settle = () => vi.advanceTimersByTimeAsync(0)

function fakeChannel() {
  return {
    readyState: 'open',
    sent: [] as string[],
    send(data: string) {
      this.sent.push(data)
    },
  }
}

/** A listing over a fake channel; `dirPages` is whether the agent advertised
 *  `dir-pages` in its hello. */
function setup(opts: { dirPages?: boolean } = {}) {
  const ch = fakeChannel()
  const dirPages = opts.dirPages ?? true
  const deps = { channels: { files: ch }, hasDirPages: () => dirPages }
  const dl = createDirListing(deps)
  return { ch, dl }
}

function reqIdOf(ch: { sent: string[] }, i: number): string {
  return String(JSON.parse(ch.sent[i]).req_id)
}

function entries(n: number, prefix = 'f'): DirEntry[] {
  return Array.from({ length: n }, (_, i) => ({
    name: `${prefix}${String(i).padStart(5, '0')}`,
    is_dir: false,
    size: 0,
    mtime_unix: null,
  }))
}

function split<T>(all: T[], size: number): T[][] {
  const out: T[][] = []
  for (let i = 0; i < all.length; i += size) out.push(all.slice(i, i + size))
  return out
}

function whole(reqId: string, path: string, list: DirEntry[], extra: { capped?: boolean } = {}): Frame {
  return { t: 'files:dir-list', req_id: reqId, path, parent: '/home/p', entries: list, ...extra }
}

function pageOf(
  reqId: string,
  path: string,
  list: DirEntry[],
  page: number,
  pages: number,
  total: number,
  capped?: boolean,
): Frame {
  const f: Frame = { t: 'files:dir-list', req_id: reqId, path, parent: '/home/p', entries: list, page, pages, total }
  if (capped !== undefined) f.capped = capped
  return f
}

/** What a listing says about the folder's size, read without naming a field
 *  the listing's type may lack. */
function cappedOf(listing: object | undefined): unknown {
  return listing ? (listing as Record<string, unknown>).capped : undefined
}

describe('remoteDirListing: a large folder in pages (#4121)', () => {
  beforeEach(() => {
    vi.useFakeTimers()
  })
  afterEach(() => {
    vi.clearAllTimers()
    vi.useRealTimers()
  })

  it('VR1 pages of one req_id join into one listing, each request its own', async () => {
    const { ch, dl } = setup()
    const big = track(dl.listDir('/home/p/w23-1500'))
    const other = track(dl.listDir('/home/p/w23-other'))
    const bigId = reqIdOf(ch, 0)
    const otherId = reqIdOf(ch, 1)
    const bigAll = entries(1500)
    const otherAll = entries(300, 'o')
    const bigPages = split(bigAll, 228)
    const otherPages = split(otherAll, 150)
    expect(bigPages.length).toBe(7)
    // The two listings' pages interleave on the one channel.
    for (let i = 0; i < bigPages.length; i++) {
      dl.onFilesMessage(pageOf(bigId, '/home/p/w23-1500', bigPages[i], i, bigPages.length, bigAll.length))
      if (i < otherPages.length) {
        dl.onFilesMessage(pageOf(otherId, '/home/p/w23-other', otherPages[i], i, otherPages.length, otherAll.length))
      }
    }
    await settle()
    expect(big.state).toBe('resolved')
    const got = big.value?.entries.length ?? 0
    expect(got, `${got} of 1 500 entries (#4121)`).toBe(1500)
    expect(big.value?.entries.map((e) => e.name)).toEqual(bigAll.map((e) => e.name))
    expect(big.value?.path).toBe('/home/p/w23-1500')
    expect(big.value?.parent).toBe('/home/p')
    expect(other.state).toBe('resolved')
    expect(other.value?.entries.map((e) => e.name)).toEqual(otherAll.map((e) => e.name))
  })

  it('VR2a pages 3 s apart resolve: each page re-arms the 5 s wait', async () => {
    const { ch, dl } = setup()
    const out = track(dl.listDir('/home/p/w23-slow'))
    const id = reqIdOf(ch, 0)
    const all = entries(30)
    const parts = split(all, 10)
    for (let i = 0; i < parts.length; i++) {
      await vi.advanceTimersByTimeAsync(3_000)
      expect(out.state, `settled (${out.state}) before page ${i + 1} of 3 arrived (#4121)`).toBe('pending')
      dl.onFilesMessage(pageOf(id, '/home/p/w23-slow', parts[i], i, parts.length, all.length))
      await settle()
    }
    expect(out.state, `after the last page, 9 s from the request: ${out.state} (#4121)`).toBe('resolved')
    expect(out.value?.entries.map((e) => e.name)).toEqual(all.map((e) => e.name))
  })

  it('VR2b 5 s of silence after a page rejects, naming the folder', async () => {
    const { ch, dl } = setup()
    const out = track(dl.listDir('/home/p/w23-stalls'))
    const id = reqIdOf(ch, 0)
    await vi.advanceTimersByTimeAsync(1_000)
    dl.onFilesMessage(pageOf(id, '/home/p/w23-stalls', entries(10), 0, 3, 30))
    // 5.9 s from the request, 4.9 s after the page: still waiting.
    await vi.advanceTimersByTimeAsync(4_900)
    expect(out.state, `settled (${out.state}) on page 1 of 3, before 5 s of silence (#4121)`).toBe('pending')
    await vi.advanceTimersByTimeAsync(100)
    expect(out.state, `5 s of silence after a page: ${out.state} (#4121)`).toBe('rejected')
    expect(out.error?.message).toContain('/home/p/w23-stalls')
  })

  it('VR3a the timeout names the folder; without dir-pages, the host needs an update', async () => {
    const { dl } = setup({ dirPages: false })
    const out = track(dl.listDir('/home/p/w23-1500'))
    await vi.advanceTimersByTimeAsync(4_999)
    expect(out.state).toBe('pending')
    await vi.advanceTimersByTimeAsync(1)
    expect(out.state).toBe('rejected')
    const words = out.error?.message ?? ''
    expect(words, `the timeout says "${words}" (#4121)`).toContain('/home/p/w23-1500')
    expect(words).not.toContain('may not support remote browse')
    expect(words).toMatch(/update/i)
  })

  it('VR3b the timeout names the folder; with dir-pages, no update words', async () => {
    const { dl } = setup({ dirPages: true })
    const out = track(dl.listDir('C:\\Users\\p\\w23-1500'))
    await vi.advanceTimersByTimeAsync(5_000)
    expect(out.state).toBe('rejected')
    const words = out.error?.message ?? ''
    expect(words, `the timeout says "${words}" (#4121)`).toContain('C:\\Users\\p\\w23-1500')
    expect(words).not.toContain('may not support remote browse')
    expect(words).not.toMatch(/update/i)
  })

  it('VR4 the request asks for pages', () => {
    const { ch, dl } = setup()
    dl.listDir('/home/p').catch(() => {})
    expect(ch.sent.length).toBe(1)
    const sent: unknown = JSON.parse(ch.sent[0])
    expect(sent, `the request ${ch.sent[0]} does not ask for pages (#4121)`).toEqual({
      t: 'files:dir',
      req_id: expect.any(String),
      path: '/home/p',
      paged: true,
    })
  })

  it('VR5 a capped listing says so, from its last page or a whole frame', async () => {
    const { ch, dl } = setup()
    const paged = track(dl.listDir('/home/p/w23-10001'))
    const wholeOne = track(dl.listDir('/home/p/w23-small-capped'))
    const plain = track(dl.listDir('/home/p/w23-plain'))
    const [pagedId, wholeId, plainId] = [0, 1, 2].map((i) => reqIdOf(ch, i))
    const all = entries(20)
    const [p0, p1] = split(all, 10)
    dl.onFilesMessage(pageOf(pagedId, '/home/p/w23-10001', p0, 0, 2, 20))
    dl.onFilesMessage(pageOf(pagedId, '/home/p/w23-10001', p1, 1, 2, 20, true))
    dl.onFilesMessage(whole(wholeId, '/home/p/w23-small-capped', entries(5), { capped: true }))
    dl.onFilesMessage(whole(plainId, '/home/p/w23-plain', entries(5)))
    await settle()
    expect(paged.state).toBe('resolved')
    expect(cappedOf(paged.value), `a capped listing does not say so: capped = ${String(cappedOf(paged.value))} (#4121)`).toBe(true)
    expect(paged.value?.entries.length).toBe(20)
    expect(wholeOne.state).toBe('resolved')
    expect(cappedOf(wholeOne.value), `a capped whole frame does not say so (#4121)`).toBe(true)
    expect(plain.state).toBe('resolved')
    expect(cappedOf(plain.value)).toBeFalsy()
  })

  it('VGa a missing page rejects, naming the folder', async () => {
    const { ch, dl } = setup()
    const out = track(dl.listDir('/home/p/w23-gap'))
    const id = reqIdOf(ch, 0)
    const [p0, , p2] = split(entries(30), 10)
    dl.onFilesMessage(pageOf(id, '/home/p/w23-gap', p0, 0, 3, 30))
    dl.onFilesMessage(pageOf(id, '/home/p/w23-gap', p2, 2, 3, 30))
    await settle()
    const said = out.state === 'resolved' ? `resolved short, ${out.value?.entries.length} of 30 entries` : out.state
    expect(out.state, `a listing missing page 2 of 3: ${said} (#4121)`).toBe('rejected')
    expect(out.error?.message).toContain('/home/p/w23-gap')
  })

  it('VGb a joined count other than total rejects, naming the folder', async () => {
    const { ch, dl } = setup()
    const out = track(dl.listDir('/home/p/w23-short'))
    const id = reqIdOf(ch, 0)
    const [p0, p1] = split(entries(20), 10)
    dl.onFilesMessage(pageOf(id, '/home/p/w23-short', p0, 0, 2, 25))
    dl.onFilesMessage(pageOf(id, '/home/p/w23-short', p1, 1, 2, 25))
    await settle()
    const said = out.state === 'resolved' ? `resolved short, ${out.value?.entries.length} of 25 entries` : out.state
    expect(out.state, `a listing of 20 entries whose total is 25: ${said} (#4121)`).toBe('rejected')
    expect(out.error?.message).toContain('/home/p/w23-short')
  })

  it('VD4 of two navigations, the first answered last, only the latest applies', async () => {
    const { ch, dl } = setup()
    const latest = createLatest()
    let shown = ''
    // The drawer's `navigateTo`, as `RemoteControl.vue` uses the helper.
    const navigate = async (path: string) => {
      const isCurrent = latest.begin()
      try {
        const listing = await dl.listDir(path)
        if (isCurrent()) shown = listing.path
      } catch {
        // Not under test here.
      }
    }
    const first = navigate('/home/p/slow')
    const second = navigate('/home/p/fast')
    const slowId = reqIdOf(ch, 0)
    const fastId = reqIdOf(ch, 1)
    dl.onFilesMessage(whole(fastId, '/home/p/fast', entries(1)))
    await second
    dl.onFilesMessage(whole(slowId, '/home/p/slow', entries(1)))
    await first
    expect(shown, `the first navigation, answered last, replaced the latest: the drawer shows ${shown} (#4121)`).toBe(
      '/home/p/fast',
    )
  })

  it('VC1 a whole frame resolves at once', async () => {
    const { ch, dl } = setup()
    const out = track(dl.listDir('/home/p/small'))
    const list = entries(4)
    dl.onFilesMessage(whole(reqIdOf(ch, 0), '/home/p/small', list))
    await settle()
    expect(out.state).toBe('resolved')
    expect(out.value?.path).toBe('/home/p/small')
    expect(out.value?.parent).toBe('/home/p')
    expect(out.value?.entries).toEqual(list)
    expect(cappedOf(out.value)).toBeFalsy()
  })

  it("VC2 a files:dir-error rejects with the agent's words", async () => {
    const { ch, dl } = setup()
    const out = track(dl.listDir('/home/p/w23-1500'))
    const words = '/home/p/w23-1500 holds 1500 entries, 271 KB as a listing: too many for this viewer to receive at once'
    const err: Frame = { t: 'files:dir-error', req_id: reqIdOf(ch, 0), message: words }
    expect(dl.onFilesMessage(err)).toBe(true)
    await settle()
    expect(out.state).toBe('rejected')
    expect(out.error?.message).toBe(words)
  })

  it('VC3 an answer to an unknown req_id is ignored, and other kinds are not a listing', async () => {
    const { ch, dl } = setup()
    const out = track(dl.listDir('/home/p/mine'))
    expect(dl.onFilesMessage(whole('ls-not-mine', '/elsewhere', entries(2)))).toBe(true)
    expect(dl.onFilesMessage(pageOf('ls-not-mine', '/elsewhere', entries(2), 0, 2, 4))).toBe(true)
    const other: Frame = { t: 'files:begin', req_id: reqIdOf(ch, 0) }
    expect(dl.onFilesMessage(other)).toBe(false)
    await settle()
    expect(out.state).toBe('pending')
    dl.onFilesMessage(whole(reqIdOf(ch, 0), '/home/p/mine', entries(3)))
    await settle()
    expect(out.state).toBe('resolved')
    expect(out.value?.entries.length).toBe(3)
  })

  it('VC4 a lone navigation applies', async () => {
    const latest = createLatest()
    const isCurrent = latest.begin()
    await settle()
    expect(isCurrent()).toBe(true)
  })
})
