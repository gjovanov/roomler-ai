// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// #3156 — Device configuration opened with an EMPTY draft the first time after
// a page load, whatever the device's desired config held. AgentsSection mounts
// the dialog with `v-if` on its target and sets the target and the open flag
// in the same tick, so the dialog mounts already open, and a lazy watch on
// `modelValue` never ran for that open. Request was enabled, and a Request
// from there sent only the keys the user touched: every key the device was
// already managed for dropped out of its desired config.
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest'
import { flushPromises, mount, type VueWrapper } from '@vue/test-utils'
import { createVuetify } from 'vuetify'
import * as components from 'vuetify/components'
import * as directives from 'vuetify/directives'
import type { Agent } from '@/stores/agents'

const hoisted = vi.hoisted(() => ({ agent: {} as Record<string, unknown> }))

vi.mock('@/stores/agents', () => ({ useAgentStore: () => hoisted.agent }))
// An owner may grant both exec and SSH, so no switch is disabled.
vi.mock('@/stores/tenant', () => ({
  useTenantStore: () => ({ myPermissions: null, isOwner: true }),
}))

import RemoteConfigDialog from '@/components/admin/RemoteConfigDialog.vue'

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
      width: 1024,
      height: 768,
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

/** The store as a fresh page load has it: the org gates not read yet. */
function agentStore() {
  return {
    orgExecEnabled: null as boolean | null,
    orgSshEnabled: null as boolean | null,
    fetchOrgExecEnabled: vi.fn(),
    fetchOrgSshEnabled: vi.fn(),
    updateDesiredConfig: vi.fn().mockResolvedValue({ revision: 4 }),
  }
}

let store: ReturnType<typeof agentStore>
beforeEach(() => {
  store = agentStore()
  hoisted.agent = store
})

let wrapper: VueWrapper | null = null
afterEach(() => {
  wrapper?.unmount()
  wrapper = null
  document.body.innerHTML = ''
})

const TID = 'aaaaaaaaaaaaaaaaaaaaaaaa'
const AGENT_ID = 'c00000000000000000000001'

/** A device already managed for four keys, as the server recorded them. */
function managedAgent(): Agent {
  return {
    id: AGENT_ID,
    name: 'build-01',
    agent_version: '0.4.188',
    remote_config: {
      desired: {
        revision: 3,
        exec_enabled: true,
        ssh_enabled: false,
        encoder_cells_deny: 'hevc_qsv:yuv444',
        color_description: 'off',
      },
      state: 'applied',
    },
  } as Agent
}

function mountDialog(modelValue: boolean) {
  const w = mount(RemoteConfigDialog, {
    props: { modelValue, tenantId: TID, agent: managedAgent() },
    global: { plugins: [vuetify] },
    attachTo: document.body,
  })
  wrapper = w
  return w
}

// The dialog's content is teleported to <body>, outside the wrapper.

/** The pressed button of the three-state switch whose label names `key`. */
function switchState(key: string): string | undefined {
  return [...document.body.querySelectorAll('.v-btn-toggle')]
    .find((t) => t.parentElement?.textContent?.includes(`(${key})`))
    ?.querySelector('.v-btn--active')
    ?.textContent?.trim()
}

/** The input of the text field whose label names `key`. */
function field(key: string): HTMLInputElement | null | undefined {
  return [...document.body.querySelectorAll('.v-text-field')]
    .find((f) => f.textContent?.includes(`(${key})`))
    ?.querySelector('input')
}

/** What the select whose label names `key` shows as chosen. */
function selection(key: string): string | undefined {
  return [...document.body.querySelectorAll('.v-select')]
    .find((s) => s.textContent?.includes(`(${key})`))
    ?.querySelector('.v-select__selection-text')
    ?.textContent?.trim()
}

function button(text: string): HTMLButtonElement | undefined {
  return [...document.body.querySelectorAll<HTMLButtonElement>('.v-card-actions button')].find(
    (b) => b.textContent?.trim() === text,
  )
}

/** The recorded config, shown as recorded, with nothing yet to request. */
function expectRecordedDraft() {
  expect(switchState('exec_enabled')).toBe('On')
  expect(switchState('ssh_enabled')).toBe('Off')
  expect(field('encoder_cells_deny')?.value).toBe('hevc_qsv:yuv444')
  expect(selection('color_description')).toBe('Off: no colour description, as earlier agents sent')
  expect(button('Request')?.disabled).toBe(true)
}

describe('RemoteConfigDialog fills its draft from the device (#3156)', () => {
  it('on the first open, when it mounts already open', async () => {
    mountDialog(true)
    await flushPromises()
    expectRecordedDraft()
    // And reads the org gates, as every later open does.
    expect(store.fetchOrgExecEnabled).toHaveBeenCalledWith(TID)
    expect(store.fetchOrgSshEnabled).toHaveBeenCalledWith(TID)
  })

  it('when a mounted, closed dialog opens', async () => {
    const w = mountDialog(false)
    await flushPromises()
    // Mounted closed, it asks for nothing and emits nothing.
    expect(store.fetchOrgExecEnabled).not.toHaveBeenCalled()
    expect(store.fetchOrgSshEnabled).not.toHaveBeenCalled()
    expect(w.emitted('update:modelValue')).toBeUndefined()
    await w.setProps({ modelValue: true })
    await flushPromises()
    expectRecordedDraft()
  })

  it('so a Request from the first open keeps every key already managed', async () => {
    mountDialog(true)
    await flushPromises()
    const fps = field('fps_deny')!
    fps.value = '*:144'
    fps.dispatchEvent(new Event('input'))
    await flushPromises()
    button('Request')!.click()
    await flushPromises()
    expect(store.updateDesiredConfig).toHaveBeenCalledWith(TID, AGENT_ID, {
      exec_enabled: true,
      ssh_enabled: false,
      encoder_cells_deny: 'hevc_qsv:yuv444',
      fps_deny: '*:144',
      color_description: 'off',
    })
  })
})
