// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * FR-88 (#1790) P3 — video on the site.
 *
 * The property that must hold is a privacy one: a page with a video loads
 * NOTHING from YouTube until someone presses play. So the rendered HTML is
 * checked for the player host, and docs.js is RUN to prove the click is what
 * brings it in. The manifest is a build gate: an id it does not hold fails.
 */
import { existsSync, readFileSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'
import { describe, expect, it } from 'vitest'
import { clockOf, embedUrl, FEATURED_VIDEO_ID, minutesOf, VIDEOS, videoById, watchUrl } from '../../src/utils/videos.ts'
import { SITE_ORIGIN } from '../site.ts'
import { renderHome } from '../theme/home-layout.ts'
import { createRenderer, renderMarkdown } from '../theme/render.ts'
import type { ShellNav, SiteAssets } from '../theme/shell.ts'
import { videoObject } from '../theme/structured.ts'

const HERE = dirname(fileURLToPath(import.meta.url))
const DOCS_JS = readFileSync(join(HERE, '..', 'theme', 'docs.js'), 'utf8')
const POSTERS = join(HERE, '..', '..', 'src', 'assets', 'tutorial')

const md = createRenderer()
const ID = FEATURED_VIDEO_ID
const poster = { url: '/docs/assets/video-C1YdfmhbAKM.0123456789.jpg', width: 1280, height: 720 }

describe('the manifest', () => {
  it('holds only well-formed entries whose poster is in the shared assets', () => {
    expect(VIDEOS.length).toBeGreaterThan(0)
    for (const v of VIDEOS) {
      expect(v.id, v.id).toMatch(/^[A-Za-z0-9_-]{11}$/)
      expect(v.uploadDate, v.id).toMatch(/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(Z|[+-]\d{2}:\d{2})$/)
      expect(() => clockOf(v.duration), v.id).not.toThrow()
      expect(existsSync(join(POSTERS, v.poster)), v.poster).toBe(true)
    }
    expect(new Set(VIDEOS.map((v) => v.id)).size).toBe(VIDEOS.length)
  })

  it('features a video it holds', () => {
    expect(videoById(FEATURED_VIDEO_ID)).toBeDefined()
  })

  it('formats durations for the poster badge and the label', () => {
    expect(clockOf('PT9M3S')).toBe('9:03')
    expect(clockOf('PT1H2M5S')).toBe('1:02:05')
    expect(clockOf('PT45S')).toBe('0:45')
    expect(minutesOf('PT9M3S')).toBe(9)
    expect(minutesOf('PT9M30S')).toBe(10)
    expect(() => clockOf('9:03')).toThrow(/ISO 8601/)
  })

  it('plays from youtube-nocookie, and links to the watch page', () => {
    expect(embedUrl(ID)).toBe(`https://www.youtube-nocookie.com/embed/${ID}?autoplay=1&rel=0`)
    expect(watchUrl(ID)).toBe(`https://www.youtube.com/watch?v=${ID}`)
  })
})

describe(':::video', () => {
  const src = `Intro.\n\n:::video ${ID}\nA *short* tour.\n:::\n\nAfter.`

  it('renders a facade that requests nothing from YouTube', () => {
    const { html, videos } = renderMarkdown(md, src, 'test.md', { resolveImage: () => poster })
    expect(html).toContain(`data-video-id="${ID}"`)
    expect(html).toContain(`href="${watchUrl(ID)}"`)
    expect(html).toContain(`src="${poster.url}"`)
    expect(html).toContain('<em>short</em> tour.')
    expect(html).toContain('9:03')
    // No player, no request: nothing from a YouTube host loads with the page.
    expect(html).not.toMatch(/<iframe/i)
    expect(html).not.toContain('youtube-nocookie.com')
    expect(html).not.toContain('ytimg.com')
    // The paragraphs around it survive (the block is closed).
    expect(html).toContain('<p>Intro.</p>')
    expect(html).toContain('<p>After.</p>')
    expect(videos).toEqual([{ video: videoById(ID), poster }])
  })

  it('fails on an id the manifest does not hold', () => {
    expect(() => renderMarkdown(md, ':::video dQw4w9WgXcQ\n:::', 'test.md')).toThrow(/not in ui\/src\/utils\/videos\.ts/)
  })

  it('reports to the build instead of throwing when the build collects errors', () => {
    const errors: string[] = []
    renderMarkdown(md, ':::video dQw4w9WgXcQ\n:::', 'start/x.md', { fail: (m) => errors.push(m) })
    expect(errors).toEqual([expect.stringContaining('start/x.md — `:::video dQw4w9WgXcQ` is not in')])
  })
})

describe('docs.js — the click swaps in the player', () => {
  function page(): Document {
    const { html } = renderMarkdown(md, `:::video ${ID}\n:::`, 'test.md', { resolveImage: () => poster })
    const doc = new DOMParser().parseFromString(`<!DOCTYPE html><html><body>${html}</body></html>`, 'text/html')
    new Function('window', 'document', 'navigator', DOCS_JS)({ isSecureContext: true }, doc, {})
    return doc
  }
  const click = (el: Element, init: MouseEventInit = {}) =>
    el.dispatchEvent(new MouseEvent('click', { bubbles: true, cancelable: true, button: 0, ...init }))

  it('replaces the poster link with the youtube-nocookie player', () => {
    const doc = page()
    expect(doc.querySelector('iframe')).toBeNull()
    const link = doc.querySelector('a[data-video-id]')!
    const notPrevented = click(link.querySelector('.video-facade__play')!)
    expect(notPrevented).toBe(false)
    const f = doc.querySelector('iframe')!
    expect(f.getAttribute('src')).toBe(embedUrl(ID))
    expect(f.getAttribute('title')).toBe(videoById(ID)!.title)
    expect(f.getAttribute('allow')).toContain('autoplay')
    expect(f.hasAttribute('allowfullscreen')).toBe(true)
    expect(doc.querySelector('a[data-video-id]')).toBeNull()
  })

  it('leaves a modified click to the browser (a new tab on YouTube)', () => {
    const doc = page()
    const link = doc.querySelector('a[data-video-id]')!
    click(link, { ctrlKey: true })
    click(link, { button: 1 })
    expect(doc.querySelector('iframe')).toBeNull()
  })
})

describe('the homepage and the structured data', () => {
  const assets: SiteAssets = {
    css: '/docs/assets/docs.0123456789.css',
    js: '/docs/assets/docs.abcdefabcd.js',
    search: '/docs/assets/search.1111111111.js',
    osPreference: '/docs/assets/os-preference.2222222222.js',
    searchIndex: '/docs/assets/search-index.3333333333.json',
  }
  const nav: ShellNav = { current: 'home', hasBlog: false }
  const hero = { url: '/docs/assets/hero-mesh.7777777777.svg', width: 880, height: 560 }
  const graphOf = (html: string) =>
    JSON.parse(/<script type="application\/ld\+json">([\s\S]*?)<\/script>/.exec(html)![1]!)['@graph'] as Array<Record<string, unknown>>

  it('shows the featured video under the capability chips, with its VideoObject', () => {
    const v = videoById(FEATURED_VIDEO_ID)!
    const html = renderHome({ assets, nav, hero, video: { video: v, poster } })
    expect(html).toContain('id="video"')
    expect(html.indexOf('home-caps')).toBeLessThan(html.indexOf('id="video"'))
    expect(html.indexOf('id="video"')).toBeLessThan(html.indexOf('id="features"'))
    expect(html).toContain(`data-video-id="${v.id}"`)
    expect(html).not.toMatch(/<iframe/i)
    const node = graphOf(html).find((n) => n['@type'] === 'VideoObject')!
    expect(node).toMatchObject({
      name: v.title,
      uploadDate: v.uploadDate,
      duration: v.duration,
      embedUrl: `https://www.youtube-nocookie.com/embed/${v.id}`,
      thumbnailUrl: [`${SITE_ORIGIN}${poster.url}`],
    })
  })

  it('has no video section without a video', () => {
    const html = renderHome({ assets, nav, hero })
    expect(html).not.toContain('id="video"')
    expect(graphOf(html).some((n) => n['@type'] === 'VideoObject')).toBe(false)
  })

  it('builds a VideoObject Google can read', () => {
    const v = videoById(ID)!
    const node = videoObject({ ...v, thumbnailUrl: `${SITE_ORIGIN}/x.jpg` })
    for (const key of ['name', 'description', 'thumbnailUrl', 'uploadDate']) expect(node[key], key).toBeTruthy()
    expect(node['@type']).toBe('VideoObject')
  })
})
