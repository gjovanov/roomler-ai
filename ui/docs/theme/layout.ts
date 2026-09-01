// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * FR-60 (#1165) — the page shell.
 *
 * Everything a crawler reads is emitted here, statically, per page: a
 * unique <title>, a real <meta name="description">, an ABSOLUTE canonical,
 * OG/Twitter tags and JSON-LD. The SPA next door has none of these — one
 * `<title>Roomler</title>` for every public route — which is the gap this
 * FR exists to close.
 *
 * ⚠️ `application/ld+json` is a DATA block, not an executable script, so
 * the pod CSP's `script-src 'self'` does not apply to it. That is why the
 * structured data can be inline while `search.js` cannot be.
 */
import { BASE, OG_IMAGE, SITE_NAME, SITE_ORIGIN, SITE_TITLE_SUFFIX, type SectionDef } from '../site.ts'
import { icon } from './icons.ts'
import { escapeHtml, type Heading } from './render.ts'

export interface DocPage {
  /** Path under content/, without extension: `start/install/windows`. */
  slug: string
  /** Site-absolute URL with a trailing slash: `/docs/start/install/windows/`. */
  url: string
  /** Output path relative to the docs root. */
  outFile: string
  section?: SectionDef
  title: string
  description: string
  tags: string[]
  order: number
  hero?: string
  heroAlt?: string
  noindex: boolean
  /** Rendered body HTML. */
  html: string
  headings: Heading[]
  /** Body as plain text, for the search index. */
  plain: string
  /** Emit FAQPage structured data from this page's h2s. */
  faq: boolean
  /** ISO date from git, for the sitemap and the page footer. */
  lastmod: string
  /** Path under `content/` this page was authored from, without extension.
   *  Absent for GENERATED pages — tag indexes, and a section index nobody
   *  wrote. Those must not offer "Edit this page": the link would point at
   *  a file that does not exist, which is a 404 shipped on 32 pages. */
  sourceFile?: string
}

export interface NavSection {
  section: SectionDef
  pages: DocPage[]
}

export interface LayoutCtx {
  nav: NavSection[]
  page: DocPage
  prev?: DocPage
  next?: DocPage
  /** Extra JSON-LD objects beyond the per-page defaults. */
  extraJsonLd?: unknown[]
}

function jsonLdBlock(objects: unknown[]): string {
  if (objects.length === 0) return ''
  const payload = objects.length === 1 ? objects[0] : objects
  // `</script>` inside a JSON string would close the block early; `<` is
  // escaped to its unicode form, which is valid JSON and inert in HTML.
  const json = JSON.stringify(payload).replace(/</g, '\\u003c')
  return `<script type="application/ld+json">${json}</script>`
}

function breadcrumbs(page: DocPage): { html: string; jsonLd: unknown } {
  const trail: Array<{ name: string; url: string }> = [{ name: 'Docs', url: `${BASE}/` }]
  if (page.section) {
    trail.push({ name: page.section.title, url: `${BASE}/${page.section.dir}/` })
  }
  const isSectionIndex = page.section && page.url === `${BASE}/${page.section.dir}/`
  if (!isSectionIndex && page.slug !== 'index') {
    trail.push({ name: page.title, url: page.url })
  }

  const html =
    trail.length <= 1
      ? ''
      : `<nav class="crumbs" aria-label="Breadcrumb"><ol>${trail
          .map((t, i) =>
            i === trail.length - 1
              ? `<li aria-current="page">${escapeHtml(t.name)}</li>`
              : `<li><a href="${t.url}">${escapeHtml(t.name)}</a>${icon('chevronRight', { size: 14, cls: 'crumbs__sep' })}</li>`,
          )
          .join('')}</ol></nav>`

  const jsonLd = {
    '@context': 'https://schema.org',
    '@type': 'BreadcrumbList',
    itemListElement: trail.map((t, i) => ({
      '@type': 'ListItem',
      position: i + 1,
      name: t.name,
      item: `${SITE_ORIGIN}${t.url}`,
    })),
  }
  return { html, jsonLd }
}

function sidebar(nav: NavSection[], current: DocPage): string {
  const groups = nav
    .map(({ section, pages }) => {
      const open = current.section?.dir === section.dir
      const items = pages
        .map(
          (p) =>
            `<li><a href="${p.url}"${p.url === current.url ? ' class="is-active" aria-current="page"' : ''}>${escapeHtml(p.title)}</a></li>`,
        )
        .join('')
      return (
        `<details class="side-group"${open ? ' open' : ''}>` +
        `<summary><span class="side-group__icon side-group__icon--${section.accent}">${icon(section.icon, { size: 17 })}</span>` +
        `<span class="side-group__title">${escapeHtml(section.title)}</span>${icon('chevronDown', { size: 15, cls: 'side-group__chev' })}</summary>` +
        `<ul>${items}</ul></details>`
      )
    })
    .join('')
  return `<nav class="sidebar__nav" aria-label="Documentation sections">${groups}</nav>`
}

function tocHtml(headings: Heading[]): string {
  const items = headings.filter((h) => h.level === 2 || h.level === 3)
  if (items.length < 2) return ''
  return (
    `<nav class="toc" aria-label="On this page"><p class="toc__head">On this page</p><ul>` +
    items
      .map(
        (h) =>
          `<li class="toc__item toc__item--h${h.level}"><a href="#${h.slug}">${escapeHtml(h.text)}</a></li>`,
      )
      .join('') +
    `</ul></nav>`
  )
}

function tagChips(page: DocPage, tags: string[], tagIndexed: Set<string>): string {
  // A tag index's only tag is itself; rendering the chip there is a
  // self-link and adds nothing.
  if (tags.length === 0 || page.slug.startsWith('tags/')) return ''
  return (
    `<ul class="chips" aria-label="Tags">` +
    tags
      .map((t) => {
        const label = escapeHtml(t)
        return tagIndexed.has(t)
          ? `<li><a class="chip chip--link" href="${BASE}/tags/${encodeURIComponent(t)}/">${label}</a></li>`
          : `<li><span class="chip">${label}</span></li>`
      })
      .join('') +
    `</ul>`
  )
}

function pager(prev?: DocPage, next?: DocPage): string {
  if (!prev && !next) return ''
  const left = prev
    ? `<a class="pager__link pager__link--prev" href="${prev.url}">${icon('arrowLeft', { size: 18 })}<span><span class="pager__dir">Previous</span><span class="pager__title">${escapeHtml(prev.title)}</span></span></a>`
    : '<span></span>'
  const right = next
    ? `<a class="pager__link pager__link--next" href="${next.url}"><span><span class="pager__dir">Next</span><span class="pager__title">${escapeHtml(next.title)}</span></span>${icon('arrowRight', { size: 18 })}</a>`
    : '<span></span>'
  return `<nav class="pager" aria-label="Pagination">${left}${right}</nav>`
}

function faqJsonLd(page: DocPage): unknown | null {
  if (!page.faq) return null
  // Questions are the h2s; the answer is the plain text that follows one,
  // up to the next h2. Built from the SAME heading list the TOC uses, so
  // the structured data cannot describe a page shape that is not there.
  const qs = page.headings.filter((h) => h.level === 2)
  if (qs.length === 0) return null
  const entries: unknown[] = []
  for (const [i, q] of qs.entries()) {
    const start = page.plain.indexOf(q.text)
    if (start === -1) continue
    const nextQ = qs[i + 1]
    const end = nextQ ? page.plain.indexOf(nextQ.text, start + q.text.length) : page.plain.length
    const answer = page.plain
      .slice(start + q.text.length, end === -1 ? page.plain.length : end)
      .trim()
    if (!answer) continue
    entries.push({
      '@type': 'Question',
      name: q.text,
      acceptedAnswer: { '@type': 'Answer', text: answer.slice(0, 1200) },
    })
  }
  if (entries.length === 0) return null
  return { '@context': 'https://schema.org', '@type': 'FAQPage', mainEntity: entries }
}

export function renderPage(ctx: LayoutCtx, tagIndexed: Set<string>): string {
  const { page, nav, prev, next } = ctx
  const crumb = breadcrumbs(page)
  const canonical = `${SITE_ORIGIN}${page.url}`
  const fullTitle =
    page.slug === 'index'
      ? `${SITE_TITLE_SUFFIX} — remote desktop, private mesh network, chat & video`
      : page.section && page.url !== `${BASE}/${page.section.dir}/`
        ? `${page.title} · ${page.section.title} — ${SITE_TITLE_SUFFIX}`
        : `${page.title} — ${SITE_TITLE_SUFFIX}`

  const structured: unknown[] = [
    {
      '@context': 'https://schema.org',
      '@type': 'TechArticle',
      headline: page.title,
      description: page.description,
      url: canonical,
      dateModified: page.lastmod,
      inLanguage: 'en',
      keywords: page.tags.join(', '),
      author: { '@type': 'Organization', name: 'G ROX LTD' },
      publisher: {
        '@type': 'Organization',
        name: SITE_NAME,
        url: SITE_ORIGIN,
        logo: { '@type': 'ImageObject', url: `${SITE_ORIGIN}/logo.svg` },
      },
    },
    crumb.jsonLd,
  ]
  const faq = faqJsonLd(page)
  if (faq) structured.push(faq)
  if (ctx.extraJsonLd) structured.push(...ctx.extraJsonLd)

  const hero = page.hero
    ? `<figure class="hero"><img src="${escapeHtml(page.hero)}" alt="${escapeHtml(page.heroAlt ?? page.title)}" loading="eager" decoding="async" width="960" height="420"></figure>`
    : ''

  const toc = tocHtml(page.headings)

  return `<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>${escapeHtml(fullTitle)}</title>
<meta name="description" content="${escapeHtml(page.description)}">
<link rel="canonical" href="${canonical}">
${page.noindex ? '<meta name="robots" content="noindex, follow">\n' : ''}<meta property="og:type" content="article">
<meta property="og:site_name" content="${SITE_NAME}">
<meta property="og:title" content="${escapeHtml(page.title)}">
<meta property="og:description" content="${escapeHtml(page.description)}">
<meta property="og:url" content="${canonical}">
<meta property="og:image" content="${SITE_ORIGIN}${OG_IMAGE}">
<meta name="twitter:card" content="summary_large_image">
<meta name="twitter:title" content="${escapeHtml(page.title)}">
<meta name="twitter:description" content="${escapeHtml(page.description)}">
<meta name="twitter:image" content="${SITE_ORIGIN}${OG_IMAGE}">
<meta name="theme-color" content="#009688">
<link rel="icon" type="image/svg+xml" href="/favicon.svg">
<link rel="stylesheet" href="${BASE}/assets/docs.css">
<script src="${BASE}/assets/os-preference.js"></script>
${jsonLdBlock(structured)}
</head>
<body>
<a class="skip-link" href="#main">Skip to content</a>

<header class="topbar">
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
</header>

<div class="layout">
  <aside class="sidebar" data-nav>
    ${sidebar(nav, page)}
  </aside>

  <main id="main" class="content">
    ${crumb.html}
    <h1 class="page-title">${escapeHtml(page.title)}</h1>
    <p class="page-lead">${escapeHtml(page.description)}</p>
    ${tagChips(page, page.tags, tagIndexed)}
    ${hero}
    <div class="prose">
${page.html}
    </div>
    ${pager(prev, next)}
    <p class="page-meta">
      Last updated <time datetime="${page.lastmod}">${page.lastmod}</time>${
        page.sourceFile
          ? ` ·\n      <a href="https://github.com/gjovanov/roomler-ai/edit/master/ui/docs/content/${page.sourceFile}.md" target="_blank" rel="noopener noreferrer">Edit this page</a>`
          : ''
      }
    </p>
  </main>

  ${toc ? `<aside class="toc-rail">${toc}</aside>` : '<aside class="toc-rail"></aside>'}
</div>

<footer class="site-footer">
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
</footer>

<dialog class="search-dialog" data-search-dialog aria-label="Search documentation">
  <form class="search-form" method="dialog" role="search">
    ${icon('search', { size: 18, cls: 'search-form__icon' })}
    <input type="search" class="search-input" data-search-input placeholder="Search the docs…" autocomplete="off" spellcheck="false" aria-label="Search query">
    <button type="button" class="search-close" data-search-close aria-label="Close search">${icon('close', { size: 18 })}</button>
  </form>
  <div class="search-results" data-search-results aria-live="polite"></div>
  <p class="search-hint"><kbd>&uarr;</kbd><kbd>&darr;</kbd> to navigate · <kbd>Enter</kbd> to open · <kbd>Esc</kbd> to close</p>
</dialog>

<script src="${BASE}/assets/docs.js" defer></script>
<script src="${BASE}/assets/search.js" defer></script>
</body>
</html>
`
}
