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
    // per_page=100 (the server cap): without it the server defaulted to 25
    // and every consumer saw a silently truncated fleet.
    expect(mockApi.get).toHaveBeenCalledWith(`/tenant/${TENANT_ID}/agent?per_page=100`)
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

  it('rotateOverlayKey POSTs the overlay-key rotate path and returns the dispatch', async () => {
    const store = useAgentStore()
    mockApi.post.mockResolvedValueOnce({
      agent_id: 'a1',
      request_id: 'r1',
      dispatch: 'queued',
      delivered: false,
    })

    const res = await store.rotateOverlayKey(TENANT_ID, 'a1')

    expect(mockApi.post).toHaveBeenCalledWith(
      `/tenant/${TENANT_ID}/agent/a1/overlay-key/rotate`,
      {},
    )
    expect(res.request_id).toBe('r1')
    expect(res.dispatch).toBe('queued')
    expect(res.delivered).toBe(false)
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

  // ── Roomler SSH ────────────────────────────────────────────────────

  it('an agent with no ssh_policy is treated as closed, not permissive', () => {
    // Same rule as exec: a device that predates the feature, or whose API
    // body omits the field, must read as OFF. Defaulting the other way would
    // silently open every legacy device the moment the UI shipped.
    const a = mkAgent()
    expect(a.ssh_policy).toBeUndefined()
    expect(a.ssh_policy?.mode ?? 'off').toBe('off')
  })

  it('updateSshPolicy hits the ssh route and updates the row in place', async () => {
    mockApi.put.mockResolvedValueOnce({})
    const s = useAgentStore()
    s.agents = [mkAgent({ id: 'a1' })]
    const policy = {
      mode: 'on' as const,
      can_originate: false,
      allowed_user_ids: [],
      allowed_role_ids: [],
      account_mode: 'console_user' as const,
      account: null,
      consent_mode: null,
    }
    await s.updateSshPolicy(TENANT_ID, 'a1', policy)
    expect(mockApi.put).toHaveBeenCalledWith(`/tenant/${TENANT_ID}/agent/a1/ssh-policy`, policy)
    expect(s.agents[0]!.ssh_policy?.mode).toBe('on')
    // The exec policy is a different gate and must not be touched by this.
    expect(s.agents[0]!.exec_policy).toBeUndefined()
  })

  it('a rejected ssh-policy save PROPAGATES instead of reporting success', async () => {
    // The server refuses a prompt policy for an agent that would ignore it
    // (pre-P5d). Swallowing that would leave an admin believing a prompt is
    // enforced when nothing would ever ask — the exact lie P5d closed.
    mockApi.put.mockRejectedValueOnce(new Error('agent would ignore consent_mode'))
    const s = useAgentStore()
    s.agents = [mkAgent({ id: 'a1' })]
    await expect(
      s.updateSshPolicy(TENANT_ID, 'a1', {
        mode: 'on',
        can_originate: false,
        allowed_user_ids: [],
        allowed_role_ids: [],
        account_mode: 'console_user',
        account: null,
        consent_mode: 'prompt',
      }),
    ).rejects.toThrow()
    expect(s.agents[0]!.ssh_policy).toBeUndefined()
  })

  it('the ssh org switch is separate from the exec one', async () => {
    // Two independent decisions server-side; the store must not let one
    // fetch populate the other's flag.
    mockApi.get.mockResolvedValueOnce({ remote_ssh_enabled: true })
    const s = useAgentStore()
    await s.fetchOrgSshEnabled(TENANT_ID)
    expect(s.orgSshEnabled).toBe(true)
    expect(s.orgExecEnabled).toBeNull()
    expect(mockApi.get.mock.calls.at(-1)![0]).toBe(`/tenant/${TENANT_ID}/ssh-settings`)
  })

  it('a 403 on the ssh org switch leaves it UNKNOWN, not "off"', async () => {
    mockApi.get.mockRejectedValueOnce(new Error('403 forbidden'))
    const s = useAgentStore()
    await s.fetchOrgSshEnabled(TENANT_ID)
    expect(s.orgSshEnabled).toBeNull()
  })

  it('fetchSshAudit passes the narrowing filters through', async () => {
    mockApi.get.mockResolvedValueOnce({ items: [], total: 0 })
    const s = useAgentStore()
    await s.fetchSshAudit(TENANT_ID, { userId: 'u9', perPage: 25 })
    const url = mockApi.get.mock.calls.at(-1)![0] as string
    expect(url).toContain(`/tenant/${TENANT_ID}/ssh-audit?`)
    expect(url).toContain('user_id=u9')
    expect(url).toContain('per_page=25')
  })

  it('fetchSshActivity hits its OWN route, not the audit one', async () => {
    // The two logs mean different things — the server's decision vs the
    // device's claim about itself — and are deliberately separate
    // collections. Pointing this at `ssh-audit` would silently present
    // unverified device reports as authoritative.
    mockApi.get.mockResolvedValueOnce({ items: [], total: 0 })
    const s = useAgentStore()
    await s.fetchSshActivity(TENANT_ID)
    const url = mockApi.get.mock.calls.at(-1)![0] as string
    expect(url).toContain(`/tenant/${TENANT_ID}/ssh-activity?`)
    expect(url).not.toContain('ssh-audit')
  })

  it('fetchSshActivity can narrow to one session via grant_id', async () => {
    // The join from an audit decision row to what followed it.
    mockApi.get.mockResolvedValueOnce({ items: [], total: 0 })
    const s = useAgentStore()
    await s.fetchSshActivity(TENANT_ID, { grantId: 'g-7', agentId: 'a1' })
    const url = mockApi.get.mock.calls.at(-1)![0] as string
    expect(url).toContain('grant_id=g-7')
    expect(url).toContain('agent_id=a1')
  })

  it('an activity row with allowed:false survives the round trip', async () => {
    // A refused forward is the row an operator most wants to find. If
    // `allowed` were dropped or defaulted, a denial would render as an
    // ordinary action.
    mockApi.get.mockResolvedValueOnce({
      items: [
        {
          id: 'r1',
          agent_id: 'a1',
          caller: 'ssh:Someone@100.65.4.2:1',
          kind: 'forward',
          detail: '10.0.0.5:5432',
          allowed: false,
          at: '2026-08-23T00:00:00Z',
        },
      ],
      total: 1,
    })
    const s = useAgentStore()
    const res = await s.fetchSshActivity(TENANT_ID)
    expect(res.items[0]!.allowed).toBe(false)
    expect(res.items[0]!.kind).toBe('forward')
    expect(res.items[0]!.detail).toBe('10.0.0.5:5432')
  })

  // ── Remote config (docs/remote-config.md) ──────────────────────────

  it('updateDesiredConfig sends only the keys under management', async () => {
    // `undefined` means "leave the device alone" and must NOT be sent as a
    // value. A body that asserted every key would silently turn SSH off on a
    // device whose admin only meant to touch exec.
    mockApi.put.mockResolvedValueOnce({
      revision: 1,
      desired: { revision: 1, exec_enabled: true },
    })
    const s = useAgentStore()
    s.agents = [mkAgent()]
    await s.updateDesiredConfig(TENANT_ID, 'a1', { exec_enabled: true })

    const [url, body] = mockApi.put.mock.calls.at(-1)!
    expect(url).toBe(`/tenant/${TENANT_ID}/agent/a1/desired-config`)
    expect(body).toEqual({ exec_enabled: true })
    expect('ssh_enabled' in (body as object)).toBe(false)
  })

  it('a saved request is optimistically PENDING, never applied', async () => {
    // The device has not spoken yet. Writing anything but `pending` would put
    // an answer on screen that no device has given — the exact lie the
    // report-back mechanism exists to prevent.
    mockApi.put.mockResolvedValueOnce({
      revision: 4,
      desired: { revision: 4, exec_enabled: true },
    })
    const s = useAgentStore()
    s.agents = [mkAgent()]
    await s.updateDesiredConfig(TENANT_ID, 'a1', { exec_enabled: true })

    expect(s.agents[0]!.remote_config!.state).toBe('pending')
    expect(s.agents[0]!.remote_config!.report).toBeUndefined()
    expect(s.agents[0]!.remote_config!.desired.exec_enabled).toBe(true)
  })

  it('a device with no remote_config is simply unmanaged', () => {
    // Absent means nobody has requested anything — NOT "everything is off".
    expect(mkAgent().remote_config).toBeUndefined()
  })

  it('a refusal keeps its reason, because the reason IS the next action', async () => {
    // `not_opted_in` (go set a key on the host) and `not_primary` (ask the
    // other org) need completely different responses. Collapsing them into
    // "refused" would leave an operator with nothing to do.
    mockApi.get.mockResolvedValueOnce({
      items: [
        mkAgent({
          remote_config: {
            desired: { revision: 2, exec_enabled: true },
            report: {
              revision: 2,
              outcome: 'not_primary',
              live: [],
              needs_restart: [],
              reported_at: '2026-08-24T00:00:00Z',
            },
            state: 'refused',
          },
        }),
      ],
      total: 1,
      page: 1,
      per_page: 20,
      total_pages: 1,
    })
    const s = useAgentStore()
    await s.fetchAgents(TENANT_ID)
    expect(s.agents[0]!.remote_config!.state).toBe('refused')
    expect(s.agents[0]!.remote_config!.report!.outcome).toBe('not_primary')
  })

  it('needs_restart is never reported as applied', async () => {
    // The keys in that list are written to disk and not in force. Reading it
    // as "applied" tells an operator SSH is open while the device refuses
    // every session.
    mockApi.get.mockResolvedValueOnce({
      items: [
        mkAgent({
          remote_config: {
            desired: { revision: 3, ssh_enabled: true },
            report: {
              revision: 3,
              outcome: 'applied',
              live: [],
              needs_restart: ['ssh_enabled'],
              reported_at: '2026-08-24T00:00:00Z',
            },
            state: 'needs_restart',
          },
        }),
      ],
      total: 1,
      page: 1,
      per_page: 20,
      total_pages: 1,
    })
    const s = useAgentStore()
    await s.fetchAgents(TENANT_ID)
    const rc = s.agents[0]!.remote_config!
    // The device said "applied"; the SERVER resolved that to `needs_restart`
    // because of the key list. The state is the thing to render.
    expect(rc.report!.outcome).toBe('applied')
    expect(rc.state).toBe('needs_restart')
  })
})
