// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// FR-116 D4 — a person's usage names each device as the device list does: its
// friendly name (`agent_display_name`, sent beside the machine name since this
// change), else the machine's, else the id's tail. The lanes used the machine
// name alone, so a device renamed "Reception PC" came back as DESKTOP-….
import { describe, expect, it } from 'vitest'
import { mount } from '@vue/test-utils'
import { createVuetify } from 'vuetify'
import * as components from 'vuetify/components'
import * as directives from 'vuetify/directives'

import UsageTimeline from '@/components/stats/UsageTimeline.vue'
// The store's type, never one imported from the `.vue` file: the hosted image's
// build checks `.vue` imports through a shim with a default export only, so a
// named type from an SFC failed that build while local vue-tsc passed.
import type { UsageViewingWindow as ViewingWindow } from '@/stores/stats'

const vuetify = createVuetify({ components, directives })

const T0 = 1_700_000_000
const HOUR = 3600

function win(over: Partial<ViewingWindow>): ViewingWindow {
  return {
    session_id: 's1',
    agent_id: 'aaaaaaaaaaaaaaaaaaaa0001',
    tenant_id: 't1',
    started_at: T0 + HOUR,
    ended_at: T0 + 2 * HOUR,
    seconds: HOUR,
    role: 'controller',
    ...over,
  }
}

describe('UsageTimeline — device names (FR-116 D4)', () => {
  it("labels a lane by the friendly name, else the machine's, else the id's tail", () => {
    const w = mount(UsageTimeline, {
      props: {
        from: T0,
        to: T0 + 4 * HOUR,
        windows: [
          win({ agent_id: 'aaaaaaaaaaaaaaaaaaaa0001', agent_name: 'build-01', agent_display_name: 'Build server', seconds: 3 * HOUR }),
          win({ session_id: 's2', agent_id: 'aaaaaaaaaaaaaaaaaaaa0002', agent_name: 'kiosk-03', agent_display_name: null, seconds: 2 * HOUR }),
          win({ session_id: 's3', agent_id: 'aaaaaaaaaaaaaaaaaaaa0003', seconds: HOUR }),
        ],
      },
      global: { plugins: [vuetify] },
    })
    const labels = w.findAll('.tl-label .text-body-2').map((l) => l.text())
    expect(labels).toEqual(['Build server', 'kiosk-03', 'aa0003'])
    expect(w.find('.tl-bar').attributes('title')).toContain('Controlled Build server')
  })
})
