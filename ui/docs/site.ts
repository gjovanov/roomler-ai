// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * FR-60 (#1165) — site-wide configuration for the static docs generator.
 *
 * The nav is NOT a hand-maintained list of pages. Sections are declared
 * here (order, title, blurb, icon); the pages inside them are DISCOVERED
 * by scanning `content/<section>/**\/*.md`. That is deliberate: a nav
 * entry pointing at a page nobody wrote is the single most common way a
 * docs site 404s its own navigation, and it cannot happen if the nav is
 * derived from the files that exist.
 */

/** Absolute origin. Canonicals, OG URLs and the sitemap are absolute — a
 *  relative canonical is legal but silently useless when a crawler reaches
 *  the page through any other host (a preview deploy, a staging origin). */
export const SITE_ORIGIN = 'https://roomler.ai'

/** Everything is served under this path prefix by nginx. */
export const BASE = '/docs'

export const SITE_NAME = 'Roomler'
export const SITE_TITLE_SUFFIX = 'Roomler Docs'

/** Social card. Lives in the repo already (`docs/assets/social-preview.png`)
 *  and is copied into the output by the build. */
export const OG_IMAGE = `${BASE}/assets/social-preview.png`

/** Hard ceiling on the search index. Above this the build FAILS rather
 *  than shipping a page-load cost nobody decided to spend. */
export const SEARCH_INDEX_MAX_GZIP_BYTES = 150 * 1024

/** A `<meta name="description">` past this is truncated in results, so a
 *  longer one is a silent defect. Build gate, not a lint. */
export const MAX_DESCRIPTION_CHARS = 160

/**
 * A tag index page is only generated at or above this many pages. Below it,
 * a wall of one-link pages reads to a crawler as doorway pages — a penalty,
 * not an optimisation.
 *
 * ⚠️ Tag pages are NAVIGATION, not content: they are deliberately kept out
 * of `sitemap.xml` and out of the search index (see `build.ts`). Their whole
 * body is other pages' titles, so promoting them would submit a third of the
 * site as thin listings and return every page twice in search.
 */
export const MIN_PAGES_PER_TAG_INDEX = 3

export interface SectionDef {
  /** Directory under `content/`, and the first URL segment. */
  dir: string
  title: string
  /** One line, shown on the home page card and in the sidebar header. */
  blurb: string
  /** Key into ICONS (`theme/icons.ts`). */
  icon: string
  /** Accent colour for the section's card and heading rule. */
  accent: 'teal' | 'coral' | 'deep'
}

/**
 * Section order IS the sidebar order and the sitemap order. Reading order
 * follows the product's own pivot (#490): remote access first, the private
 * network second, collaboration as the included bonus — then the
 * cross-cutting material, then reference.
 */
export const SECTIONS: SectionDef[] = [
  {
    dir: 'start',
    title: 'Get started',
    blurb: 'Install Roomler on Windows, macOS or Linux and reach your first device',
    icon: 'flag',
    accent: 'teal',
  },
  {
    dir: 'remote-desktop',
    title: 'Remote desktop',
    blurb: 'Use any of your machines from a browser tab',
    icon: 'monitor',
    accent: 'coral',
  },
  {
    dir: 'network',
    title: 'Private network',
    blurb: 'A WireGuard-style mesh, tunnels, exit nodes and SSH',
    icon: 'network',
    accent: 'teal',
  },
  {
    dir: 'collaboration',
    title: 'Chat & video',
    blurb: 'Rooms, threaded chat, HD calls and file sharing',
    icon: 'video',
    accent: 'deep',
  },
  {
    dir: 'architecture',
    title: 'Architecture',
    blurb: 'How the control plane and the three data planes fit together',
    icon: 'blueprint',
    accent: 'coral',
  },
  {
    dir: 'security',
    title: 'Security & access control',
    blurb: 'What the server can and cannot see, and who may reach what',
    icon: 'shield',
    accent: 'teal',
  },
  {
    dir: 'troubleshooting',
    title: 'Troubleshooting',
    blurb: 'When a device is offline, a screen is black, or a call has no media',
    icon: 'wrench',
    accent: 'coral',
  },
  {
    dir: 'reference',
    title: 'Reference',
    blurb: 'CLI, configuration keys, ports and the HTTP API',
    icon: 'book',
    accent: 'deep',
  },
  {
    dir: 'faq',
    title: 'FAQ',
    blurb: 'Short answers to the questions people actually ask',
    icon: 'help',
    accent: 'teal',
  },
  {
    dir: 'compare',
    title: 'How Roomler compares',
    blurb: 'Against Tailscale, RustDesk, TeamViewer, MeshCentral and NetBird',
    icon: 'compare',
    accent: 'coral',
  },
]

export function sectionByDir(dir: string): SectionDef | undefined {
  return SECTIONS.find((s) => s.dir === dir)
}

/**
 * Public SPA routes that belong in the sitemap. The app itself is
 * client-rendered and behind auth from `/` down, so only the guest-visible
 * marketing and legal routes are listed. `/tenant/**` is `Disallow`ed in
 * robots.txt for the same reason.
 */
export const PUBLIC_SPA_ROUTES = ['/landing', '/pricing', '/privacy', '/terms', '/imprint']
