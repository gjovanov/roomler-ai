import { describe, it, expect, vi, beforeEach } from 'vitest'
import { setActivePinia, createPinia } from 'pinia'

vi.mock('@/api/client', () => ({
  api: {
    get: vi.fn(),
    post: vi.fn(),
    put: vi.fn(),
    delete: vi.fn(),
  },
}))

import { useAgentStore, type Agent } from '@/stores/agents'
import { api } from '@/api/client'

const mockApi = vi.mocked(api)

const TENANT_ID = 'ten_1'

function mkAgent(over: Partial<Agent> = {}): Agent {
  return {
    id: 'a1',
    tenant_id: TENANT_ID,
    owner_user_id: 'u1',
    name: 'Laptop',
    machine_id: 'mach-1',
    os: 'linux',
    agent_version: '0.1.0',
    status: 'offline',
    is_online: false,
    last_seen_at: '2026-04-17T09:00:00Z',
    access_policy: {
      require_consent: true,
      allowed_role_ids: [],
      allowed_user_ids: [],
      auto_terminate_idle_minutes: null,
    },
    ...over,
  }
}

describe('useAgentStore', () => {
  beforeEach(() => {
    setActivePinia(createPinia())
    vi.clearAllMocks()
  })

  it('starts empty', () => {
    const s = useAgentStore()
    expect(s.agents).toEqual([])
    expect(s.total).toBe(0)
    expect(s.loading).toBe(false)
    expect(s.error).toBeNull()
  })

  it('fetchAgents populates list', async () => {
    mockApi.get.mockResolvedValueOnce({
      items: [mkAgent({ id: 'a1' }), mkAgent({ id: 'a2', name: 'Desktop' })],
      total: 2,
      page: 1,
      per_page: 25,
      total_pages: 1,
    })
    const s = useAgentStore()
    await s.fetchAgents(TENANT_ID)
    expect(mockApi.get).toHaveBeenCalledWith(`/tenant/${TENANT_ID}/agent`)
    expect(s.agents).toHaveLength(2)
    expect(s.total).toBe(2)
  })

  it('fetchAgents stores error and clears list on failure', async () => {
    mockApi.get.mockRejectedValueOnce(new Error('network'))
    const s = useAgentStore()
    await s.fetchAgents(TENANT_ID)
    expect(s.error).toBe('network')
    expect(s.agents).toEqual([])
    expect(s.total).toBe(0)
  })

  it('issueEnrollmentToken POSTs to correct path', async () => {
    const tok = {
      enrollment_token: 'jwt.here',
      expires_in: 600,
      jti: 'abc',
    }
    mockApi.post.mockResolvedValueOnce(tok)
    const s = useAgentStore()
    const out = await s.issueEnrollmentToken(TENANT_ID)
    expect(mockApi.post).toHaveBeenCalledWith(`/tenant/${TENANT_ID}/agent/enroll-token`)
    expect(out).toEqual(tok)
  })

  it('rename updates local row on success', async () => {
    mockApi.put.mockResolvedValueOnce({ updated: true })
    const s = useAgentStore()
    s.agents = [mkAgent({ id: 'a1', name: 'Old' })]
    await s.rename(TENANT_ID, 'a1', 'New')
    expect(mockApi.put).toHaveBeenCalledWith(`/tenant/${TENANT_ID}/agent/a1`, { name: 'New' })
    expect(s.agents[0]!.name).toBe('New')
  })

  it('deleteAgent removes from list and decrements total', async () => {
    mockApi.delete.mockResolvedValueOnce({ deleted: true })
    const s = useAgentStore()
    s.agents = [mkAgent({ id: 'a1' }), mkAgent({ id: 'a2' })]
    s.total = 2
    await s.deleteAgent(TENANT_ID, 'a1')
    expect(mockApi.delete).toHaveBeenCalledWith(`/tenant/${TENANT_ID}/agent/a1`)
    expect(s.agents.map((a) => a.id)).toEqual(['a2'])
    expect(s.total).toBe(1)
  })

  it('updateAccessPolicy merges into local row', async () => {
    mockApi.put.mockResolvedValueOnce({ updated: true })
    const s = useAgentStore()
    s.agents = [mkAgent({ id: 'a1' })]
    const policy = {
      require_consent: false,
      allowed_role_ids: ['r1'],
      allowed_user_ids: [],
      auto_terminate_idle_minutes: 30,
    }
    await s.updateAccessPolicy(TENANT_ID, 'a1', policy)
    expect(mockApi.put).toHaveBeenCalledWith(`/tenant/${TENANT_ID}/agent/a1`, {
      access_policy: policy,
    })
    expect(s.agents[0]!.access_policy).toEqual(policy)
  })
})
