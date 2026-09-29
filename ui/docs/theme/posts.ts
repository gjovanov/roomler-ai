// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * FR-87 (#1776) — the blog's posts: what a post must declare, and the
 * orderings, pages and cross-links built from them.
 *
 * A post is `ui/blog/posts/<slug>.md`. Its dates are EDITORIAL: they come
 * from front-matter, never from git, because fixing a typo is not a new post
 * and a rename is not a new publication. Everything here is pure, so each
 * gate can be tested by feeding it the front-matter that should trip it.
 */
import { ISO_TIMESTAMP } from '../dates.ts'
import {
  AUTHORS,
  BLOG_BASE,
  BLOG_FRONTMATTER_KEYS,
  MAX_DESCRIPTION_CHARS,
  MAX_TITLE_CHARS,
  SITE_ORIGIN,
  WORDS_PER_MINUTE,
  type Author,
} from '../site.ts'
import { unknownKeys, type Frontmatter } from './frontmatter.ts'
import type { Heading, ResolvedImage } from './render.ts'
import type { OgImage } from './shell.ts'

export interface PostMeta {
  /** The file name without `.md`, and the URL segment. */
  slug: string
  title: string
  seoTitle?: string
  /** The dek: one line under the title. */
  subtitle?: string
  description: string
  /** ISO 8601 timestamp with its offset: when the post was first published. */
  date: string
  /** ISO 8601 timestamp, never before `date`: the last substantive edit. */
  updated?: string
  authorKey: string
  author: Author
  tags: string[]
  /** A file in the blog's assets (or the shared docs assets). */
  hero: string
  heroAlt: string
  /** A raster share image; required when the hero is not one (an SVG). */
  ogImage?: string
  /** Site-absolute URLs of pages this post leans on; link-checked. */
  related: string[]
  /** Where else the post is published, e.g. the Medium copy. */
  syndication?: string
  /** Only for a post whose ORIGINAL lives elsewhere. */
  canonical?: string
}

export interface Post extends PostMeta {
  url: string
  outFile: string
  /** Repo-relative path of the source, for errors and "Edit this page". */
  sourceFile: string
  heroImage: ResolvedImage
  og: OgImage
  html: string
  headings: Heading[]
  plain: string
  readingMinutes: number
}

const SLUG = /^[a-z0-9]+(?:-[a-z0-9]+)*$/
const TAG = /^[a-z0-9-]+$/

/**
 * Reads and checks a post's front-matter. Returns every problem at once, so
 * an author fixes a post in one pass rather than one build per mistake.
 */
export function readPostMeta(
  data: Frontmatter,
  slug: string,
  rel: string,
  now: Date,
): { meta?: PostMeta; errors: string[] } {
  const errors: string[] = []
  const err = (msg: string) => errors.push(`${rel} — ${msg}`)

  if (!SLUG.test(slug)) err(`file name "${slug}" must be kebab-case (a-z, 0-9, single hyphens): it becomes the URL`)

  for (const key of unknownKeys(data, BLOG_FRONTMATTER_KEYS)) {
    if (key === 'draft') {
      err('`draft` does not exist here: a file in ui/blog/posts/ IS published. Keep an unpublished post in the private promo repo.')
    } else {
      err(`unknown frontmatter key \`${key}\`. Known: ${BLOG_FRONTMATTER_KEYS.join(', ')}`)
    }
  }

  const str = (key: string, required: boolean): string | undefined => {
    const v = data[key]
    if (v === undefined) {
      if (required) err(`frontmatter \`${key}\` is required`)
      return undefined
    }
    if (typeof v !== 'string' || v.trim() === '') {
      err(`frontmatter \`${key}\` must be a non-empty string`)
      return undefined
    }
    return v.trim()
  }
  const list = (key: string, required: boolean): string[] => {
    const v = data[key]
    if (v === undefined) {
      if (required) err(`frontmatter \`${key}\` is required`)
      return []
    }
    if (!Array.isArray(v) || (required && v.length === 0)) {
      err(`frontmatter \`${key}\` must be a${required ? ' non-empty' : ''} list`)
      return []
    }
    return v.map((s) => String(s).trim()).filter(Boolean)
  }

  const title = str('title', true)
  const seoTitle = str('seoTitle', false)
  const subtitle = str('subtitle', false)
  const description = str('description', true)
  const date = str('date', true)
  const updated = str('updated', false)
  const authorKey = str('author', true)
  const tags = list('tags', true)
  const hero = str('hero', true)
  const heroAlt = str('heroAlt', true)
  const ogImage = str('ogImage', false)
  const related = list('related', false)
  const syndication = str('syndication', false)
  const canonical = str('canonical', false)

  if (seoTitle && seoTitle.length > MAX_TITLE_CHARS) {
    err(`frontmatter \`seoTitle\` is ${seoTitle.length} chars; the limit is ${MAX_TITLE_CHARS}`)
  }
  if (description && description.length > MAX_DESCRIPTION_CHARS) {
    err(`frontmatter \`description\` is ${description.length} chars; the limit is ${MAX_DESCRIPTION_CHARS}`)
  }

  // Dates are full timestamps: Google flags a bare date in structured data,
  // and the Atom feed requires the time and the zone.
  const stamp = (key: string, v: string | undefined): number | undefined => {
    if (v === undefined) return undefined
    if (!ISO_TIMESTAMP.test(v) || Number.isNaN(Date.parse(v))) {
      err(`\`${key}: ${v}\` must be an ISO 8601 timestamp with its offset, e.g. 2026-09-25T22:45:13Z`)
      return undefined
    }
    const t = Date.parse(v)
    if (t > now.getTime()) err(`\`${key}: ${v}\` is in the future. A post is published when it is in this directory; there is no scheduling`)
    return t
  }
  const t0 = stamp('date', date)
  const t1 = stamp('updated', updated)
  if (t0 !== undefined && t1 !== undefined && t1 < t0) err(`\`updated\` (${updated}) is before \`date\` (${date})`)

  const author = authorKey ? AUTHORS[authorKey] : undefined
  if (authorKey && !author) err(`author "${authorKey}" is not in AUTHORS (ui/docs/site.ts). Known: ${Object.keys(AUTHORS).join(', ')}`)

  for (const t of tags) if (!TAG.test(t)) err(`tag "${t}" must be lowercase a-z, 0-9 and hyphens`)
  for (const r of related) if (!r.startsWith('/')) err(`related "${r}" must be a site-absolute path such as /docs/start/`)

  const offSite = (key: string, v: string | undefined) => {
    if (v === undefined) return
    if (!/^https:\/\/[^/]+\//.test(v)) err(`\`${key}\` must be an absolute https:// URL`)
    else if (v.startsWith(`${SITE_ORIGIN}/`)) err(`\`${key}\` points at this site; ${key === 'canonical' ? 'a post published here is its own canonical' : 'it names a copy published elsewhere'}`)
  }
  offSite('syndication', syndication)
  offSite('canonical', canonical)

  if (errors.length) return { errors }
  return {
    errors,
    meta: {
      slug,
      title: title!,
      seoTitle,
      subtitle,
      description: description!,
      date: date!,
      updated,
      authorKey: authorKey!,
      author: author!,
      tags,
      hero: hero!,
      heroAlt: heroAlt!,
      ogImage,
      related,
      syndication,
      canonical,
    },
  }
}

export function postUrl(slug: string): string {
  return `${BLOG_BASE}/${slug}/`
}

/** Page 1 of the index is `/blog/`; there is no `/blog/page/1/`. */
export function indexPageUrl(n: number): string {
  return n <= 1 ? `${BLOG_BASE}/` : `${BLOG_BASE}/page/${n}/`
}

export function readingMinutes(plain: string): number {
  const words = plain.split(/\s+/).filter(Boolean).length
  return Math.max(1, Math.round(words / WORDS_PER_MINUTE))
}

/** Newest first, by instant (offsets differ, so strings do not sort). */
export function sortPosts<T extends { date: string; slug: string }>(posts: T[]): T[] {
  return [...posts].sort((a, b) => Date.parse(b.date) - Date.parse(a.date) || a.slug.localeCompare(b.slug))
}

export function paginate<T>(items: T[], perPage: number): T[][] {
  const pages: T[][] = []
  for (let i = 0; i < items.length; i += perPage) pages.push(items.slice(i, i + perPage))
  return pages.length ? pages : [[]]
}

/** The newest instant a post was touched: `updated`, else `date`. */
export function lastTouched(p: { date: string; updated?: string }): string {
  return p.updated ?? p.date
}

/**
 * Docs page URL → the posts that link to it (body links and `related`). A
 * docs page shows these as "On the blog", so a reader who arrives from search
 * on the reference page can find the story around it.
 */
export function docsBacklinks<T extends { html: string; related: string[] }>(posts: T[], docsBase: string): Map<string, T[]> {
  const map = new Map<string, T[]>()
  for (const post of posts) {
    const targets = new Set<string>()
    for (const m of post.html.matchAll(/href="([^"#?]+)/g)) targets.add(m[1]!)
    for (const r of post.related) targets.add(r.split('#')[0]!)
    for (const t of targets) {
      if (!t.startsWith(`${docsBase}/`) || /\.[a-z0-9]+$/i.test(t)) continue
      const url = t.endsWith('/') ? t : `${t}/`
      if (!map.has(url)) map.set(url, [])
      map.get(url)!.push(post)
    }
  }
  return map
}
