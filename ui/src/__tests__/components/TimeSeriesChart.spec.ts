import { beforeAll, describe, expect, it } from 'vitest'
import { mount } from '@vue/test-utils'
import { createVuetify } from 'vuetify'
import * as components from 'vuetify/components'
import * as directives from 'vuetify/directives'
import TimeSeriesChart from '@/components/stats/TimeSeriesChart.vue'

beforeAll(() => {
  // jsdom lacks ResizeObserver; the chart sizes from props/defaults.
  if (!('ResizeObserver' in globalThis)) {
    ;(globalThis as Record<string, unknown>).ResizeObserver = class {
      observe() {}
      unobserve() {}
      disconnect() {}
    }
  }
})

const vuetify = createVuetify({ components, directives })

const points = [
  { t: 1_700_000_000, online: 1, cpu: 10 },
  { t: 1_700_000_300, online: 3, cpu: 20 },
  { t: 1_700_000_600, online: 2, cpu: null },
]

function mountChart(props: Record<string, unknown>) {
  return mount(TimeSeriesChart, {
    props: { points, series: [{ key: 'online', label: 'Online' }], ...props },
    global: { plugins: [vuetify] },
  })
}

describe('TimeSeriesChart', () => {
  it('renders one path per line series with real path data', () => {
    const w = mountChart({})
    const paths = w.findAll('path')
    expect(paths.length).toBe(1)
    const d = paths[0].attributes('d') ?? ''
    expect(d.startsWith('M')).toBe(true)
    expect(d.length).toBeGreaterThan(10)
  })

  it('area mode adds a fill path under the line', () => {
    const w = mountChart({ area: true })
    expect(w.findAll('path').length).toBe(2)
  })

  it('stacked mode renders one layer per series and tolerates nulls', () => {
    const w = mountChart({
      stacked: true,
      series: [
        { key: 'online', label: 'Online' },
        { key: 'cpu', label: 'CPU' },
      ],
    })
    const paths = w.findAll('path')
    expect(paths.length).toBe(2)
    for (const p of paths) {
      expect((p.attributes('d') ?? '').startsWith('M')).toBe(true)
    }
  })

  it('shows the empty state instead of an SVG when there are no points', () => {
    const w = mountChart({ points: [], emptyText: 'nothing here' })
    expect(w.find('svg').exists()).toBe(false)
    expect(w.text()).toContain('nothing here')
  })

  it('renders axis tick labels', () => {
    const w = mountChart({})
    const ticks = w.findAll('text.ts-tick')
    expect(ticks.length).toBeGreaterThan(2)
  })
})
