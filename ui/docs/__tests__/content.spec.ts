// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * FR-59 (#1165) — the CONTENT contract.
 *
 * The generator already fails the build on each of these. Asserting them
 * here too is not redundant: `bun run build` is the slow path, and these
 * run in the unit lane on every change, so a contributor finds out from a
 * test rather than from a failed release build.
 */
import { readFileSync, readdirSync, statSync } from 'node:fs'
import { dirname, join, relative, sep } from 'node:path'
import { fileURLToPath } from 'node:url'
import { describe, expect, it } from 'vitest'
import { parseFrontmatter, requireString, requireStringArray } from '../theme/frontmatter.ts'
import { createRenderer, renderMarkdown } from '../theme/render.ts'
import { MAX_DESCRIPTION_CHARS, SECTIONS, sectionByDir } from '../site.ts'

const HERE = dirname(fileURLToPath(import.meta.url))
const CONTENT = join(HERE, '..', 'content')

function walk(dir: string, out: string[] = []): string[] {
  for (const entry of readdirSync(dir)) {
    const full = join(dir, entry)
    if (statSync(full).isDirectory()) walk(full, out)
    else if (entry.endsWith('.md')) out.push(full)
  }
  return out
}

const files = walk(CONTENT).sort()
const slugOf = (f: string) => relative(CONTENT, f).split(sep).join('/').replace(/\.md$/, '')

describe('documentation content', () => {
  it('has content to check', () => {
    // A suite that silently matched zero files would pass while proving
    // nothing — the precondition is the load-bearing assertion.
    expect(files.length).toBeGreaterThan(30)
  })

  it('has a home page', () => {
    expect(files.map(slugOf)).toContain('index')
  })

  it.each(files.map((f) => [slugOf(f), f]))('%s — frontmatter contract', (slug, file) => {
    const { data } = parseFrontmatter(readFileSync(file, 'utf8'), slug)

    const title = requireString(data, 'title', slug)
    const description = requireString(data, 'description', slug)
    const tags = requireStringArray(data, 'tags', slug)

    expect(title.length).toBeGreaterThan(2)
    // Over the limit is silently truncated in search results, which is a
    // defect nobody sees until they read their own listing.
    expect(description.length).toBeLessThanOrEqual(MAX_DESCRIPTION_CHARS)
    expect(tags.every((t) => /^[a-z0-9-]+$/.test(t))).toBe(true)
  })

  it('places every page in a declared section', () => {
    const orphans = files
      .map(slugOf)
      .filter((s) => s.includes('/'))
      .map((s) => s.split('/')[0]!)
      .filter((dir) => !sectionByDir(dir))
    expect([...new Set(orphans)]).toEqual([])
  })

  it('produces a unique URL per page', () => {
    const urls = files.map(slugOf).map((s) => (s === 'index' ? '/' : `/${s.replace(/\/index$/, '')}/`))
    expect(urls.length).toBe(new Set(urls).size)
  })

  it('covers every section that site.ts declares', () => {
    // A declared section with no pages renders an empty card in the nav.
    const dirs = new Set(files.map(slugOf).filter((s) => s.includes('/')).map((s) => s.split('/')[0]!))
    for (const section of SECTIONS) {
      expect(dirs.has(section.dir)).toBe(true)
    }
  })

  it('documents all three operating systems in Getting started', () => {
    const slugs = files.map(slugOf)
    for (const os of ['windows', 'macos', 'linux']) {
      expect(slugs).toContain(`start/install/${os}`)
    }
  })
})

describe('every page renders', () => {
  const md = createRenderer()

  it.each(files.map((f) => [slugOf(f), f]))('%s', (slug, file) => {
    const { body } = parseFrontmatter(readFileSync(file, 'utf8'), slug)
    const { html } = renderMarkdown(md, body, slug)
    expect(html.length).toBeGreaterThan(0)
    // An unrendered container marker means the pre-pass missed a block and
    // readers would see `:::warning` as literal text.
    expect(html).not.toMatch(/(^|[^:]):::($|[^:])/)
  })
})

describe('internal links resolve', () => {
  const md = createRenderer()
  const known = new Set(
    files.map(slugOf).map((s) => (s === 'index' ? '/docs/' : `/docs/${s.replace(/\/index$/, '')}/`)),
  )
  // Section indexes are generated when nobody authored one, so they are
  // valid targets even with no file behind them.
  for (const section of SECTIONS) known.add(`/docs/${section.dir}/`)

  it.each(files.map((f) => [slugOf(f), f]))('%s', (slug, file) => {
    const { body } = parseFrontmatter(readFileSync(file, 'utf8'), slug)
    const { html } = renderMarkdown(md, body, slug)
    const dangling: string[] = []
    for (const m of html.matchAll(/href="(\/docs\/[^"#]*)/g)) {
      const href = m[1]!
      if (/\.[a-z0-9]+$/i.test(href)) continue
      const normalised = href.endsWith('/') ? href : `${href}/`
      if (!known.has(normalised) && !normalised.startsWith('/docs/tags/')) dangling.push(href)
    }
    expect(dangling).toEqual([])
  })
})
