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
      consent_mode: 'prompt',
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

  it('triggerUpdate POSTs the per-agent update path and returns delivered', async () => {
    const store = useAgentStore()
    mockApi.post.mockResolvedValueOnce({ agent_id: 'a1', delivered: true })

    const delivered = await store.triggerUpdate(TENANT_ID, 'a1')

    expect(mockApi.post).toHaveBeenCalledWith(`/tenant/${TENANT_ID}/agent/a1/update`, {})
    expect(delivered).toBe(true)
  })

  it('triggerUpdateAll POSTs the bulk update path and returns counts', async () => {
    const store = useAgentStore()
    mockApi.post.mockResolvedValueOnce({ requested: 3, delivered: 2, results: [] })

    const res = await store.triggerUpdateAll(TENANT_ID)

    expect(mockApi.post).toHaveBeenCalledWith(`/tenant/${TENANT_ID}/agent/update`, {})
    expect(res.requested).toBe(3)
    expect(res.delivered).toBe(2)
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
      consent_mode: 'auto' as const,
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

  it('updateRoutes PUTs routes and patches local row', async () => {
    mockApi.put.mockResolvedValueOnce({ updated: true })
    const s = useAgentStore()
    s.agents = [mkAgent({ id: 'a1' })]
    const routes = ['10.66.24.0/24', '192.168.1.0/24']
    await s.updateRoutes(TENANT_ID, 'a1', routes)
    expect(mockApi.put).toHaveBeenCalledWith(`/tenant/${TENANT_ID}/agent/a1`, { routes })
    expect(s.agents[0]!.routes).toEqual(routes)
  })

  // ─── Task 9 Phase 3: crash-report fetch ────────────────────────

  it('fetchCrashes GETs the tenant-scoped per-agent endpoint and unwraps items', async () => {
    mockApi.get.mockResolvedValueOnce({
      items: [
        {
          id: 'cr1',
          reportedAt: '2026-05-17T12:00:00Z',
          crashedAtUnix: 1779192000,
          reason: 'panic',
          summary: 'index out of bounds',
          logTail: 'line 1\nline 2',
          agentVersion: '0.3.0-rc.35',
          os: 'windows',
          hostname: 'the field-test host',
          pid: 4567,
        },
      ],
    })
    const s = useAgentStore()
    const out = await s.fetchCrashes(TENANT_ID, 'a1')
    expect(mockApi.get).toHaveBeenCalledWith(`/tenant/${TENANT_ID}/agent/a1/crash`)
    expect(out).toHaveLength(1)
    expect(out[0]!.reason).toBe('panic')
    expect(out[0]!.summary).toBe('index out of bounds')
  })

  it('fetchCrashes propagates errors (callers handle locally, not in store)', async () => {
    mockApi.get.mockRejectedValueOnce(new Error('403 forbidden'))
    const s = useAgentStore()
    await expect(s.fetchCrashes(TENANT_ID, 'a1')).rejects.toThrow('403 forbidden')
    // Store-level state (agents.error/loading) is NOT mutated by
    // fetchCrashes; the modal holds its own local loading + error
    // state.
    expect(s.error).toBeNull()
    expect(s.loading).toBe(false)
  })

  // ── Fleet RPC ──────────────────────────────────────────────────────

  it('an agent with no exec_policy is treated as closed, not permissive', () => {
    // Every device that existed before the feature deserialises without the
    // field. Reading that as anything but "off" would retroactively open the
    // whole fleet the moment the org switch is flipped.
    const a = mkAgent()
    expect(a.exec_policy).toBeUndefined()
    expect(a.exec_policy?.mode ?? 'off').toBe('off')
  })

  it('execOnAgent posts the command and returns the result verbatim', async () => {
    mockApi.post.mockResolvedValueOnce({
      request_id: 'req1',
      agent_id: 'a1',
      agent_name: 'Laptop',
      exit_code: 0,
      stdout: 'uid=0(root)',
      stderr: '',
      truncated: false,
      duration_ms: 12,
      error: null,
    })
    const s = useAgentStore()
    const out = await s.execOnAgent(TENANT_ID, 'a1', { command: 'id', timeout_ms: 5000 })
    expect(mockApi.post).toHaveBeenCalledWith(`/tenant/${TENANT_ID}/agent/a1/exec`, {
      command: 'id',
      timeout_ms: 5000,
    })
    expect(out.exit_code).toBe(0)
    expect(out.stdout).toBe('uid=0(root)')
  })

  it('a policy refusal RESOLVES with an error, it does not reject', async () => {
    // The server answers 200 with `error` set for every gate refusal, so the
    // UI renders one shape and never has to guess whether a rejection was a
    // policy decision or a network failure.
    mockApi.post.mockResolvedValueOnce({
      request_id: 'req2',
      agent_id: 'a1',
      agent_name: 'Laptop',
      exit_code: null,
      stdout: '',
      stderr: '',
      truncated: false,
      duration_ms: 0,
      error: 'remote execution is not enabled on this device',
    })
    const s = useAgentStore()
    const out = await s.execOnAgent(TENANT_ID, 'a1', { command: 'id' })
    expect(out.error).toContain('not enabled on this device')
    // The distinction the whole result shape exists to preserve.
    expect(out.exit_code).toBeNull()
  })

  it('execOnFleet unwraps the results array', async () => {
    mockApi.post.mockResolvedValueOnce({
      results: [
        { request_id: 'r1', agent_id: 'a1', agent_name: 'A', exit_code: 0, stdout: 'x', stderr: '', truncated: false, duration_ms: 3, error: null },
        { request_id: 'r2', agent_id: 'a2', agent_name: 'B', exit_code: null, stdout: '', stderr: '', truncated: false, duration_ms: 0, error: 'device is offline' },
      ],
    })
    const s = useAgentStore()
    const out = await s.execOnFleet(TENANT_ID, ['a1', 'a2'], { command: 'id' })
    expect(mockApi.post).toHaveBeenCalledWith(`/tenant/${TENANT_ID}/agent/exec`, {
      agent_ids: ['a1', 'a2'],
      command: 'id',
    })
    expect(out).toHaveLength(2)
    expect(out[1]!.error).toBe('device is offline')
  })

  it('updateExecPolicy PUTs and patches the local agent', async () => {
    mockApi.put.mockResolvedValueOnce({})
    const s = useAgentStore()
    s.agents = [mkAgent({ id: 'a1' })]
    const policy = {
      mode: 'on' as const,
      can_originate: false,
      allowed_user_ids: [],
      allowed_role_ids: [],
      consent_mode: 'auto' as const,
      shells: [],
    }
    await s.updateExecPolicy(TENANT_ID, 'a1', policy)
    expect(mockApi.put).toHaveBeenCalledWith(`/tenant/${TENANT_ID}/agent/a1/exec-policy`, policy)
    expect(s.agents[0]!.exec_policy?.mode).toBe('on')
  })

  it('a 403 on the org switch leaves it UNKNOWN, not "off"', async () => {
    // "You are not an admin" and "the org has it switched off" are different
    // facts. Collapsing the former into the latter would have the console
    // tell a non-admin something false about their org.
    mockApi.get.mockRejectedValueOnce(new Error('403 forbidden'))
    const s = useAgentStore()
    await s.fetchOrgExecEnabled(TENANT_ID)
    expect(s.orgExecEnabled).toBeNull()
  })

  it('fetchOrgExecEnabled stores the flag', async () => {
    mockApi.get.mockResolvedValueOnce({ remote_exec_enabled: true })
    const s = useAgentStore()
    await s.fetchOrgExecEnabled(TENANT_ID)
    expect(s.orgExecEnabled).toBe(true)
  })

  it('fetchExecAudit passes the narrowing filters through', async () => {
    mockApi.get.mockResolvedValueOnce({ items: [], total: 0 })
    const s = useAgentStore()
    await s.fetchExecAudit(TENANT_ID, { agentId: 'a1', perPage: 25 })
    const url = mockApi.get.mock.calls.at(-1)![0] as string
    expect(url).toContain(`/tenant/${TENANT_ID}/exec-audit?`)
    expect(url).toContain('agent_id=a1')
    expect(url).toContain('per_page=25')
  })
})
