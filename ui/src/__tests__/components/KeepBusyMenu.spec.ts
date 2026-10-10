// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// FR-92 — the Keep busy menu renders the device's state and asks for
// changes; it never decides anything itself.
import { beforeAll, describe, expect, it } from 'vitest'
import { flushPromises, mount } from '@vue/test-utils'
import { createVuetify } from 'vuetify'
import * as components from 'vuetify/components'
import * as directives from 'vuetify/directives'
import KeepBusyMenu from '@/components/remote/KeepBusyMenu.vue'
import { parseKeepBusyState, type KeepBusyRequest, type KeepBusyState } from '@/composables/keepBusy'

const vuetify = createVuetify({ components, directives })

beforeAll(() => {
  // Vuetify overlays measure themselves against the viewport; jsdom has
  // neither a ResizeObserver nor a visualViewport.
  const g = globalThis as unknown as Record<string, unknown>
  if (!('ResizeObserver' in g)) {
    g.ResizeObserver = class {
      observe() {}
      unobserve() {}
      disconnect() {}
    }
  }
  if (!('visualViewport' in g)) {
    g.visualViewport = {
      width: 1280,
      height: 800,
      offsetLeft: 0,
      offsetTop: 0,
      pageLeft: 0,
      pageTop: 0,
      scale: 1,
      addEventListener() {},
      removeEventListener() {},
    }
  }
})

function state(over: Record<string, unknown> = {}): KeepBusyState {
  const s = parseKeepBusyState(
    {
      t: 'rc:keep-busy.state',
      rev: 1,
      available: true,
      on: false,
      phase: 'off',
      reason: null,
      sentence: null,
      pattern: 'circle',
      size: 'm',
      speed: 'normal',
      resume_after_s: 30,
      warn: [],
      ...over,
    },
    Date.now(),
  )
  if (!s) throw new Error('fixture did not parse')
  return s
}

function render(props: { state: KeepBusyState | null; canControl: boolean }) {
  return mount(KeepBusyMenu, {
    props: { ...props, agentId: 'agent-1' },
    global: { plugins: [vuetify] },
    attachTo: document.body,
  })
}

describe('KeepBusyMenu (FR-92)', () => {
  it('is a plain button while keep busy is off', () => {
    const w = render({ state: state(), canControl: true })
    expect(w.find('[data-testid="rc-keep-busy-btn"]').exists()).toBe(true)
    expect(w.find('[data-testid="rc-keep-busy-chip"]').exists()).toBe(false)
    w.unmount()
  })

  it('shows a chip with what the device is drawing while it runs', () => {
    const w = render({ state: state({ on: true, phase: 'running', pattern: 'heart' }), canControl: true })
    expect(w.find('[data-testid="rc-keep-busy-chip"]').text()).toContain('Keep busy · heart')
    w.unmount()
  })

  it('counts a pause down in the chip', () => {
    const w = render({
      state: state({ on: true, phase: 'paused', reason: 'user_active', resumes_in_ms: 23_000 }),
      canControl: true,
    })
    expect(w.find('[data-testid="rc-keep-busy-chip"]').text()).toMatch(/Keep busy · paused 2[23]s/)
    w.unmount()
  })

  it('asks the device to turn it on with the chosen settings', async () => {
    const w = render({ state: state(), canControl: true })
    await w.find('[data-testid="rc-keep-busy-btn"]').trigger('click')
    await flushPromises()
    const star = document.querySelector<HTMLButtonElement>('[data-testid="rc-keep-busy-pattern-star"]')
    expect(star, 'the menu opened').not.toBeNull()
    star!.click()
    await flushPromises()
    // While off, choosing a pattern sends nothing.
    expect(w.emitted('set')).toBeUndefined()
    const input = document.querySelector<HTMLInputElement>('[data-testid="rc-keep-busy-switch"] input')
    input!.click()
    await flushPromises()
    const sent = w.emitted('set') as KeepBusyRequest[][]
    expect(sent).toHaveLength(1)
    expect(sent[0][0]).toMatchObject({ on: true, pattern: 'star', speed: 'normal', size: 'm', resumeAfterS: 30 })
    w.unmount()
  })

  it('lets a view-only session watch but not change it', async () => {
    const w = render({ state: state({ on: true, phase: 'running' }), canControl: false })
    await w.find('[data-testid="rc-keep-busy-chip"]').trigger('click')
    await flushPromises()
    const input = document.querySelector<HTMLInputElement>('[data-testid="rc-keep-busy-switch"] input')
    expect(input?.disabled).toBe(true)
    expect(document.body.textContent).toContain('view-only')
    w.unmount()
  })
})
