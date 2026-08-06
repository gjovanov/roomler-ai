import { describe, expect, it } from 'vitest'
import { mount } from '@vue/test-utils'
import { createVuetify } from 'vuetify'
import * as components from 'vuetify/components'
import * as directives from 'vuetify/directives'
import UptimeStrip from '@/components/stats/UptimeStrip.vue'

const vuetify = createVuetify({ components, directives })

const T0 = 1_700_000_000
const HOUR = 3600

function mountStrip(agents: unknown[]) {
  return mount(UptimeStrip, {
    props: { agents: agents as never },
    global: { plugins: [vuetify] },
  })
}

describe('UptimeStrip', () => {
  it('renders one row per agent with a segment per interval', () => {
    const w = mountStrip([
      {
        agent_id: 'aaaaaaaaaaaa111111111111',
        name: 'alpha',
        intervals: [
          { from: T0, to: T0 + HOUR, presence: 'online' },
          { from: T0 + HOUR, to: T0 + 2 * HOUR, presence: 'offline' },
        ],
      },
      {
        agent_id: 'bbbbbbbbbbbb222222222222',
        name: 'beta',
        intervals: [{ from: T0, to: T0 + 2 * HOUR, presence: 'online' }],
      },
    ])
    expect(w.findAll('.uptime-row').length).toBe(2)
    expect(w.findAll('.uptime-seg').length).toBe(3)
    expect(w.text()).toContain('alpha')
    expect(w.text()).toContain('beta')
  })

  it('computes online percentage and sorts the healthiest agent first', () => {
    const w = mountStrip([
      {
        agent_id: 'half',
        name: 'half',
        intervals: [
          { from: T0, to: T0 + HOUR, presence: 'online' },
          { from: T0 + HOUR, to: T0 + 2 * HOUR, presence: 'offline' },
        ],
      },
      {
        agent_id: 'full',
        name: 'full',
        intervals: [{ from: T0, to: T0 + 2 * HOUR, presence: 'online' }],
      },
    ])
    const pcts = w.findAll('.uptime-pct').map((n) => n.text())
    expect(pcts).toEqual(['100%', '50%'])
    // Sorted by uptime: 'full' before 'half'.
    const labels = w.findAll('.uptime-label').map((n) => n.text())
    expect(labels).toEqual(['full', 'half'])
  })

  it('positions segments proportionally across the shared window', () => {
    const w = mountStrip([
      {
        agent_id: 'x',
        name: 'x',
        intervals: [
          { from: T0, to: T0 + HOUR, presence: 'online' },
          { from: T0 + HOUR, to: T0 + 4 * HOUR, presence: 'stale' },
        ],
      },
    ])
    const segs = w.findAll('.uptime-seg')
    expect(segs[0].attributes('style')).toContain('left: 0%')
    expect(segs[0].attributes('style')).toContain('width: 25%')
    expect(segs[1].attributes('style')).toContain('left: 25%')
    expect(segs[1].attributes('style')).toContain('width: 75%')
  })

  it('shows the empty state when nothing was recorded', () => {
    const w = mountStrip([])
    expect(w.find('.uptime-row').exists()).toBe(false)
    expect(w.text()).toContain('No presence history yet')
  })

  it('tolerates an agent with no intervals without breaking the others', () => {
    const w = mountStrip([
      { agent_id: 'empty', name: 'empty', intervals: [] },
      {
        agent_id: 'ok',
        name: 'ok',
        intervals: [{ from: T0, to: T0 + HOUR, presence: 'online' }],
      },
    ])
    expect(w.findAll('.uptime-row').length).toBe(1)
    expect(w.text()).toContain('ok')
  })
})
