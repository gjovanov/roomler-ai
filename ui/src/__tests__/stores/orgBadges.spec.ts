import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest'
import { setActivePinia, createPinia } from 'pinia'

vi.mock('@/api/client', () => ({
  api: {
    get: vi.fn(),
    post: vi.fn(),
    put: vi.fn(),
    delete: vi.fn(),
  },
}))

import { api } from '@/api/client'
import { useOrgBadgesStore } from '@/stores/orgBadges'

describe('useOrgBadgesStore (P4 cross-org badges)', () => {
  beforeEach(() => {
    setActivePinia(createPinia())
    vi.clearAllMocks()
    vi.useFakeTimers()
  })

  afterEach(() => {
    vi.useRealTimers()
  })

  it('accumulates live deltas per org and sums them into badgeCount', () => {
    const s = useOrgBadgesStore()
    s.noteForeignMessage('org-b')
    s.noteForeignMessage('org-b')
    s.noteForeignNotification('org-b', 'mention')
    s.noteForeignNotification('org-c', 'consent_request')

    expect(s.badgeCount('org-b')).toBe(3) // 2 messages + 1 notification
    expect(s.summaries['org-b']?.mentions).toBe(1)
    expect(s.badgeCount('org-c')).toBe(1)
    expect(s.summaries['org-c']?.consents).toBe(1)
    expect(s.badgeCount('org-unknown')).toBe(0)
  })

  it('counts only offline/stale device transitions as attention events', () => {
    const s = useOrgBadgesStore()
    s.noteDevicePresence('org-b', [
      { presence: 'online' },
      { presence: 'offline' },
      { presence: 'stale' },
    ])
    expect(s.hasDeviceEvents('org-b')).toBe(true)
    expect(s.deviceEvents['org-b']).toBe(2)

    s.noteDevicePresence('org-b', [{ presence: 'online' }])
    expect(s.deviceEvents['org-b']).toBe(2) // online alone never bumps
  })

  it('clearForTenant acknowledges device events and schedules a summary re-sync', async () => {
    const s = useOrgBadgesStore()
    vi.mocked(api.get).mockResolvedValue({ tenants: [] })
    s.noteDevicePresence('org-b', [{ presence: 'offline' }])

    s.clearForTenant('org-b')
    expect(s.hasDeviceEvents('org-b')).toBe(false)

    expect(api.get).not.toHaveBeenCalled() // debounced, not immediate
    await vi.advanceTimersByTimeAsync(2100)
    expect(api.get).toHaveBeenCalledWith('/user/unread-summary')
  })

  it('fetchSummaryNow replaces summaries with server truth', async () => {
    const s = useOrgBadgesStore()
    s.noteForeignMessage('org-stale')
    vi.mocked(api.get).mockResolvedValue({
      tenants: [
        {
          tenant_id: 'org-b',
          name: 'B',
          unread_messages: 4,
          unread_rooms: 2,
          notifications: 1,
          mentions: 1,
          consents: 0,
        },
      ],
    })

    await s.fetchSummaryNow()

    expect(s.badgeCount('org-b')).toBe(5)
    expect(s.badgeCount('org-stale')).toBe(0) // live delta superseded by fetch
  })

  it('anyForeignActivity ignores the active org', () => {
    const s = useOrgBadgesStore()
    s.noteForeignMessage('org-a')
    expect(s.anyForeignActivity('org-a')).toBe(false)
    expect(s.anyForeignActivity('org-z')).toBe(true)

    const s2 = useOrgBadgesStore()
    s2.deviceEvents['org-d'] = 1
    expect(s2.anyForeignActivity('org-a')).toBe(true)
  })
})
