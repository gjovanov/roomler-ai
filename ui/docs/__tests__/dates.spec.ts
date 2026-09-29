// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * FR-87 (#1776) — the dates contract.
 *
 * The property that matters is the one FR-60 lost silently in production:
 * when nothing knows a page's date, the page has NO date. The build date is
 * never a fallback, because a `lastmod` that moves on every deploy teaches a
 * crawler to ignore every `lastmod` we publish.
 */
import { mkdtempSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { afterAll, describe, expect, it } from 'vitest'
import { manifestJson, parseGitLog, parseManifest, resolveDates, type ContentDates } from '../dates.ts'

const LOG = [
  '__C__2026-09-20',
  'ui/docs/content/a.md',
  '',
  '__C__2026-09-10',
  'ui/docs/content/a.md',
  'ui/docs/content/b.md',
  '',
  '__C__2026-09-01',
  'ui/docs/content/a.md',
  '',
].join('\n')

describe('parseGitLog', () => {
  it('reads the newest commit as `modified` and the oldest as `created`', () => {
    const files = parseGitLog(LOG)
    expect(files.get('ui/docs/content/a.md')).toEqual({ created: '2026-09-01', modified: '2026-09-20' })
    expect(files.get('ui/docs/content/b.md')).toEqual({ created: '2026-09-10', modified: '2026-09-10' })
  })

  it('ignores names that appear before any commit line', () => {
    expect(parseGitLog('stray.md\n__C__2026-09-01\nreal.md').has('stray.md')).toBe(false)
  })

  it('ignores a commit whose date is not YYYY-MM-DD', () => {
    expect(parseGitLog('__C__yesterday\nx.md').size).toBe(0)
  })

  it('tolerates CRLF output', () => {
    expect(parseGitLog('__C__2026-09-01\r\nx.md\r\n').get('x.md')?.modified).toBe('2026-09-01')
  })
})

describe('the manifest', () => {
  const a = new Map<string, ContentDates>([
    ['ui/docs/content/z.md', { created: '2026-09-01', modified: '2026-09-02' }],
    ['ui/docs/content/a.md', { created: '2026-09-03', modified: '2026-09-04' }],
  ])

  it('is byte-identical for the same history, whatever the insertion order', () => {
    // The Docker layer cache keys on these bytes: a manifest that changed on
    // every run would rebuild the UI stage for a commit that touched no UI.
    const b = new Map([...a.entries()].reverse())
    expect(manifestJson(b)).toBe(manifestJson(a))
    expect(manifestJson(a).indexOf('a.md')).toBeLessThan(manifestJson(a).indexOf('z.md'))
  })

  it('round-trips', () => {
    expect(parseManifest(manifestJson(a), 'm')).toEqual(a)
  })

  it('carries no timestamp of its own', () => {
    expect(manifestJson(a)).not.toMatch(/generated|built|T\d\d:/i)
  })

  it('refuses a corrupt manifest instead of silently dropping every date', () => {
    expect(() => parseManifest('{', 'm')).toThrow(/not valid JSON/)
    expect(() => parseManifest('{"version":2,"files":{}}', 'm')).toThrow(/version/)
    expect(() => parseManifest('{"version":1,"files":{"x.md":{"created":"2026-09-01","modified":"soon"}}}', 'm')).toThrow(/x\.md/)
  })
})

describe('resolveDates', () => {
  const dir = mkdtempSync(join(tmpdir(), 'fr87-dates-'))
  afterAll(() => rmSync(dir, { recursive: true, force: true }))
  const shallow = () => ({ note: 'shallow clone' })

  it('prefers live git when the clone has history', () => {
    const live = new Map([['x.md', { created: '2026-09-01', modified: '2026-09-05' }]])
    const r = resolveDates(join(dir, 'absent.json'), () => ({ files: live }))
    expect(r.source).toBe('git')
    expect(r.files).toBe(live)
  })

  it('falls back to the manifest when git cannot answer', () => {
    const path = join(dir, 'content-dates.json')
    writeFileSync(path, manifestJson(new Map([['x.md', { created: '2026-09-01', modified: '2026-09-05' }]])))
    const r = resolveDates(path, shallow)
    expect(r.source).toBe('manifest')
    expect(r.files.get('x.md')?.modified).toBe('2026-09-05')
    expect(r.note).toBe('shallow clone')
  })

  it('answers NOTHING when neither knows (layout.spec.ts proves nothing is then published)', () => {
    const r = resolveDates(join(dir, 'absent.json'), shallow)
    expect(r.source).toBe('none')
    expect(r.files.size).toBe(0)
    expect(r.note).toMatch(/shallow clone, and no manifest/)
  })
})
