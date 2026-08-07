import { beforeAll, describe, expect, it } from 'vitest'
import { mount } from '@vue/test-utils'
import { createVuetify } from 'vuetify'
import * as components from 'vuetify/components'
import * as directives from 'vuetify/directives'
import MeshGraph from '@/components/stats/MeshGraph.vue'

beforeAll(() => {
  if (!('ResizeObserver' in globalThis)) {
    ;(globalThis as Record<string, unknown>).ResizeObserver = class {
      observe() {}
      unobserve() {}
      disconnect() {}
    }
  }
})

const vuetify = createVuetify({ components, directives })

const nodes = [
  { id: 'n1', name: 'alpha', online: true },
  { id: 'n2', name: 'beta', online: true },
  { id: 'n3', name: 'gamma', online: false },
]
const edges = [
  { from: 'n1', to: 'n2', carrier: 'direct', rtt_ms: 12 },
  { from: 'n1', to: 'n3', carrier: 'derp', rtt_ms: 90, stalled: true },
  { from: 'n2', to: 'n3', carrier: 'relay', rtt_ms: 40, reports: 1 },
]

function mountGraph(props: Record<string, unknown> = {}) {
  return mount(MeshGraph, {
    props: { nodes, edges, ...props },
    global: { plugins: [vuetify] },
  })
}

describe('MeshGraph', () => {
  it('places every device on the ring and draws each carrier edge', () => {
    const w = mountGraph()
    expect(w.findAll('.mesh-node').length).toBe(3)
    // 3 chords + the ring guide is a <circle>, so count paths only.
    expect(w.findAll('path').length).toBe(3)
    for (const p of w.findAll('path')) {
      expect((p.attributes('d') ?? '').startsWith('M')).toBe(true)
    }
  })

  it('draws a control-plane spoke per visible device and can hide them', async () => {
    const w = mountGraph()
    expect(w.findAll('line.mesh-spoke').length).toBe(3)
    expect(w.find('circle.mesh-center').exists()).toBe(true)

    await w.setValue?.(false) // no-op guard for older utils
    const vm = w.vm as unknown as { showControlPlane: boolean }
    vm.showControlPlane = false
    await w.vm.$nextTick()
    expect(w.findAll('line.mesh-spoke').length).toBe(0)
    expect(w.find('circle.mesh-center').exists()).toBe(false)
  })

  it('hiding offline devices also drops the edges that touch them', async () => {
    const w = mountGraph()
    const vm = w.vm as unknown as { showOffline: boolean }
    vm.showOffline = false
    await w.vm.$nextTick()
    // gamma is offline: only the alpha↔beta edge survives.
    expect(w.findAll('.mesh-node').length).toBe(2)
    expect(w.findAll('path').length).toBe(1)
  })

  it('carrier toggles filter edges without touching nodes', async () => {
    const w = mountGraph()
    const vm = w.vm as unknown as { shownCarriers: string[] }
    vm.shownCarriers = ['direct']
    await w.vm.$nextTick()
    expect(w.findAll('path').length).toBe(1)
    expect(w.findAll('.mesh-node').length).toBe(3)
  })

  it('marks a stalled carrier as dashed', async () => {
    const w = mountGraph()
    const vm = w.vm as unknown as { shownCarriers: string[] }
    vm.shownCarriers = ['derp']
    await w.vm.$nextTick()
    const p = w.findAll('path')[0]
    expect(p.attributes('stroke-dasharray')).toBeTruthy()
  })

  it('emits the node id when a device is clicked', async () => {
    const w = mountGraph()
    await w.findAll('.mesh-node')[0].trigger('click')
    expect(w.emitted('select')?.[0]).toBeTruthy()
  })

  it('shows the empty state with no devices', () => {
    const w = mountGraph({ nodes: [], edges: [] })
    expect(w.find('svg').exists()).toBe(false)
    expect(w.text()).toContain('No devices')
  })
})
