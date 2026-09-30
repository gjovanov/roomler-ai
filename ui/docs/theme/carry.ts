// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * FR-88 (#1790) — the build's half of the static pages' carry script.
 *
 * `attribution.js` is plain JavaScript served to browsers, so it cannot import
 * `INSTALL_PAGES` from `site.ts`. The build writes the list into it instead,
 * and the published file (content-hashed) is the filled one. The same list is
 * checked against every page the generator renders: a page that shows an
 * install command must be on it, so the pages where `install-copy` can fire
 * and the pages a campaign is carried to are one set, not two.
 */

/** The one spot in `attribution.js` the build writes the list into. */
export const INSTALL_PAGES_PLACEHOLDER = '[] /* @install-pages */'

/** `attribution.js` with the install pages written in. Throws unless the
 *  placeholder is there exactly once: a script that silently kept an empty
 *  list would carry a campaign onto no install page at all. */
export function carryScript(source: string, installPages: readonly string[]): string {
  const at = source.indexOf(INSTALL_PAGES_PLACEHOLDER)
  if (at === -1 || source.indexOf(INSTALL_PAGES_PLACEHOLDER, at + 1) !== -1) {
    throw new Error(`attribution.js must contain \`${INSTALL_PAGES_PLACEHOLDER}\` exactly once; the build writes INSTALL_PAGES there`)
  }
  return source.slice(0, at) + JSON.stringify(installPages) + source.slice(at + INSTALL_PAGES_PLACEHOLDER.length)
}

/** What `codeBlock` in `render.ts` emits for an install or enroll command. */
const INSTALL_BLOCK = /<div class="code-block" data-code data-install>/

export interface RenderedPage {
  /** What an error names: the source file, or the URL of a generated page. */
  id: string
  url: string
  html: string
}

/** The build gate: every page that shows an install command is listed, and
 *  every listed page exists. One message per problem; empty when consistent. */
export function installPageErrors(pages: readonly RenderedPage[], installPages: readonly string[]): string[] {
  const errors: string[] = []
  const urls = new Set(pages.map((p) => p.url))
  for (const url of installPages) {
    if (!urls.has(url)) errors.push(`INSTALL_PAGES (ui/docs/site.ts) lists ${url}, which is not a page this site generates`)
  }
  for (const p of pages) {
    if (INSTALL_BLOCK.test(p.html) && !installPages.includes(p.url)) {
      errors.push(
        `${p.id} — shows an install command but ${p.url} is not in INSTALL_PAGES (ui/docs/site.ts); ` +
          `add it, or a campaign never reaches the page its install-copy goal is counted on`,
      )
    }
  }
  return errors
}
