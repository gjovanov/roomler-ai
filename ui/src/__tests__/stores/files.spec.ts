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

    // #3327 — the server resolves a file by id as a live one only, so a delete
    // of a file already deleted answers 404.
    it('a 404 is a file already deleted: it leaves the list and nothing is thrown', async () => {
      mockApi.delete.mockRejectedValueOnce(Object.assign(new Error('Resource not found'), { status: 404 }))

      const store = useFileStore()
      store.files = [makeFile({ id: 'f1' }), makeFile({ id: 'f2' })]

      await expect(store.deleteFile('t1', 'f1')).resolves.toBeUndefined()

      expect(store.files.map((f) => f.id)).toEqual(['f2'])
    })

    it('any other refusal is still an error, and the file stays', async () => {
      mockApi.delete.mockRejectedValueOnce(
        Object.assign(new Error('Only the uploader can delete this file'), { status: 403 }),
      )

      const store = useFileStore()
      store.files = [makeFile({ id: 'f1' })]

      await expect(store.deleteFile('t1', 'f1')).rejects.toThrow('Only the uploader')
      expect(store.files.map((f) => f.id)).toEqual(['f1'])
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

// ─── #4257: the org its list is of ───────────────────────────────────────
// The Files page is reused when its org changes in place (a link, back or
// forward). The store records which org its list is of: a fetch for another
// org drops the previous org's files at once, so none show while the new
// org's list loads or after that load failed, and an upload answered for an
// org the page has left stays out of the list. A room's files are the same
// org's list: moving between all files and a room's never drops it.

describe('useFileStore: the org its list is of (#4257)', () => {
  const ORG_A = 'a'.repeat(24)
  const ORG_B = 'b'.repeat(24)
  const aFile = makeFile({ id: 'fa', tenant_id: ORG_A, filename: 'org-a.txt' })
  const bFile = makeFile({ id: 'fb', tenant_id: ORG_B, filename: 'org-b.txt' })

  beforeEach(() => {
    setActivePinia(createPinia())
    vi.clearAllMocks()
    mockApi.get.mockReset()
    mockApi.upload.mockReset()
  })

  function envelope(items: unknown[]) {
    return { items, total: items.length, page: 1, per_page: 25, total_pages: 1 }
  }

  it("a fetch for org B drops org A's files at once, before it answers", async () => {
    const store = useFileStore()
    mockApi.get.mockResolvedValueOnce(envelope([aFile]))
    await store.fetchTenantFiles(ORG_A)
    expect(store.files.map((f) => f.id)).toEqual(['fa'])

    let answerB: (v: unknown) => void
    mockApi.get.mockReturnValueOnce(new Promise((r) => { answerB = r }))
    const fetching = store.fetchTenantFiles(ORG_B)
    expect(store.files).toEqual([])
    expect(store.total).toBe(0)
    answerB!(envelope([bFile]))
    await fetching
    expect(store.files.map((f) => f.id)).toEqual(['fb'])
  })

  it("a failed fetch for org B leaves none of org A's files", async () => {
    const store = useFileStore()
    mockApi.get.mockResolvedValueOnce(envelope([aFile]))
    await store.fetchTenantFiles(ORG_A)

    mockApi.get.mockRejectedValueOnce(new Error('unavailable'))
    await expect(store.fetchTenantFiles(ORG_B)).rejects.toThrow('unavailable')
    expect(store.files).toEqual([])
  })

  it("an upload for org A that answers after a fetch for org B stays out of org B's list, and still answers its file", async () => {
    const store = useFileStore()
    mockApi.get.mockResolvedValueOnce(envelope([aFile]))
    await store.fetchTenantFiles(ORG_A)
    let uploaded: (v: unknown) => void
    mockApi.upload.mockReturnValueOnce(new Promise((r) => { uploaded = r }))
    const uploading = store.uploadFile(ORG_A, 'room-a', new File(['x'], 'late.txt'))

    mockApi.get.mockResolvedValueOnce(envelope([bFile]))
    await store.fetchTenantFiles(ORG_B)
    const late = makeFile({ id: 'f-late', tenant_id: ORG_A, filename: 'late.txt' })
    uploaded!(late)
    await expect(uploading).resolves.toEqual(late)
    expect(store.files.map((f) => f.id)).toEqual(['fb'])
  })

  it("the same org's room files and uploads keep the list: nothing drops while a room's files load", async () => {
    const store = useFileStore()
    mockApi.get.mockResolvedValueOnce(envelope([aFile]))
    await store.fetchTenantFiles(ORG_A)

    let answerRoom: (v: unknown) => void
    mockApi.get.mockReturnValueOnce(new Promise((r) => { answerRoom = r }))
    const fetching = store.fetchFiles(ORG_A, 'room-a')
    expect(store.files.map((f) => f.id)).toEqual(['fa'])
    answerRoom!(envelope([makeFile({ id: 'fr', tenant_id: ORG_A, filename: 'room.txt' })]))
    await fetching
    expect(store.files.map((f) => f.id)).toEqual(['fr'])

    mockApi.upload.mockResolvedValueOnce(makeFile({ id: 'fu', tenant_id: ORG_A, filename: 'up.txt' }))
    await store.uploadFile(ORG_A, 'room-a', new File(['x'], 'up.txt'))
    expect(store.files.map((f) => f.id)).toEqual(['fr', 'fu'])
  })
})
