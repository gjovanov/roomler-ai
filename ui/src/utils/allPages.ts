// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
import { api } from '@/api/client'

/**
 * #4188 — read every page of a paginated list route.
 *
 * A list route answers one page at a time (`BaseDao::find_paginated` in
 * `crates/services/src/dao/base.rs`): 25 rows unless the request asks for
 * more, and never more than its `MAX_PER_PAGE` of 100, whatever it asks. A
 * store that asked once, with no paging parameters, kept the first 25 rows of
 * a longer list, and nothing said that more existed. That was the Tunnel ACL
 * and the Overlay ACL tabs.
 */

/** One page as a list route answers it. The tunnel-policy route also answers
 *  `total_pages`; the overlay-acl route does not. So the walk counts the pages
 *  itself, from `total` and `per_page`, the way the server counts them. */
export interface ListPage<T> {
  items: T[]
  total: number
  page: number
  per_page: number
}

/** The rows a walk asks for in each request: the server's `MAX_PER_PAGE`. The
 *  server may serve fewer, so the walk counts pages by the `per_page` each
 *  answer carries, never by this. */
export const PAGE_SIZE = 100

/** The most pages one walk asks for: 5 000 rows at the server's page size. */
export const MAX_PAGES = 50

export interface AllPages<T, P extends ListPage<T>> {
  /** Every row the walk read, in the route's order, each id once. */
  items: T[]
  /** The last answer. Its `total`, and any field beside the page (overlay-acl's
   *  `mode`), are the newest the server gave. */
  last: P
  /** The walk stopped at `MAX_PAGES` while the server counted more pages. */
  truncated: boolean
}

/**
 * Asks the route at `path` (no query of its own) for page 1, 2, … at
 * `PAGE_SIZE` rows until the last page is in, or until `MAX_PAGES` pages are.
 * The first failed request throws to the caller, which handles it as it
 * handled its one request before, so no partial list is ever kept.
 */
export async function fetchAllPages<T extends { id: string }, P extends ListPage<T>>(
  path: string,
): Promise<AllPages<T, P>> {
  const seen = new Set<string>()
  const items: T[] = []
  for (let page = 1; ; page += 1) {
    const last = await api.get<P>(`${path}?page=${page}&per_page=${PAGE_SIZE}`)
    // The pages are offsets into a list that can change during the walk. A row
    // created between two requests pushes a row already read onto the next
    // page, so each id is kept once. A row deleted between them pulls the next
    // one back onto a page already read, and that one shows at the next fetch.
    // There is no retry: the last answer wins, for the total and for every
    // field beside the page.
    for (const row of last.items) {
      if (seen.has(row.id)) continue
      seen.add(row.id)
      items.push(row)
    }
    const pages = last.per_page > 0 ? Math.ceil(last.total / last.per_page) : 0
    // Written as "more", so a count that is not a number ends the walk. An
    // empty page ends it too, whatever the total says: a total the rows never
    // reach cannot keep it asking.
    const more = page < pages && last.items.length > 0
    if (!more) return { items, last, truncated: false }
    if (page >= MAX_PAGES) return { items, last, truncated: true }
  }
}
