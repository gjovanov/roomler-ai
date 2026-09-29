// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * FR-87 (#1776) — the parts of a page every collection shares: the `<head>`,
 * the top bar, the footer, the search dialog and the scripts. The docs use
 * it now; the blog and the static homepage render the same shell, so a crawler
 * and a reader meet one site rather than three.
 *
 * Everything a crawler reads is emitted statically, per page: a unique
 * `<title>` that fits, a real description, an ABSOLUTE canonical, OG/Twitter
 * tags, robots directives and one JSON-LD graph.
 */
import { ANALYTICS, BASE, MAX_TITLE_CHARS, OG_IMAGE, OG_IMAGE_META, SITE_NAME, SITE_ORIGIN } from '../site.ts'
import { icon } from './icons.ts'
import { escapeHtml } from './render.ts'
import { jsonLdScript } from './structured.ts'

/** The theme's own files, published under content-hashed names. */
export interface SiteAssets {
  css: string
  js: string
  search: string
  osPreference: string
  searchIndex: string
}

export interface OgImage {
  /** Absolute URL of a raster image. */
  url: string
  width: number
  height: number
  alt: string
}

export const DEFAULT_OG_IMAGE: OgImage = { url: `${SITE_ORIGIN}${OG_IMAGE}`, ...OG_IMAGE_META }

export interface HeadInput {
  /** The whole `<title>`, already fitted (see `fitTitle`). */
  title: string
  description: string
  /** Absolute. Absent only on the 404 page, which has no address of its own. */
  canonical?: string
  noindex: boolean
  og: { type: 'website' | 'article'; title: string; image?: OgImage }
  /** `article:*` properties, on leaves only. */
  article?: { published?: string; modified?: string; tags: string[] }
  /** The page's JSON-LD graph; absent on the 404 page. */
  jsonLd?: unknown
  assets: SiteAssets
}

/** The first candidate that fits the title limit, or null when none does. */
export function fitTitle(candidates: string[]): string | null {
  return candidates.find((c) => c.length <= MAX_TITLE_CHARS) ?? null
}

export function renderHead(h: HeadInput): string {
  const image = h.og.image ?? DEFAULT_OG_IMAGE
  const a = h.article
  return [
    '<meta charset="UTF-8">',
    '<meta name="viewport" content="width=device-width, initial-scale=1">',
    `<title>${escapeHtml(h.title)}</title>`,
    `<meta name="description" content="${escapeHtml(h.description)}">`,
    h.canonical ? `<link rel="canonical" href="${h.canonical}">` : '',
    // Opted-out pages keep their links followed. Everything else asks for
    // large image previews, which Google otherwise caps at a thumbnail.
    `<meta name="robots" content="${h.noindex ? 'noindex, follow' : 'max-image-preview:large'}">`,
    `<meta property="og:type" content="${h.og.type}">`,
    `<meta property="og:site_name" content="${SITE_NAME}">`,
    `<meta property="og:title" content="${escapeHtml(h.og.title)}">`,
    `<meta property="og:description" content="${escapeHtml(h.description)}">`,
    h.canonical ? `<meta property="og:url" content="${h.canonical}">` : '',
    `<meta property="og:image" content="${image.url}">`,
    `<meta property="og:image:width" content="${image.width}">`,
    `<meta property="og:image:height" content="${image.height}">`,
    `<meta property="og:image:alt" content="${escapeHtml(image.alt)}">`,
    a?.published ? `<meta property="article:published_time" content="${a.published}">` : '',
    a?.modified ? `<meta property="article:modified_time" content="${a.modified}">` : '',
    ...(a?.tags ?? []).map((t) => `<meta property="article:tag" content="${escapeHtml(t)}">`),
    '<meta name="twitter:card" content="summary_large_image">',
    `<meta name="twitter:title" content="${escapeHtml(h.og.title)}">`,
    `<meta name="twitter:description" content="${escapeHtml(h.description)}">`,
    `<meta name="twitter:image" content="${image.url}">`,
    `<meta name="twitter:image:alt" content="${escapeHtml(image.alt)}">`,
    '<meta name="theme-color" content="#009688">',
    '<link rel="icon" type="image/svg+xml" href="/favicon.svg">',
    `<link rel="stylesheet" href="${h.assets.css}">`,
    `<script src="${h.assets.osPreference}"></script>`,
    // The same first-party script the SPA loads; `defer`, so it never holds
    // up the page. `ANALYTICS = null` in site.ts turns it off.
    ANALYTICS ? `<script defer data-domain="${ANALYTICS.domain}" src="${ANALYTICS.src}"></script>` : '',
    h.jsonLd ? jsonLdScript(h.jsonLd) : '',
  ]
    .filter(Boolean)
    .join('\n')
}

export function renderTopbar(): string {
  return `<header class="topbar">
  <div class="topbar__inner">
    <a class="brand" href="${BASE}/"><span class="brand__mark">Roomler</span><span class="brand__docs">Docs</span></a>
    <button class="topbar__burger" type="button" aria-label="Open navigation" aria-expanded="false" data-nav-toggle>${icon('menu', { size: 22 })}</button>
    <button class="search-open" type="button" data-search-open aria-label="Search the documentation">
      ${icon('search', { size: 17 })}<span>Search</span><kbd>/</kbd>
    </button>
    <nav class="topbar__links" aria-label="Site">
      <a href="/landing">Product</a>
      <a href="/pricing">Pricing</a>
      <a href="https://github.com/gjovanov/roomler-ai" target="_blank" rel="noopener noreferrer">GitHub</a>
      <a class="btn btn--primary" href="/register">Get started free</a>
    </nav>
  </div>
</header>`
}

export function renderFooter(): string {
  return `<footer class="site-footer">
  <div class="site-footer__inner">
    <div>
      <p class="site-footer__brand">Roomler</p>
      <p class="site-footer__blurb">Remote desktop in a browser tab, a private WireGuard-style mesh, and team chat and video — on one agent you can self-host.</p>
    </div>
    <div>
      <p class="site-footer__head">Docs</p>
      <a href="${BASE}/start/">Get started</a>
      <a href="${BASE}/network/">Private network</a>
      <a href="${BASE}/remote-desktop/">Remote desktop</a>
      <a href="${BASE}/faq/">FAQ</a>
    </div>
    <div>
      <p class="site-footer__head">Product</p>
      <a href="/landing">Overview</a>
      <a href="/pricing">Pricing</a>
      <a href="${BASE}/start/self-hosting/">Self-hosting</a>
    </div>
    <div>
      <p class="site-footer__head">Legal</p>
      <a href="/privacy">Privacy</a>
      <a href="/terms">Terms</a>
      <a href="/imprint">Imprint</a>
    </div>
  </div>
</footer>`
}

export function renderSearchDialog(assets: SiteAssets): string {
  return `<dialog class="search-dialog" data-search-dialog data-search-index="${assets.searchIndex}" aria-label="Search documentation">
  <form class="search-form" method="dialog" role="search">
    ${icon('search', { size: 18, cls: 'search-form__icon' })}
    <input type="search" class="search-input" data-search-input placeholder="Search the docs…" autocomplete="off" spellcheck="false" aria-label="Search query">
    <button type="button" class="search-close" data-search-close aria-label="Close search">${icon('close', { size: 18 })}</button>
  </form>
  <div class="search-results" data-search-results aria-live="polite"></div>
  <p class="search-hint"><kbd>&uarr;</kbd><kbd>&darr;</kbd> to navigate · <kbd>Enter</kbd> to open · <kbd>Esc</kbd> to close</p>
</dialog>`
}

export function renderBodyScripts(assets: SiteAssets): string {
  return `<script src="${assets.js}" defer></script>
<script src="${assets.search}" defer></script>`
}
