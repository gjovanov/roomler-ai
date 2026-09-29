// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * FR-60 (#1165), moved here by FR-87 (#1776) — the internal-link gate.
 *
 * A site whose own navigation 404s costs more search standing than the site
 * earns, so a link to a page nobody generates, or to a heading that is not
 * there, FAILS the build. It lives in its own module so it can be tested,
 * and so the blog's links into the docs are checked by the same code as the
 * docs' own.
 */
import { posix } from 'node:path'

export interface LinkPage {
  /** How errors name the page: its source file, or the URL of a generated page. */
  id: string
  /** Site-absolute URL with a trailing slash. */
  url: string
  /** Rendered body HTML — the only part an author writes links into. */
  html: string
  /** Heading ids on the page. */
  anchors: Set<string>
}

/**
 * Returns one message per broken link, empty when all resolve.
 *
 * @param owns whether this generator produces the URL path. A link outside it
 *   (`/register`, the SPA's routes) cannot be verified here and is skipped.
 */
export function checkLinks(pages: LinkPage[], owns: (path: string) => boolean): string[] {
  const errors: string[] = []
  const byUrl = new Map(pages.map((p) => [p.url, p]))

  for (const page of pages) {
    for (const m of page.html.matchAll(/href="([^"]+)"/g)) {
      const href = m[1]!
      if (/^(https?:|mailto:|tel:)/i.test(href)) continue

      // Same-page anchor: `[see below](#install)`.
      if (href.startsWith('#')) {
        const hash = href.slice(1)
        if (hash && !page.anchors.has(hash)) {
          errors.push(`${page.id} — link "${href}" points at a heading this page does not have`)
        }
        continue
      }

      // Resolve relative hrefs against the page's own URL, so authors can
      // write `../network/exit-nodes/` as well as the site-absolute form.
      const [rawPath, hash] = href.split('#')
      const target = rawPath!.startsWith('/') ? rawPath! : posix.normalize(posix.join(page.url, rawPath!))
      const isFile = /\.[a-z0-9]+$/i.test(target)
      const normalised = target.endsWith('/') || isFile ? target : `${target}/`

      if (!owns(normalised)) continue
      // A file (an asset) is verified where it is resolved, not here.
      if (isFile) continue

      const dest = byUrl.get(normalised)
      if (!dest) {
        errors.push(`${page.id} — link "${href}" points at ${normalised}, which no page generates`)
        continue
      }
      if (hash && !dest.anchors.has(hash)) {
        errors.push(`${page.id} — link "${href}" points at #${hash} on ${normalised}, which has no heading with that id`)
      }
    }
  }
  return errors
}
