// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
import { describe, it, expect, vi, beforeEach } from 'vitest'
import { setActivePinia, createPinia } from 'pinia'

vi.mock('@/api/client', () => ({
  api: {
    get: vi.fn(),
    post: vi.fn(),
    put: vi.fn(),
    delete: vi.fn(),
    upload: vi.fn(),
  },
}))

import { useFileStore } from '@/stores/files'
import { api } from '@/api/client'

const mockApi = vi.mocked(api)

function makeFile(overrides: Record<string, unknown> = {}) {
  return {
    id: 'f1',
    tenant_id: 't1',
    filename: 'test.txt',
    content_type: 'text/plain',
    size: 1024,
    uploaded_by: 'u1',
    created_at: '2026-03-20T00:00:00Z',
    ...overrides,
  }
}

describe('useFileStore', () => {
  beforeEach(() => {
    setActivePinia(createPinia())
    vi.clearAllMocks()
  })

  describe('initial state', () => {
    it('should start with empty files and not loading', () => {
      const store = useFileStore()
      expect(store.files).toEqual([])
      expect(store.loading).toBe(false)
    })
  })

  describe('fetchFiles', () => {
    it('should fetch files for a tenant and room', async () => {
      const items = [makeFile({ id: 'f1' }), makeFile({ id: 'f2', filename: 'image.png' })]
      mockApi.get.mockResolvedValueOnce({ items, total: items.length, page: 1, per_page: 25, total_pages: 1 })

      const store = useFileStore()
      await store.fetchFiles('t1', 'r1')

      // FR-11: the list is a server grid now — pagination params always ride along.
      expect(mockApi.get).toHaveBeenCalledWith('/tenant/t1/room/r1/file?page=1&per_page=25')
      expect(store.files).toEqual(items)
    })

    it('should set loading while fetching', async () => {
      let resolvePromise: (v: unknown) => void
      mockApi.get.mockReturnValueOnce(new Promise((r) => { resolvePromise = r }))

      const store = useFileStore()
      const promise = store.fetchFiles('t1', 'r1')
      expect(store.loading).toBe(true)

      resolvePromise!({ items: [], total: 0, page: 1, per_page: 25, total_pages: 1 })
      await promise
      expect(store.loading).toBe(false)
    })

    it('should reset loading on error', async () => {
      mockApi.get.mockRejectedValueOnce(new Error('fail'))

      const store = useFileStore()
      await expect(store.fetchFiles('t1', 'r1')).rejects.toThrow()
      expect(store.loading).toBe(false)
    })

    it('should replace existing files on re-fetch', async () => {
      const store = useFileStore()
      store.files = [makeFile({ id: 'old' })]

      mockApi.get.mockResolvedValueOnce({ items: [makeFile({ id: 'new' })], total: 1, page: 1, per_page: 25, total_pages: 1 })
      await store.fetchFiles('t1', 'r1')

      expect(store.files).toHaveLength(1)
      expect(store.files[0].id).toBe('new')
    })
  })

  describe('uploadFile', () => {
    it('should upload a file and add it to the list', async () => {
      const entry = makeFile({ id: 'f-new', filename: 'upload.pdf' })
      mockApi.upload.mockResolvedValueOnce(entry)

      const store = useFileStore()
      const file = new File(['content'], 'upload.pdf', { type: 'application/pdf' })

      const result = await store.uploadFile('t1', 'r1', file)

      expect(mockApi.upload).toHaveBeenCalledWith(
        '/tenant/t1/file/upload',
        expect.any(FormData),
      )
      expect(result).toEqual(entry)
      expect(store.files).toContainEqual(entry)
    })

    it('should append to existing files', async () => {
      const existing = makeFile({ id: 'f1' })
      const newEntry = makeFile({ id: 'f2', filename: 'new.txt' })
      mockApi.upload.mockResolvedValueOnce(newEntry)

      const store = useFileStore()
      store.files = [existing]

      await store.uploadFile('t1', 'r1', new File([''], 'new.txt'))

      expect(store.files).toHaveLength(2)
    })
  })

  describe('deleteFile', () => {
    it('should delete a file and remove from list', async () => {
      mockApi.delete.mockResolvedValueOnce({})

      const store = useFileStore()
      store.files = [makeFile({ id: 'f1' }), makeFile({ id: 'f2' })]

      await store.deleteFile('t1', 'f1')

      expect(mockApi.delete).toHaveBeenCalledWith('/tenant/t1/file/f1')
      expect(store.files).toHaveLength(1)
      expect(store.files[0].id).toBe('f2')
    })

    it('should handle deleting non-existent file gracefully', async () => {
      mockApi.delete.mockResolvedValueOnce({})

      const store = useFileStore()
      store.files = [makeFile({ id: 'f1' })]

      await store.deleteFile('t1', 'nonexistent')

      expect(store.files).toHaveLength(1)
    })
  })

  describe('downloadUrl', () => {
    it('should return correct download URL', () => {
      const store = useFileStore()
      const url = store.downloadUrl('t1', 'f1')
      expect(url).toBe('/api/tenant/t1/file/f1/download')
    })
  })
})

// ─── FR-11 (#784): server-grid params ────────────────────────────────────

describe('useFileStore FR-11 grid params', () => {
  beforeEach(() => {
    setActivePinia(createPinia())
    vi.clearAllMocks()
  })

  function envelope(items: unknown[] = []) {
    return { items, total: items.length, page: 1, per_page: 25, total_pages: 1 }
  }

  it('fetchTenantFiles builds q/sort/dir params (server whitelist keys)', async () => {
    mockApi.get.mockResolvedValueOnce(envelope())
    const store = useFileStore()
    await store.fetchTenantFiles('t1', { page: 2, perPage: 50, q: 'report', sort: 'size', dir: 'desc' })
    expect(mockApi.get).toHaveBeenCalledWith(
      '/tenant/t1/file?page=2&per_page=50&q=report&sort=size&dir=desc',
    )
  })

  it('omits q and sort when unset (byte-compatible default request)', async () => {
    mockApi.get.mockResolvedValueOnce(envelope())
    const store = useFileStore()
    await store.fetchTenantFiles('t1')
    expect(mockApi.get).toHaveBeenCalledWith('/tenant/t1/file?page=1&per_page=25')
  })

  it('a stale response never clobbers a newer one (seq guard)', async () => {
    const store = useFileStore()
    let resolveSlow: (v: unknown) => void
    mockApi.get.mockReturnValueOnce(new Promise((r) => { resolveSlow = r }))
    const slow = store.fetchTenantFiles('t1', { q: 'old' })

    mockApi.get.mockResolvedValueOnce(envelope([makeFile({ id: 'fresh' })]))
    await store.fetchTenantFiles('t1', { q: 'new' })
    expect(store.files.map((f) => f.id)).toEqual(['fresh'])

    resolveSlow!(envelope([makeFile({ id: 'stale' })]))
    await slow
    expect(store.files.map((f) => f.id)).toEqual(['fresh'])
  })
})
