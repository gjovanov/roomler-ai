// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * FR-87 (#1776) — content-hashed asset names.
 *
 * nginx promises every `.css`/`.js`/image a year of `immutable` caching. The
 * promise is kept only if a name changes exactly when its bytes do.
 */
import { mkdtempSync, readFileSync, readdirSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { afterAll, describe, expect, it } from 'vitest'
import { AssetEmitter, hashedName, HASH_LEN } from '../theme/assets.ts'

const tmp = mkdtempSync(join(tmpdir(), 'fr87-assets-'))
afterAll(() => rmSync(tmp, { recursive: true, force: true }))

describe('hashedName', () => {
  it('puts the hash between the name and the extension', () => {
    expect(hashedName('docs.css', 'body{}')).toMatch(new RegExp(`^docs\\.[0-9a-f]{${HASH_LEN}}\\.css$`))
    expect(hashedName('search-index.json', '{}')).toMatch(/^search-index\.[0-9a-f]{10}\.json$/)
  })

  it('changes exactly when the bytes change', () => {
    expect(hashedName('a.js', 'x')).toBe(hashedName('a.js', 'x'))
    expect(hashedName('a.js', 'x')).not.toBe(hashedName('a.js', 'y'))
  })
})

describe('AssetEmitter', () => {
  it('writes nothing until flush — the build empties the output directory in between', () => {
    const out = join(tmp, 'deferred')
    const e = new AssetEmitter(out, '/docs/assets', false)
    const url = e.publishBytes('x.json', '{"a":1}')
    expect(url).toMatch(/^\/docs\/assets\/x\.[0-9a-f]{10}\.json$/)
    expect(() => readdirSync(out)).toThrow()
    e.flush()
    expect(readFileSync(join(out, url.split('/').pop()!), 'utf8')).toBe('{"a":1}')
  })

  it('publishes one source once, under one URL', () => {
    const src = join(tmp, 'hero.svg')
    writeFileSync(src, '<svg viewBox="0 0 1 1"/>')
    const e = new AssetEmitter(join(tmp, 'once'), '/docs/assets', false)
    expect(e.publishFile(src)).toBe(e.publishFile(src))
    expect(e.names()).toHaveLength(1)
  })

  it('adds the plain legacy name only while LEGACY_UNHASHED_ASSETS is on', () => {
    const on = new AssetEmitter(join(tmp, 'on'), '/docs/assets', true)
    on.publishBytes('docs.css', 'a{}')
    expect(on.names()).toContain('docs.css')
    on.flush()
    expect(readFileSync(join(tmp, 'on', 'docs.css'), 'utf8')).toBe('a{}')

    const off = new AssetEmitter(join(tmp, 'off'), '/docs/assets', false)
    off.publishBytes('docs.css', 'a{}')
    expect(off.names()).not.toContain('docs.css')
  })

  it('refuses two different files that would share one legacy name', () => {
    const e = new AssetEmitter(join(tmp, 'clash'), '/docs/assets', true)
    e.publishBytes('hero.png', 'one')
    expect(() => e.publishBytes('hero.png', 'two')).toThrow(/two different assets/)
    // The same bytes twice is not a clash.
    expect(() => e.publishBytes('hero.png', 'one')).not.toThrow()
  })
})
