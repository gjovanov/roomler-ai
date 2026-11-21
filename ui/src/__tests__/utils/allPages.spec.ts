// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// #4188 — a list read page by page, as the server's list routes answer it.
import { beforeEach, describe, expect, it, vi } from 'vitest'

vi.mock('@/api/client', () => ({
  api: { get: vi.fn(), post: vi.fn(), put: vi.fn(), delete: vi.fn() },
}))

import { fetchAllPages, type ListPage } from '@/utils/allPages'
import { api } from '@/api/client'
import { pagedRoute, rowId } from '../stores/pagedRoute'

const mockApi = vi.mocked(api)

interface Row {
  id: string
  n: number
}
const row = (i: number): Row => ({ id: rowId(i), n: i })
const upTo = (n: number) => Array.from({ length: n }, (_, i) => i)
const walk = () => fetchAllPages<Row, ListPage<Row>>('/tenant/t/list')
const asked = () => mockApi.get.mock.calls.map(([path]) => path)

beforeEach(() => {
  vi.clearAllMocks()
})

describe('fetchAllPages', () => {
  it('asks page after page at the server page size until the total is in', async () => {
    mockApi.get.mockImplementation(pagedRoute({ total: 250, row }))
    const all = await walk()

    expect(asked()).toEqual([
      '/tenant/t/list?page=1&per_page=100',
      '/tenant/t/list?page=2&per_page=100',
      '/tenant/t/list?page=3&per_page=100',
    ])
    expect(all.items.map((r) => r.n)).toEqual(upTo(250))
    expect(all.last.total).toBe(250)
    expect(all.truncated).toBe(false)
  })

  it('counts the pages by the page size the server answers, not the one it asked for', async () => {
    mockApi.get.mockImplementation(pagedRoute({ total: 70, row, perPageCap: 30 }))
    const all = await walk()

    expect(mockApi.get).toHaveBeenCalledTimes(3)
    expect(all.items.map((r) => r.n)).toEqual(upTo(70))
  })

  it('asks once for an empty list', async () => {
    mockApi.get.mockImplementation(pagedRoute({ total: 0, row }))
    const all = await walk()

    expect(mockApi.get).toHaveBeenCalledTimes(1)
    expect(all.items).toEqual([])
    expect(all.truncated).toBe(false)
  })

  it('keeps a row once when a create between two requests pushes it onto the next page', async () => {
    // Newest first: the row created before page 2 is asked moves every row
    // down one, so the last row of page 1 is also the first of page 2.
    mockApi.get
      .mockResolvedValueOnce({ items: upTo(100).map(row), total: 101, page: 1, per_page: 100 } as never)
      .mockResolvedValueOnce({ items: [row(99), row(100)], total: 102, page: 2, per_page: 100 } as never)
    const all = await walk()

    expect(all.items.map((r) => r.n)).toEqual(upTo(101))
    // The last answer wins: the new row shows at the next fetch.
    expect(all.last.total).toBe(102)
  })

  it('stops at an empty page, whatever the total says', async () => {
    mockApi.get
      .mockResolvedValueOnce({ items: [row(0)], total: 1000, page: 1, per_page: 1 } as never)
      .mockResolvedValueOnce({ items: [], total: 1000, page: 2, per_page: 1 } as never)
    const all = await walk()

    expect(mockApi.get).toHaveBeenCalledTimes(2)
    expect(all.items.map((r) => r.n)).toEqual([0])
    expect(all.truncated).toBe(false)
  })

  it('reads at most 50 pages: 5 000 rows whole, and one row more is cut and said', async () => {
    mockApi.get.mockImplementation(pagedRoute({ total: 5000, row }))
    const whole = await walk()
    expect(mockApi.get).toHaveBeenCalledTimes(50)
    expect(whole.items).toHaveLength(5000)
    expect(whole.truncated).toBe(false)

    vi.clearAllMocks()
    mockApi.get.mockImplementation(pagedRoute({ total: 5001, row }))
    const cut = await walk()
    expect(mockApi.get).toHaveBeenCalledTimes(50)
    expect(cut.items).toHaveLength(5000)
    expect(cut.last.total).toBe(5001)
    expect(cut.truncated).toBe(true)
  })

  it('throws the first failed page, and asks for no page after it', async () => {
    const failure = Object.assign(new Error('API error 502'), { status: 502 })
    mockApi.get.mockImplementation(pagedRoute({ total: 250, row, fail: { page: 2, error: failure } }))

    await expect(walk()).rejects.toBe(failure)
    expect(asked()).toEqual([
      '/tenant/t/list?page=1&per_page=100',
      '/tenant/t/list?page=2&per_page=100',
    ])
  })
})
