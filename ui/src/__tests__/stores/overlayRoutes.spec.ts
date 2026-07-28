import { beforeEach, describe, expect, it, vi } from 'vitest'
import { createPinia, setActivePinia } from 'pinia'

vi.mock('@/api/client', () => ({
  api: {
    get: vi.fn(),
    post: vi.fn(),
    put: vi.fn(),
    delete: vi.fn(),
  },
}))

import {
  deriveOverlayV6,
  useOverlayRoutesStore,
  type OverlayNode,
} from '@/stores/overlayRoutes'
import { api } from '@/api/client'

const mockApi = vi.mocked(api)
const TENANT_ID = 'ten_1'

function mkNode(over: Partial<OverlayNode> = {}): OverlayNode {
  return {
    id: 'n1',
    name: 'laptop',
    overlay_ip: '100.64.0.7',
    kind: 'agent',
    advertised_routes: [],
    approved_routes: [],
    is_exit_node: false,
    can_be_exit_node: false,
    online: true,
    will_rejoin: true,
    last_seen_at: '2026-07-28T00:00:00Z',
    ...over,
  }
}

// The TS mirror of `overlay/router.rs::derive_overlay_v6` — its output must
// match Rust's `Ipv6Addr` Display exactly (the Rust side pins
// `derive_overlay_v6(100.64.3.129) == "fd72:6f6f:6d6c::6440:381"` in a test).
describe('deriveOverlayV6', () => {
  it('embeds the overlay v4 in the ULA /96 like the Rust derivation', () => {
    expect(deriveOverlayV6('100.64.3.129')).toBe('fd72:6f6f:6d6c::6440:381')
    expect(deriveOverlayV6('100.64.0.2')).toBe('fd72:6f6f:6d6c::6440:2')
    expect(deriveOverlayV6('100.127.255.255')).toBe('fd72:6f6f:6d6c::647f:ffff')
  })

  it('folds a zero high segment into the :: (Rust Display parity)', () => {
    expect(deriveOverlayV6('0.0.0.9')).toBe('fd72:6f6f:6d6c::9')
    expect(deriveOverlayV6('0.0.0.0')).toBe('fd72:6f6f:6d6c::')
  })

  it('rejects malformed input', () => {
    expect(deriveOverlayV6('')).toBeNull()
    expect(deriveOverlayV6('100.64.0')).toBeNull()
    expect(deriveOverlayV6('100.64.0.256')).toBeNull()
    expect(deriveOverlayV6('not-an-ip')).toBeNull()
  })
})

describe('useOverlayRoutesStore', () => {
  beforeEach(() => {
    setActivePinia(createPinia())
    vi.clearAllMocks()
  })

  it('fetchNodes populates the list', async () => {
    mockApi.get.mockResolvedValueOnce({ items: [mkNode(), mkNode({ id: 'n2' })] })
    const store = useOverlayRoutesStore()
    await store.fetchNodes(TENANT_ID)
    expect(mockApi.get).toHaveBeenCalledWith(`/tenant/${TENANT_ID}/overlay-node`)
    expect(store.nodes).toHaveLength(2)
    expect(store.error).toBeNull()
  })

  it('fetchNodes clears the list and records the error on failure', async () => {
    mockApi.get.mockRejectedValueOnce(new Error('boom'))
    const store = useOverlayRoutesStore()
    store.nodes = [mkNode()]
    await store.fetchNodes(TENANT_ID)
    expect(store.nodes).toEqual([])
    expect(store.error).toBe('boom')
  })

  it('evictNode DELETEs the node and drops only that row', async () => {
    mockApi.delete.mockResolvedValueOnce({
      released: true,
      node_id: 'n1',
      name: 'laptop',
      overlay_ip: '100.64.0.7',
      host_recycled: true,
    })
    const store = useOverlayRoutesStore()
    store.nodes = [mkNode({ id: 'n1' }), mkNode({ id: 'n2' })]

    const res = await store.evictNode(TENANT_ID, 'n1')

    expect(mockApi.delete).toHaveBeenCalledWith(
      `/tenant/${TENANT_ID}/overlay-node/n1`,
    )
    expect(store.nodes.map((n) => n.id)).toEqual(['n2'])
    expect(res.host_recycled).toBe(true)
  })

  // Locks the await-before-filter ordering: a failed evict must not make the
  // node disappear from the admin's view.
  it('a rejected evictNode propagates and leaves the list intact', async () => {
    mockApi.delete.mockRejectedValueOnce(new Error('forbidden'))
    const store = useOverlayRoutesStore()
    store.nodes = [mkNode({ id: 'n1' }), mkNode({ id: 'n2' })]

    await expect(store.evictNode(TENANT_ID, 'n1')).rejects.toThrow('forbidden')
    expect(store.nodes.map((n) => n.id)).toEqual(['n1', 'n2'])
  })
})
