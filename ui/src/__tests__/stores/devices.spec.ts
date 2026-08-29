// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
import { beforeEach, describe, expect, it, vi } from 'vitest'
import { createPinia, setActivePinia } from 'pinia'
import { useDeviceStore, type DeviceRow } from '@/stores/devices'

vi.mock('@/api/client', () => ({
  api: {
    get: vi.fn(),
    post: vi.fn(),
    put: vi.fn(),
    delete: vi.fn(),
    upload: vi.fn(),
  },
}))

import { api } from '@/api/client'
const mockApi = api as unknown as { get: ReturnType<typeof vi.fn> }

const TENANT = 'tid1'

function makeRow(overrides: Partial<DeviceRow> = {}): DeviceRow {
  return {
    kind: 'agent',
    id: 'a1',
    owner_user_id: 'u1',
    name: 'Box',
    machine_id: 'm1',
    os: 'linux',
    version: '0.3.0',
    status: 'online',
    presence: 'online',
    is_online: true,
    last_seen_at: '2026-08-26T00:00:00Z',
    created_at: '2026-08-01T00:00:00Z',
    ...overrides,
  }
}

function envelope(items: DeviceRow[], overrides: Record<string, unknown> = {}) {
  return {
    items,
    total: items.length,
    page: 1,
    per_page: 25,
    total_pages: 1,
    ...overrides,
  }
}

describe('devices store', () => {
  beforeEach(() => {
    setActivePinia(createPinia())
    vi.clearAllMocks()
  })

  it('builds the query string from opts (q trimmed, empties dropped)', async () => {
    mockApi.get.mockResolvedValue(envelope([makeRow()]))
    const s = useDeviceStore()
    await s.fetchDevices(TENANT, {
      page: 2,
      perPage: 50,
      q: '  vienna ',
      sort: 'name',
      dir: 'desc',
      kind: 'agent',
    })
    expect(mockApi.get).toHaveBeenCalledWith(
      `/tenant/${TENANT}/device?page=2&per_page=50&q=vienna&sort=name&dir=desc&kind=agent`,
    )
    expect(s.items).toHaveLength(1)
  })

  it('omits q entirely when blank', async () => {
    mockApi.get.mockResolvedValue(envelope([]))
    const s = useDeviceStore()
    await s.fetchDevices(TENANT, { q: '   ' })
    const url = mockApi.get.mock.calls[0]![0] as string
    expect(url).not.toContain('q=')
  })

  it('adopts the server envelope (total/page/per_page/total_pages)', async () => {
    mockApi.get.mockResolvedValue(
      envelope([makeRow()], { total: 42, page: 3, per_page: 10, total_pages: 5 }),
    )
    const s = useDeviceStore()
    await s.fetchDevices(TENANT)
    expect(s.total).toBe(42)
    expect(s.page).toBe(3)
    expect(s.perPage).toBe(10)
    expect(s.totalPages).toBe(5)
  })

  it('a stale response never clobbers a newer one', async () => {
    let resolveSlow!: (v: unknown) => void
    const slow = new Promise((r) => (resolveSlow = r))
    mockApi.get.mockReturnValueOnce(slow) // fetch #1 hangs
    mockApi.get.mockResolvedValueOnce(envelope([makeRow({ id: 'new' })])) // fetch #2 wins
    const s = useDeviceStore()
    const p1 = s.fetchDevices(TENANT, { page: 1 })
    await s.fetchDevices(TENANT, { page: 2 })
    expect(s.items[0]!.id).toBe('new')
    resolveSlow(envelope([makeRow({ id: 'stale' })]))
    await p1
    expect(s.items[0]!.id).toBe('new')
  })

  it('applyPresence patches agent rows in place and ignores unknown ids', () => {
    const s = useDeviceStore()
    s.items = [
      makeRow({ id: 'a1', presence: 'online', is_online: true }),
      makeRow({ id: 't1', kind: 'tunnel_client' }),
    ]
    s.applyPresence([
      { agent_id: 'a1', presence: 'offline' },
      { agent_id: 'ghost', presence: 'online' },
      // A tunnel client id must never match (kind-guarded).
      { agent_id: 't1', presence: 'offline' },
    ])
    expect(s.items[0]!.presence).toBe('offline')
    expect(s.items[0]!.is_online).toBe(false)
    expect(s.items[1]!.presence).toBe('online')
  })

  it('patchRow merges fields into the matching row', () => {
    const s = useDeviceStore()
    s.items = [makeRow({ id: 'a1', name: 'Old' })]
    s.patchRow({ id: 'a1', name: 'New', tags: ['x'] })
    expect(s.items[0]!.name).toBe('New')
    expect(s.items[0]!.tags).toEqual(['x'])
    s.patchRow({ id: 'nope', name: 'Ignored' })
    expect(s.items).toHaveLength(1)
  })
})
