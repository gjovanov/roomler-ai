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

// git's `%cI`: committer timestamps, each in its committer's own offset.
const LOG = [
  '__C__2026-09-20T10:00:00+02:00',
  'ui/docs/content/a.md',
  '',
  '__C__2026-09-10T09:30:00Z',
  'ui/docs/content/a.md',
  'ui/docs/content/b.md',
  '',
  '__C__2026-09-01T18:45:12-04:00',
  'ui/docs/content/a.md',
  '',
].join('\n')

describe('parseGitLog', () => {
  it('reads the newest commit as `modified` and the oldest as `created`', () => {
    const files = parseGitLog(LOG)
    expect(files.get('ui/docs/content/a.md')).toEqual({ created: '2026-09-01T18:45:12-04:00', modified: '2026-09-20T10:00:00+02:00' })
    expect(files.get('ui/docs/content/b.md')).toEqual({ created: '2026-09-10T09:30:00Z', modified: '2026-09-10T09:30:00Z' })
  })

  it('ignores names that appear before any commit line', () => {
    expect(parseGitLog('stray.md\n__C__2026-09-01T00:00:00Z\nreal.md').has('stray.md')).toBe(false)
  })

  it('ignores a commit whose date is not an ISO timestamp', () => {
    expect(parseGitLog('__C__yesterday\nx.md').size).toBe(0)
    // A bare date is what `%cs` prints; structured data needs `%cI`'s time and zone.
    expect(parseGitLog('__C__2026-09-01\nx.md').size).toBe(0)
  })

  it('tolerates CRLF output', () => {
    expect(parseGitLog('__C__2026-09-01T12:00:00Z\r\nx.md\r\n').get('x.md')?.modified).toBe('2026-09-01T12:00:00Z')
  })
})

describe('the manifest', () => {
  const a = new Map<string, ContentDates>([
    ['ui/docs/content/z.md', { created: '2026-09-01T10:00:00Z', modified: '2026-09-02T11:00:00+02:00' }],
    ['ui/docs/content/a.md', { created: '2026-09-03T08:00:00Z', modified: '2026-09-04T09:15:00-07:00' }],
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

  it('carries no timestamp of its own — only the commits\'', () => {
    expect(Object.keys(JSON.parse(manifestJson(a)))).toEqual(['version', 'files'])
  })

  it('refuses a corrupt manifest instead of silently dropping every date', () => {
    expect(() => parseManifest('{', 'm')).toThrow(/not valid JSON/)
    // Version 1 held bare dates; reading it as timestamps would be wrong.
    expect(() => parseManifest('{"version":1,"files":{}}', 'm')).toThrow(/version/)
    expect(() => parseManifest('{"version":2,"files":{"x.md":{"created":"2026-09-01T00:00:00Z","modified":"soon"}}}', 'm')).toThrow(/x\.md/)
    expect(() => parseManifest('{"version":2,"files":{"x.md":{"created":"2026-09-01","modified":"2026-09-01"}}}', 'm')).toThrow(/x\.md/)
  })
})

describe('resolveDates', () => {
  const dir = mkdtempSync(join(tmpdir(), 'fr87-dates-'))
  afterAll(() => rmSync(dir, { recursive: true, force: true }))
  const shallow = () => ({ note: 'shallow clone' })

  it('prefers live git when the clone has history', () => {
    const live = new Map([['x.md', { created: '2026-09-01T00:00:00Z', modified: '2026-09-05T00:00:00Z' }]])
    const r = resolveDates(join(dir, 'absent.json'), () => ({ files: live }))
    expect(r.source).toBe('git')
    expect(r.files).toBe(live)
  })

  it('falls back to the manifest when git cannot answer', () => {
    const path = join(dir, 'content-dates.json')
    writeFileSync(path, manifestJson(new Map([['x.md', { created: '2026-09-01T00:00:00Z', modified: '2026-09-05T12:30:00+02:00' }]])))
    const r = resolveDates(path, shallow)
    expect(r.source).toBe('manifest')
    expect(r.files.get('x.md')?.modified).toBe('2026-09-05T12:30:00+02:00')
    expect(r.note).toBe('shallow clone')
  })

  it('answers NOTHING when neither knows (layout.spec.ts proves nothing is then published)', () => {
    const r = resolveDates(join(dir, 'absent.json'), shallow)
    expect(r.source).toBe('none')
    expect(r.files.size).toBe(0)
    expect(r.note).toMatch(/shallow clone, and no manifest/)
  })
})
