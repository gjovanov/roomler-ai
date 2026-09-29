// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * FR-60 (#1165) — the static documentation generator.
 *
 *   bun docs/build.ts            (from ui/, after `vite build`)
 *
 * Ordering matters: `vite build` empties `dist/`, so this MUST run after
 * it. It writes `dist/docs/**` plus `dist/sitemap.xml` and
 * `dist/robots.txt` at the site root.
 *
 * Everything below that can be a BUILD GATE is one. A docs site whose own
 * navigation 404s costs more SEO than the site earns, so a dangling
 * internal link fails the build rather than logging a warning nobody
 * reads. Same for a missing description (a search engine invents the
 * snippet), a duplicate slug (two pages competing for one URL), and a
 * search index that outgrew its budget (a page-load cost nobody decided
 * to spend).
 *
 * FR-87 (#1776) added honest dates (`dates.ts`: an unknown date is omitted,
 * never the build date), content-hashed asset names (`theme/assets.ts`),
 * images with their real size (`theme/images.ts`), and the 404 page nginx
 * serves for any missing `/docs/` URL.
 */
import { gzipSync } from 'node:zlib'
import {
  cpSync,
  existsSync,
  mkdirSync,
  readFileSync,
  readdirSync,
  rmSync,
  statSync,
  writeFileSync,
} from 'node:fs'
import { dirname, join, relative, resolve, sep } from 'node:path'
import { resolveDates } from './dates.ts'
import {
  BASE,
  LEGACY_UNHASHED_ASSETS,
  MAX_DESCRIPTION_CHARS,
  MIN_PAGES_PER_TAG_INDEX,
  PUBLIC_SPA_ROUTES,
  SEARCH_INDEX_MAX_GZIP_BYTES,
  SECTIONS,
  SITE_ORIGIN,
  sectionByDir,
} from './site.ts'
import { AssetEmitter } from './theme/assets.ts'
import {
  optionalBoolean,
  optionalNumber,
  optionalString,
  parseFrontmatter,
  requireString,
  requireStringArray,
  type Frontmatter,
} from './theme/frontmatter.ts'
import { imageSize, MAX_IMAGE_BYTES } from './theme/images.ts'
import { checkLinks } from './theme/links.ts'
import { createRenderer, escapeHtml, renderMarkdown, type ResolvedImage } from './theme/render.ts'
import { renderPage, type DocPage, type NavSection, type SiteAssets } from './theme/layout.ts'

const DOCS_ROOT = dirname(new URL(import.meta.url).pathname.replace(/^\/([A-Za-z]:)/, '$1'))
const UI_ROOT = resolve(DOCS_ROOT, '..')
const REPO_ROOT = resolve(UI_ROOT, '..')
const CONTENT_DIR = join(DOCS_ROOT, 'content')
const THEME_DIR = join(DOCS_ROOT, 'theme')
const DIST = join(UI_ROOT, 'dist')
const OUT = join(DIST, 'docs')
const OUT_ASSETS = join(OUT, 'assets')

/**
 * Where a frontmatter `hero:` / inline image name is looked up, in order.
 * Reusing the tutorial's artwork is the point — it is the same product.
 *
 * ⚠️ EVERY path must be inside `ui/`. The Dockerfile's UI stage is
 * `COPY ui/ .` and nothing else, so an asset resolved from the repo root
 * exists on a dev box and is ABSENT in the production image. The repo's
 * `docs/assets/` used to be searched here; it is deliberately gone,
 * because a hero added from there would build green locally and fail — or
 * worse, 404 — in the image. Put shared artwork in `ui/docs/assets/`.
 */
const ASSET_SEARCH_PATHS = [
  join(DOCS_ROOT, 'assets'),
  join(UI_ROOT, 'src', 'assets', 'tutorial'),
]

const errors: string[] = []
function fail(msg: string): void {
  errors.push(msg)
}

// ── content discovery ───────────────────────────────────────────────────

function walk(dir: string, out: string[] = []): string[] {
  if (!existsSync(dir)) return out
  for (const entry of readdirSync(dir)) {
    const full = join(dir, entry)
    if (statSync(full).isDirectory()) walk(full, out)
    else if (entry.endsWith('.md')) out.push(full)
  }
  return out
}

// ── dates (FR-87) ───────────────────────────────────────────────────────
// FR-60 fell back to the build date here, calling it "when these bytes were
// made". Crawlers read `lastmod` as when the CONTENT changed, and production
// never had git, so every page claimed to change on every deploy.

const dates = resolveDates()
const ISO_DAY = /^\d{4}-\d{2}-\d{2}$/
/** Only for refusing a future date; never published. */
const NOW_DAY = new Date().toISOString().slice(0, 10)

/** A front-matter date: `YYYY-MM-DD`, real, and not in the future. */
function frontMatterDate(data: Frontmatter, key: string, rel: string): string | undefined {
  const v = optionalString(data, key)
  if (v === undefined) return undefined
  if (!ISO_DAY.test(v) || Number.isNaN(Date.parse(v))) {
    fail(`${rel} — \`${key}: ${v}\` is not a YYYY-MM-DD date`)
    return undefined
  }
  if (v > NOW_DAY) {
    fail(`${rel} — \`${key}: ${v}\` is in the future`)
    return undefined
  }
  return v
}

function toSlug(file: string): string {
  return relative(CONTENT_DIR, file).split(sep).join('/').replace(/\.md$/, '')
}

function slugToUrl(slug: string): string {
  if (slug === 'index') return `${BASE}/`
  const trimmed = slug.replace(/\/index$/, '')
  return `${BASE}/${trimmed}/`
}

// ── assets ──────────────────────────────────────────────────────────────

const assets = new AssetEmitter(OUT_ASSETS, `${BASE}/assets`, LEGACY_UNHASHED_ASSETS)

function findAsset(name: string, where: string): string | null {
  const bare = name.replace(/^.*\//, '')
  for (const dir of ASSET_SEARCH_PATHS) {
    const candidate = join(dir, bare)
    if (existsSync(candidate)) return candidate
  }
  fail(
    `${where} — asset "${name}" not found. Looked in:\n` +
      ASSET_SEARCH_PATHS.map((d) => `      ${relative(REPO_ROOT, d)}`).join('\n'),
  )
  return null
}

/** A hero or markdown image: found, size-checked, measured, and published
 *  under its hashed name. Null after reporting why it cannot be. */
function publishImage(name: string, where: string): ResolvedImage | null {
  const file = findAsset(name, where)
  if (!file) return null
  const bytes = readFileSync(file)
  if (bytes.length > MAX_IMAGE_BYTES) {
    fail(
      `${where} — image "${name}" is ${(bytes.length / 1048576).toFixed(1)} MB; the limit is ` +
        `${MAX_IMAGE_BYTES / 1048576} MB. The site serves images as they are, so resize or recompress it.`,
    )
    return null
  }
  try {
    return { url: assets.publishFile(file), ...imageSize(bytes) }
  } catch (err) {
    fail(`${where} — image "${name}": ${err instanceof Error ? err.message : String(err)}`)
    return null
  }
}

// ── plain text + per-section excerpts ───────────────────────────────────

function stripTags(html: string): string {
  return html
    .replace(/<pre[\s\S]*?<\/pre>/g, ' ')
    .replace(/<[^>]+>/g, ' ')
    .replace(/&amp;/g, '&')
    .replace(/&lt;/g, '<')
    .replace(/&gt;/g, '>')
    .replace(/&quot;/g, '"')
    .replace(/&#\d+;/g, ' ')
    .replace(/\s+/g, ' ')
    .trim()
}

interface IndexRecord {
  p: number
  h: string
  a: string
  x: string
  g: string[]
}

/** Split rendered HTML at h2 boundaries so search results deep-link to the
 *  right part of a long page instead of dumping the reader at the top. */
function sectionChunks(page: DocPage): Array<{ heading: string; anchor: string; text: string }> {
  const parts: Array<{ heading: string; anchor: string; text: string }> = []
  const re = /<h2[^>]*\bid="([^"]+)"[^>]*>([\s\S]*?)<\/h2>/g
  const marks: Array<{ idx: number; end: number; slug: string; text: string }> = []
  let m: RegExpExecArray | null
  while ((m = re.exec(page.html)) !== null) {
    marks.push({ idx: m.index, end: re.lastIndex, slug: m[1]!, text: stripTags(m[2]!) })
  }

  const intro = stripTags(page.html.slice(0, marks.length ? marks[0]!.idx : page.html.length))
  parts.push({ heading: '', anchor: '', text: `${page.description} ${intro}`.trim() })

  for (const [i, mk] of marks.entries()) {
    const end = marks[i + 1]?.idx ?? page.html.length
    parts.push({ heading: mk.text, anchor: mk.slug, text: stripTags(page.html.slice(mk.end, end)) })
  }
  return parts
}

// ── page loading ────────────────────────────────────────────────────────

const md = createRenderer()

function loadPage(file: string): DocPage {
  const rel = relative(REPO_ROOT, file).split(sep).join('/')
  const raw = readFileSync(file, 'utf8')
  const { data, body } = parseFrontmatter(raw, rel)

  const slug = toSlug(file)
  const title = requireString(data, 'title', rel)
  const description = requireString(data, 'description', rel)
  const tags = requireStringArray(data, 'tags', rel)

  if (description.length > MAX_DESCRIPTION_CHARS) {
    fail(
      `${rel} — frontmatter \`description\` is ${description.length} chars; ` +
        `the limit is ${MAX_DESCRIPTION_CHARS} (longer is silently truncated in search results)`,
    )
  }

  const sectionDir = slug.includes('/') ? slug.split('/')[0]! : undefined
  const section = sectionDir ? sectionByDir(sectionDir) : undefined
  if (sectionDir && !section) {
    fail(`${rel} — directory "${sectionDir}" is not a declared section in ui/docs/site.ts`)
  }

  const { html, headings } = renderMarkdown(md, body, rel, {
    resolveImage: (src) => publishImage(src, rel),
    fail,
  })
  const heroName = optionalString(data, 'hero')
  const git = dates.files.get(rel)

  const page: DocPage = {
    slug,
    url: slugToUrl(slug),
    outFile: slug === 'index' ? 'index.html' : `${slug.replace(/\/index$/, '')}/index.html`,
    section,
    title,
    description,
    tags,
    order: optionalNumber(data, 'order') ?? 999,
    hero: heroName ? (publishImage(heroName, rel) ?? undefined) : undefined,
    heroAlt: optionalString(data, 'heroAlt'),
    noindex: optionalBoolean(data, 'noindex') ?? false,
    html,
    headings,
    plain: '',
    faq: optionalBoolean(data, 'faq') ?? false,
    // front-matter `updated` > git > manifest > nothing. `updated` exists for
    // the commit that touched every page without changing what one says.
    lastmod: frontMatterDate(data, 'updated', rel) ?? git?.modified,
    created: git?.created,
    sourceFile: slug,
  }
  page.plain = stripTags(html)
  return page
}

// ── generated pages (section indexes, tag indexes) ──────────────────────

/** @param level `2` when the grid sits directly under the page's `<h1>`,
 *  `3` under the "In this section" `<h2>` an authored index gets. */
function sectionIndexBody(pages: DocPage[], level: 2 | 3): string {
  const cards = pages
    .map(
      (p) =>
        `<a class="section-card" href="${p.url}">` +
        `<h${level} class="section-card__title">${escapeHtml(p.title)}</h${level}>` +
        `<p class="section-card__blurb">${escapeHtml(p.description)}</p></a>`,
    )
    .join('')
  return `<div class="section-grid">${cards}</div>`
}

function makeSectionIndex(
  section: (typeof SECTIONS)[number],
  pages: DocPage[],
  authored: DocPage | undefined,
): DocPage {
  if (authored) {
    // Authored prose stays first; the generated listing is appended, so a
    // new page in the section shows up without anyone editing an index.
    const listing = sectionIndexBody(pages, 3)
    authored.html = `${authored.html}\n<h2 id="in-this-section">In this section</h2>\n${listing}`
    authored.headings = [
      ...authored.headings,
      { level: 2, text: 'In this section', slug: 'in-this-section' },
    ]
    authored.plain = stripTags(authored.html)
    return authored
  }
  const listing = sectionIndexBody(pages, 2)
  // No `lastmod`: no file produces this page, so git has no date for it, and
  // the build date is not one (FR-87).
  return {
    slug: `${section.dir}/index`,
    url: `${BASE}/${section.dir}/`,
    outFile: `${section.dir}/index.html`,
    section,
    title: section.title,
    description: section.blurb,
    tags: [section.dir],
    order: -1,
    noindex: false,
    html: listing,
    headings: [],
    plain: stripTags(listing),
    faq: false,
  }
}

function makeTagIndex(tag: string, pages: DocPage[]): DocPage {
  const cards = pages
    .map(
      (p) =>
        `<a class="section-card" href="${p.url}">` +
        `<h2 class="section-card__title">${escapeHtml(p.title)}</h2>` +
        `<p class="section-card__blurb">${escapeHtml(p.description)}</p>` +
        `<span class="section-card__count">${escapeHtml(p.section?.title ?? 'Docs')}</span></a>`,
    )
    .join('')
  const html = `<div class="section-grid">${cards}</div>`
  return {
    slug: `tags/${tag}`,
    url: `${BASE}/tags/${encodeURIComponent(tag)}/`,
    outFile: `tags/${tag}/index.html`,
    section: undefined,
    title: `${tag}`,
    description: `Every Roomler documentation page tagged “${tag}”.`,
    tags: [tag],
    order: 999,
    noindex: false,
    html,
    headings: [],
    plain: stripTags(html),
    faq: false,
  }
}

/** Served by nginx for any missing `/docs/` (and `/blog/`) URL — FR-87 P1's
 *  `error_page 404`. Every link in it is site-absolute, because it answers
 *  at whatever depth the missing URL had. */
function makeNotFoundPage(nav: NavSection[]): DocPage {
  const sections = nav
    .map(({ section }) => `<li><a href="${BASE}/${section.dir}/">${escapeHtml(section.title)}</a></li>`)
    .join('')
  const html =
    `<p>Nothing is published at this address. The page may have moved, or the link that brought you here may be wrong.</p>\n` +
    `<p><button class="btn btn--primary" type="button" data-search-open>Search the documentation</button></p>\n` +
    `<p>Or start from a section:</p>\n<ul><li><a href="${BASE}/">Documentation home</a></li>${sections}</ul>`
  return {
    slug: '404',
    url: `${BASE}/404.html`,
    outFile: '404.html',
    title: 'Page not found',
    description: 'This page does not exist. Search the documentation, or start from one of its sections.',
    tags: [],
    order: 999,
    noindex: true,
    notFound: true,
    html,
    headings: [],
    plain: '',
    faq: false,
  }
}

// ── output ──────────────────────────────────────────────────────────────

function write(file: string, contents: string | Buffer): void {
  mkdirSync(dirname(file), { recursive: true })
  writeFileSync(file, contents)
}

function buildSitemap(pages: DocPage[]): string {
  const urls: Array<{ loc: string; lastmod?: string }> = [
    ...pages.filter((p) => !p.noindex).map((p) => ({ loc: `${SITE_ORIGIN}${p.url}`, lastmod: p.lastmod })),
    // Undated: the SPA's pages are not dated from git (FR-87).
    ...PUBLIC_SPA_ROUTES.map((r) => ({ loc: `${SITE_ORIGIN}${r}` })),
  ]
  return (
    `<?xml version="1.0" encoding="UTF-8"?>\n` +
    `<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">\n` +
    urls
      .map(
        (u) =>
          `  <url>\n    <loc>${u.loc}</loc>\n` +
          (u.lastmod ? `    <lastmod>${u.lastmod}</lastmod>\n` : '') +
          `  </url>`,
      )
      .join('\n') +
    `\n</urlset>\n`
  )
}

function buildRobots(): string {
  return (
    `# Roomler — https://roomler.ai\n` +
    `User-agent: *\n` +
    `Allow: /\n` +
    `# The application itself is behind auth and client-rendered; there is\n` +
    `# nothing there for a crawler, and tenant ids should not be enumerated.\n` +
    `Disallow: /tenant/\n` +
    `Disallow: /oauth/\n` +
    `Disallow: /consent/\n` +
    `Disallow: /invite/\n` +
    `\n` +
    `Sitemap: ${SITE_ORIGIN}/sitemap.xml\n`
  )
}

// ── main ────────────────────────────────────────────────────────────────

function main(): void {
  const t0 = Date.now()

  if (!existsSync(CONTENT_DIR)) {
    console.error(`[docs] no content directory at ${CONTENT_DIR}`)
    process.exit(1)
  }

  const files = walk(CONTENT_DIR).sort()

  // Collect per-file failures instead of throwing on the first one. With
  // ~65 pages, aborting on file 3 means one build run per mistake — and a
  // raw stack trace where the other gates print an actionable list.
  const loaded: DocPage[] = []
  for (const file of files) {
    try {
      loaded.push(loadPage(file))
    } catch (err) {
      fail(err instanceof Error ? err.message : String(err))
    }
  }

  // Duplicate URLs: two files competing for one address.
  const seen = new Map<string, string>()
  for (const p of loaded) {
    const prev = seen.get(p.url)
    if (prev) fail(`duplicate URL ${p.url} — produced by both ${prev}.md and ${p.slug}.md`)
    seen.set(p.url, p.slug)
  }

  // Assemble sections. A section index is generated when nobody authored
  // one, so adding a page never requires editing an index by hand.
  const home = loaded.find((p) => p.slug === 'index')
  if (!home) fail('ui/docs/content/index.md is required (it is the /docs/ landing page)')

  const nav: NavSection[] = []
  const allPages: DocPage[] = home ? [home] : []

  for (const section of SECTIONS) {
    const own = loaded.filter((p) => p.slug.startsWith(`${section.dir}/`))
    const authoredIndex = own.find((p) => p.slug === `${section.dir}/index`)
    const leaves = own
      .filter((p) => p !== authoredIndex)
      .sort((a, b) => a.order - b.order || a.title.localeCompare(b.title))
    if (leaves.length === 0 && !authoredIndex) continue

    const index = makeSectionIndex(section, leaves, authoredIndex)
    nav.push({ section, pages: leaves })
    allPages.push(index, ...leaves)
  }

  // Pages in a directory that is not a declared section were already
  // reported by loadPage; carry them so their errors are not the only trace.
  for (const p of loaded) {
    if (!allPages.includes(p)) allPages.push(p)
  }

  // Tag indexes, above the doorway-page threshold only.
  const byTag = new Map<string, DocPage[]>()
  for (const p of allPages) {
    for (const t of p.tags) {
      if (!byTag.has(t)) byTag.set(t, [])
      byTag.get(t)!.push(p)
    }
  }
  const tagIndexed = new Set<string>()
  const tagPages: DocPage[] = []
  for (const [tag, pages] of [...byTag.entries()].sort()) {
    if (pages.length < MIN_PAGES_PER_TAG_INDEX) continue
    tagIndexed.add(tag)
    tagPages.push(makeTagIndex(tag, pages))
  }
  const renderable = [...allPages, ...tagPages]

  const linkErrors = checkLinks(
    renderable.map((p) => ({
      id: p.sourceFile ? `${p.sourceFile}.md` : p.url,
      url: p.url,
      html: p.html,
      anchors: new Set(p.headings.map((h) => h.slug)),
    })),
    // Other internal links point at the SPA (/landing, /register …); this
    // generator does not own those routes, so it cannot verify them.
    (path) => path.startsWith(`${BASE}/`),
  )
  for (const e of linkErrors) fail(e)

  if (errors.length) {
    console.error(`\n[docs] BUILD FAILED — ${errors.length} problem(s):\n`)
    for (const e of errors) console.error(`  • ${e}`)
    console.error('')
    process.exit(1)
  }

  // ── emit ──────────────────────────────────────────────────────────────
  rmSync(OUT, { recursive: true, force: true })
  mkdirSync(OUT_ASSETS, { recursive: true })

  // Search index. Built BEFORE the pages are rendered: its name is
  // content-hashed, and every page carries that name.
  //
  // ⚠️ Tag indexes are excluded. Their entire content is the titles and
  // descriptions of pages that are already in the index, so including them
  // would return the same page twice for one query — once as itself and
  // once as a tag listing that merely mentions it. The `tag:` filter in
  // `search.js` is the better answer to "everything about windows", and it
  // works off the real pages.
  const idxPages: Array<{ u: string; t: string; s: string }> = []
  const records: IndexRecord[] = []
  for (const page of allPages) {
    if (page.noindex) continue
    const pi = idxPages.length
    idxPages.push({ u: page.url, t: page.title, s: page.section?.title ?? 'Docs' })
    for (const chunk of sectionChunks(page)) {
      if (!chunk.text && !chunk.heading) continue
      records.push({
        p: pi,
        h: chunk.heading,
        a: chunk.anchor,
        x: chunk.text.slice(0, 420),
        g: page.tags,
      })
    }
  }
  const indexJson = JSON.stringify({ p: idxPages, r: records })
  const gz = gzipSync(Buffer.from(indexJson)).length
  if (gz > SEARCH_INDEX_MAX_GZIP_BYTES) {
    console.error(
      `\n[docs] BUILD FAILED — search index is ${(gz / 1024).toFixed(1)} KB gzipped, ` +
        `over the ${(SEARCH_INDEX_MAX_GZIP_BYTES / 1024).toFixed(0)} KB budget.\n` +
        `        Raise SEARCH_INDEX_MAX_GZIP_BYTES in ui/docs/site.ts deliberately, ` +
        `or shrink the per-record excerpt.\n`,
    )
    process.exit(1)
  }

  const siteAssets: SiteAssets = {
    css: assets.publishFile(join(THEME_DIR, 'docs.css')),
    js: assets.publishFile(join(THEME_DIR, 'docs.js')),
    search: assets.publishFile(join(THEME_DIR, 'search.js')),
    osPreference: assets.publishFile(join(THEME_DIR, 'os-preference.js')),
    searchIndex: assets.publishBytes('search-index.json', indexJson),
  }

  // Reading order for prev/next is the sidebar order: sections in declared
  // order, pages within them in `order` then title.
  const flow: DocPage[] = [
    ...(home ? [home] : []),
    ...nav.flatMap(({ section, pages }) => {
      const idx = allPages.find((p) => p.url === `${BASE}/${section.dir}/`)
      return idx ? [idx, ...pages] : pages
    }),
  ]

  for (const page of renderable) {
    const i = flow.indexOf(page)
    const html = renderPage(
      {
        nav,
        page,
        assets: siteAssets,
        prev: i > 0 ? flow[i - 1] : undefined,
        next: i >= 0 && i < flow.length - 1 ? flow[i + 1] : undefined,
      },
      tagIndexed,
    )
    write(join(OUT, page.outFile), html)
  }
  write(join(OUT, '404.html'), renderPage({ nav, page: makeNotFoundPage(nav), assets: siteAssets }, tagIndexed))

  // Every hashed file planned above — theme, search index, heroes, images.
  assets.flush()

  // ⚠️ The OG image must live INSIDE `ui/`. The Docker UI stage is
  // `COPY ui/ .` and nothing else, so a card read from the repo's
  // `docs/assets/` exists on a dev box and is ABSENT in the image — every
  // page's `og:image` would 404 in production while looking perfect
  // locally. Same class as the `@types/node` optional-peer divergence.
  //
  // A gate rather than a warning, for the same reason as the other gates:
  // this URL is referenced by all 96 pages, and a warning in a Docker build
  // log is precisely the thing nobody reads.
  const social = join(DOCS_ROOT, 'assets', 'social-preview.png')
  if (!existsSync(social)) {
    console.error(
      `\n[docs] BUILD FAILED — the Open Graph image is missing:\n` +
        `        ${relative(REPO_ROOT, social)}\n` +
        `        Every page references it as og:image, and it MUST live under ui/ —\n` +
        `        the Docker UI stage copies ui/ and nothing else.\n`,
    )
    process.exit(1)
  }
  cpSync(social, join(OUT_ASSETS, 'social-preview.png'))

  // Site-root SEO files.
  //
  // ⚠️ The sitemap lists DOCUMENTATION pages, not tag indexes. A sitemap is
  // what we actively ask a crawler to index, and tag pages exist for
  // readers navigating by chip — their content is other pages' titles. At
  // 31 tag pages against 65 real ones, promoting them would make a third of
  // what we submit thin listing pages, which is how a tag system reads as
  // doorway pages. They stay crawlable via their links; they are just not
  // advertised.
  write(join(DIST, 'sitemap.xml'), buildSitemap(allPages))
  write(join(DIST, 'robots.txt'), buildRobots())

  const ms = Date.now() - t0
  // The date source is in the log because its absence is otherwise silent:
  // an image built without the manifest looks perfect and publishes no dates.
  const dated = allPages.filter((p) => p.lastmod).length
  const dateNote =
    dates.source === 'none'
      ? `dates: none (${dates.note})`
      : `dates: ${dates.source}, ${dated} of ${allPages.length} pages`
  console.log(
    `[docs] ${renderable.length} pages · ${records.length} search records · ` +
      `index ${(gz / 1024).toFixed(1)} KB gz · ${tagPages.length} tag indexes · ` +
      `${assets.names().length} assets · ${dateNote} · ${ms} ms`,
  )
}

main()
