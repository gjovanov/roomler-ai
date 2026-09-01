// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * FR-59 (#1165) — the static documentation generator.
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
 */
import { execFileSync } from 'node:child_process'
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
import { dirname, join, posix, relative, resolve, sep } from 'node:path'
import {
  BASE,
  MAX_DESCRIPTION_CHARS,
  MIN_PAGES_PER_TAG_INDEX,
  PUBLIC_SPA_ROUTES,
  SEARCH_INDEX_MAX_GZIP_BYTES,
  SECTIONS,
  SITE_ORIGIN,
  sectionByDir,
} from './site.ts'
import {
  optionalBoolean,
  optionalNumber,
  optionalString,
  parseFrontmatter,
  requireString,
  requireStringArray,
} from './theme/frontmatter.ts'
import { createRenderer, escapeHtml, renderMarkdown, slugify } from './theme/render.ts'
import { icon } from './theme/icons.ts'
import { renderPage, type DocPage, type NavSection } from './theme/layout.ts'

const DOCS_ROOT = dirname(new URL(import.meta.url).pathname.replace(/^\/([A-Za-z]:)/, '$1'))
const UI_ROOT = resolve(DOCS_ROOT, '..')
const REPO_ROOT = resolve(UI_ROOT, '..')
const CONTENT_DIR = join(DOCS_ROOT, 'content')
const THEME_DIR = join(DOCS_ROOT, 'theme')
const DIST = join(UI_ROOT, 'dist')
const OUT = join(DIST, 'docs')
const OUT_ASSETS = join(OUT, 'assets')

/** Where a frontmatter `hero:` / inline image name is looked up, in order.
 *  Reusing the tutorial's artwork is the point — it is the same product. */
const ASSET_SEARCH_PATHS = [
  join(DOCS_ROOT, 'assets'),
  join(UI_ROOT, 'src', 'assets', 'tutorial'),
  join(REPO_ROOT, 'docs', 'assets'),
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

/** One `git log` for the whole content tree rather than one per file —
 *  50 subprocesses to stamp 50 dates is a measurable share of the build. */
function lastModifiedMap(): Map<string, string> {
  const map = new Map<string, string>()
  try {
    const rel = relative(REPO_ROOT, CONTENT_DIR).split(sep).join('/')
    const log = execFileSync(
      'git',
      ['log', '--format=__C__%cs', '--name-only', '--', rel],
      { cwd: REPO_ROOT, encoding: 'utf8', maxBuffer: 32 * 1024 * 1024 },
    )
    let date = ''
    for (const line of log.split('\n')) {
      const t = line.trim()
      if (t.startsWith('__C__')) {
        date = t.slice(5)
      } else if (t && date && !map.has(t)) {
        map.set(t, date) // first sighting == most recent commit
      }
    }
  } catch {
    // Not a git checkout (a tarball build, a fresh clone with no history).
    // Falling back to today is honest: it is when these bytes were made.
  }
  return map
}

const TODAY = new Date().toISOString().slice(0, 10)

function toSlug(file: string): string {
  return relative(CONTENT_DIR, file).split(sep).join('/').replace(/\.md$/, '')
}

function slugToUrl(slug: string): string {
  if (slug === 'index') return `${BASE}/`
  const trimmed = slug.replace(/\/index$/, '')
  return `${BASE}/${trimmed}/`
}

// ── asset resolution ────────────────────────────────────────────────────

const usedAssets = new Set<string>()

function resolveAsset(name: string, where: string): string {
  const bare = name.replace(/^.*\//, '')
  for (const dir of ASSET_SEARCH_PATHS) {
    const candidate = join(dir, bare)
    if (existsSync(candidate)) {
      usedAssets.add(candidate)
      return `${BASE}/assets/${bare}`
    }
  }
  fail(
    `${where} — asset "${name}" not found. Looked in:\n` +
      ASSET_SEARCH_PATHS.map((d) => `      ${relative(REPO_ROOT, d)}`).join('\n'),
  )
  return `${BASE}/assets/${bare}`
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
const lastMod = lastModifiedMap()

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

  const { html, headings } = renderMarkdown(md, body, rel)
  const heroName = optionalString(data, 'hero')

  const page: DocPage = {
    slug,
    url: slugToUrl(slug),
    outFile: slug === 'index' ? 'index.html' : `${slug.replace(/\/index$/, '')}/index.html`,
    section,
    title,
    description,
    tags,
    order: optionalNumber(data, 'order') ?? 999,
    hero: heroName ? resolveAsset(heroName, rel) : undefined,
    heroAlt: optionalString(data, 'heroAlt'),
    noindex: optionalBoolean(data, 'noindex') ?? false,
    html,
    headings,
    plain: '',
    faq: optionalBoolean(data, 'faq') ?? false,
    lastmod: lastMod.get(rel) ?? TODAY,
  }
  page.plain = stripTags(html)
  return page
}

// ── generated pages (section indexes, tag indexes) ──────────────────────

function sectionIndexBody(section: (typeof SECTIONS)[number], pages: DocPage[]): string {
  const cards = pages
    .map(
      (p) =>
        `<a class="section-card" href="${p.url}">` +
        `<h3 class="section-card__title">${escapeHtml(p.title)}</h3>` +
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
  const listing = sectionIndexBody(section, pages)
  if (authored) {
    // Authored prose stays first; the generated listing is appended, so a
    // new page in the section shows up without anyone editing an index.
    authored.html = `${authored.html}\n<h2 id="in-this-section">In this section</h2>\n${listing}`
    authored.headings = [
      ...authored.headings,
      { level: 2, text: 'In this section', slug: 'in-this-section' },
    ]
    authored.plain = stripTags(authored.html)
    return authored
  }
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
    lastmod: TODAY,
  }
}

function makeTagIndex(tag: string, pages: DocPage[]): DocPage {
  const cards = pages
    .map(
      (p) =>
        `<a class="section-card" href="${p.url}">` +
        `<h3 class="section-card__title">${escapeHtml(p.title)}</h3>` +
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
    lastmod: TODAY,
  }
}

// ── link checking ───────────────────────────────────────────────────────

function checkLinks(pages: DocPage[]): void {
  const byUrl = new Map(pages.map((p) => [p.url, p]))
  const anchors = new Map(pages.map((p) => [p.url, new Set(p.headings.map((h) => h.slug))]))

  for (const page of pages) {
    const re = /href="([^"]+)"/g
    let m: RegExpExecArray | null
    while ((m = re.exec(page.html)) !== null) {
      const href = m[1]!
      if (/^(https?:|mailto:|tel:|#)/i.test(href)) continue

      // Resolve relative hrefs against this page's URL so authors can write
      // `../network/exit-nodes/` as well as the site-absolute form.
      const [rawPath, hash] = href.split('#')
      const target = rawPath!.startsWith('/')
        ? rawPath!
        : posix.normalize(posix.join(page.url, rawPath!))
      const normalised = target.endsWith('/') || /\.[a-z0-9]+$/i.test(target) ? target : `${target}/`

      // Non-docs internal links point at the SPA (/landing, /register …);
      // this generator does not own those routes, so it cannot verify them.
      if (!normalised.startsWith(`${BASE}/`)) continue
      // A file reference (an asset) is verified by resolveAsset, not here.
      if (/\.[a-z0-9]+$/i.test(normalised)) continue

      if (!byUrl.has(normalised)) {
        fail(`${page.slug}.md — link "${href}" points at ${normalised}, which no page generates`)
        continue
      }
      if (hash) {
        const set = anchors.get(normalised)!
        if (!set.has(hash)) {
          fail(
            `${page.slug}.md — link "${href}" points at #${hash} on ${normalised}, ` +
              `which has no heading with that id`,
          )
        }
      }
    }
  }
}

// ── output ──────────────────────────────────────────────────────────────

function write(file: string, contents: string | Buffer): void {
  mkdirSync(dirname(file), { recursive: true })
  writeFileSync(file, contents)
}

function buildSitemap(pages: DocPage[]): string {
  const urls = [
    ...pages.filter((p) => !p.noindex).map((p) => ({ loc: `${SITE_ORIGIN}${p.url}`, lastmod: p.lastmod })),
    ...PUBLIC_SPA_ROUTES.map((r) => ({ loc: `${SITE_ORIGIN}${r}`, lastmod: TODAY })),
  ]
  return (
    `<?xml version="1.0" encoding="UTF-8"?>\n` +
    `<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">\n` +
    urls
      .map((u) => `  <url>\n    <loc>${u.loc}</loc>\n    <lastmod>${u.lastmod}</lastmod>\n  </url>`)
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

  checkLinks(renderable)

  if (errors.length) {
    console.error(`\n[docs] BUILD FAILED — ${errors.length} problem(s):\n`)
    for (const e of errors) console.error(`  • ${e}`)
    console.error('')
    process.exit(1)
  }

  // ── emit ──────────────────────────────────────────────────────────────
  rmSync(OUT, { recursive: true, force: true })
  mkdirSync(OUT_ASSETS, { recursive: true })

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
        prev: i > 0 ? flow[i - 1] : undefined,
        next: i >= 0 && i < flow.length - 1 ? flow[i + 1] : undefined,
      },
      tagIndexed,
    )
    write(join(OUT, page.outFile), html)
  }

  // Search index.
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
  write(join(OUT_ASSETS, 'search-index.json'), indexJson)

  // Theme assets.
  for (const f of ['docs.css', 'docs.js', 'search.js', 'os-preference.js']) {
    cpSync(join(THEME_DIR, f), join(OUT_ASSETS, f))
  }
  for (const asset of usedAssets) {
    cpSync(asset, join(OUT_ASSETS, asset.split(sep).pop()!))
  }
  const social = join(REPO_ROOT, 'docs', 'assets', 'social-preview.png')
  if (existsSync(social)) cpSync(social, join(OUT_ASSETS, 'social-preview.png'))
  else console.warn('[docs] warning: docs/assets/social-preview.png missing — OG image will 404')

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
  console.log(
    `[docs] ${renderable.length} pages · ${records.length} search records · ` +
      `index ${(gz / 1024).toFixed(1)} KB gz · ${tagPages.length} tag indexes · ${ms} ms`,
  )
}

main()
