// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// #4188 — a list route that pages the way the server's list routes do, for the
// specs of the stores that read one.

export interface PagedRouteOptions<T> {
  /** How many rows the route holds. */
  total: number
  /** The i-th row (from 0), in the route's own order. */
  row: (i: number) => T
  /** The tunnel-policy route answers `total_pages`; the overlay-acl one does not. */
  totalPages?: boolean
  /** Fields the route answers beside the page, such as overlay-acl's `mode`. */
  extra?: Record<string, unknown>
  /** The largest page the route serves. The server's is 100; a lower one stands
   *  in for a server that cuts the page size harder. */
  perPageCap?: number
  /** A page whose request fails, and what it throws. */
  fail?: { page: number; error: unknown }
}

/**
 * Answers a list request as `BaseDao::find_paginated` does
 * (`crates/services/src/dao/base.rs`): `page` defaults to 1 and `per_page` to
 * 25, `per_page` is cut to the cap, and the answer carries the `per_page` it
 * used. For `mockApi.get.mockImplementation`.
 */
export function pagedRoute<T>(opts: PagedRouteOptions<T>) {
  const cap = opts.perPageCap ?? 100
  return async (path: string): Promise<never> => {
    const q = new URL(path, 'http://route.test').searchParams
    const page = Number(q.get('page') ?? '1')
    const perPage = Math.min(Number(q.get('per_page') ?? '25'), cap)
    if (opts.fail && opts.fail.page === page) throw opts.fail.error
    const start = (page - 1) * perPage
    const items: T[] = []
    for (let i = start; i < Math.min(start + perPage, opts.total); i++) items.push(opts.row(i))
    const answer: Record<string, unknown> = {
      items,
      total: opts.total,
      page,
      per_page: perPage,
      ...opts.extra,
    }
    if (opts.totalPages) answer.total_pages = perPage > 0 ? Math.ceil(opts.total / perPage) : 0
    return answer as never
  }
}

/** A distinct 24-hex id for the i-th row. */
export const rowId = (i: number): string => i.toString(16).padStart(24, '0')
