// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// #3582 — the build names itself once, in the shell and in /version.json, and
// the page reads both. The plugin's two outputs can never disagree, and the
// reader learns nothing from anything but the file itself (an image older
// than this answers the SPA shell for it).
import { describe, expect, it, vi } from 'vitest'
import {
  BUILD_META_NAME,
  VERSION_FILE,
  VERSION_PATH,
  isBuildId,
  pageBuildId,
  parseVersionFile,
  readServedBuild,
} from '@/utils/buildId'
import { buildIdOf, buildIdPlugin } from '../../../buildIdPlugin'

const ID = 'a8924d0890b924c2'

function shell(meta?: string): Document {
  const doc = document.implementation.createHTMLDocument('t')
  if (meta !== undefined) {
    const m = doc.createElement('meta')
    m.setAttribute('name', BUILD_META_NAME)
    m.setAttribute('content', meta)
    doc.head.appendChild(m)
  }
  return doc
}

function response(body: unknown, init: { ok?: boolean; type?: string } = {}): Response {
  return {
    ok: init.ok ?? true,
    headers: { get: (h: string) => (h.toLowerCase() === 'content-type' ? (init.type ?? 'application/json') : null) },
    json: async () => body,
  } as unknown as Response
}

describe('the page reads its own build from its shell', () => {
  it('the meta tag the build wrote', () => {
    expect(pageBuildId(shell(ID))).toBe(ID)
  })

  it('nothing under the dev server, in a test, or in a shell built before #3582', () => {
    expect(pageBuildId(shell())).toBeNull()
    expect(pageBuildId(shell('dev'))).toBeNull()
    expect(pageBuildId(undefined)).toBeNull()
    expect(pageBuildId({ querySelector: () => { throw new Error('x') } })).toBeNull()
  })
})

describe('the served build', () => {
  it('is the hex id in a well-formed file, and nothing else', () => {
    expect(parseVersionFile({ build: ID })).toBe(ID)
    expect(parseVersionFile({ build: 'ABC' })).toBeNull()
    expect(parseVersionFile({ build: 42 })).toBeNull()
    expect(parseVersionFile('<!DOCTYPE html>')).toBeNull()
    expect(parseVersionFile(null)).toBeNull()
    expect(isBuildId(ID)).toBe(true)
  })

  it('is asked for past every cache: no-store, and a query no cache has seen', async () => {
    const fetch = vi.fn(async (_u: string, _i: RequestInit) => response({ build: ID }))
    expect(await readServedBuild({ fetch, now: () => 1234 })).toBe(ID)
    const [url, init] = fetch.mock.calls[0]
    expect(url).toBe(`${VERSION_PATH}?t=1234`)
    expect(init.cache).toBe('no-store')
    expect(init.credentials).toBe('same-origin')
  })

  it("learns nothing from an older image's SPA shell, an error, or a failed request", async () => {
    // nginx answers a missing file through `location /` with the shell: 200, text/html.
    expect(await readServedBuild({ fetch: async () => response('<!DOCTYPE html>', { type: 'text/html' }) })).toBeNull()
    expect(await readServedBuild({ fetch: async () => response({ build: ID }, { ok: false }) })).toBeNull()
    expect(await readServedBuild({ fetch: async () => response({ nope: 1 }) })).toBeNull()
    expect(await readServedBuild({ fetch: async () => { throw new TypeError('Failed to fetch') } })).toBeNull()
  })
})

describe('buildIdOf — a hash of the names the build emits', () => {
  const names = ['assets/index-BrnqUnK7.js', 'assets/vuetify-Cn2s3D7O.js', 'assets/index-9ISOyB6B.css']

  it('is 16 hex characters, the same for the same build whatever the order', () => {
    const id = buildIdOf(names)
    expect(id).toMatch(/^[0-9a-f]{16}$/)
    expect(buildIdOf([...names].reverse())).toBe(id)
  })

  it('changes when any content-hashed name does', () => {
    expect(buildIdOf([...names.slice(1), 'assets/index-XXXXXXXX.js'])).not.toBe(buildIdOf(names))
  })

  it('ignores the files whose names carry no content hash: the shells, the id file, Vite metadata', () => {
    expect(buildIdOf([...names, 'index.html', VERSION_FILE, '.vite/manifest.json'])).toBe(buildIdOf(names))
  })
})

describe('buildIdPlugin — one id, in both places', () => {
  type Hook = { order?: string; handler: (...args: unknown[]) => unknown }
  const bundle = { 'assets/index-BrnqUnK7.js': {}, 'assets/index-9ISOyB6B.css': {} }

  function run(first: 'file' | 'shell') {
    const plugin = buildIdPlugin()
    const emitted: Array<{ fileName: string; source: string }> = []
    const writeFile = () =>
      (plugin.generateBundle as unknown as Hook).handler.call(
        { emitFile: (f: { fileName: string; source: string }) => emitted.push(f) },
        {},
        { ...bundle },
      )
    const writeShell = () =>
      (plugin.transformIndexHtml as unknown as Hook).handler('<html></html>', {
        bundle: { ...bundle, 'index.html': {} },
      }) as Array<{ tag: string; attrs: Record<string, string>; injectTo: string }>
    let tags: ReturnType<typeof writeShell>
    if (first === 'file') {
      writeFile()
      tags = writeShell()
    } else {
      tags = writeShell()
      writeFile()
    }
    return { plugin, emitted, tags }
  }

  it('writes version.json and the meta tag with the same id, whichever hook runs first', () => {
    for (const first of ['file', 'shell'] as const) {
      const { emitted, tags } = run(first)
      expect(emitted).toHaveLength(1)
      expect(emitted[0].fileName).toBe(VERSION_FILE)
      const id = JSON.parse(emitted[0].source).build
      expect(id).toBe(buildIdOf(Object.keys(bundle)))
      expect(tags).toEqual([{ tag: 'meta', attrs: { name: BUILD_META_NAME, content: id }, injectTo: 'head' }])
    }
  })

  it('runs in builds only, writes the file before other plugins read the bundle, and leaves the dev shell alone', () => {
    const { plugin } = run('file')
    expect(plugin.apply).toBe('build')
    expect((plugin.generateBundle as unknown as Hook).order).toBe('pre')
    expect((plugin.transformIndexHtml as unknown as Hook).handler('<html></html>', {})).toBeUndefined()
  })
})
