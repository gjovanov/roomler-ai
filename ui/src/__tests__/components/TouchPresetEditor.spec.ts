// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
import { afterEach, beforeAll, describe, expect, it, vi, type Mock } from 'vitest'
import { flushPromises, mount, type VueWrapper } from '@vue/test-utils'
import { createVuetify } from 'vuetify'
import * as components from 'vuetify/components'
import * as directives from 'vuetify/directives'
import TouchPresetEditor from '@/components/remote/TouchPresetEditor.vue'
import { BUILTIN_PRESETS, HID, type TouchControl, type TouchPreset } from '@/composables/touchControls'
import { describeRefusal, MINE_PRESET_PREFIX, validatePreset } from '@/composables/touchPresetEdit'

const vuetify = createVuetify({ components, directives })

beforeAll(() => {
  const g = globalThis as unknown as Record<string, unknown>
  if (!('ResizeObserver' in g)) {
    g.ResizeObserver = class {
      observe() {}
      unobserve() {}
      disconnect() {}
    }
  }
})

let wrapper: VueWrapper | null = null
afterEach(() => {
  wrapper?.unmount()
  wrapper = null
})

type PublishFn = (preset: { name: string; controls: TouchControl[] }, serverId: string | null) => Promise<string | null>
type DeleteOrgFn = (serverId: string) => Promise<string | null>

function mountEditor(
  over: Partial<{
    mine: TouchPreset[]
    org: TouchPreset[]
    canPublish: boolean
    publish: Mock<PublishFn>
    deleteOrg: Mock<DeleteOrgFn>
  }> = {},
) {
  const publish = over.publish ?? vi.fn<PublishFn>().mockResolvedValue(null)
  const deleteOrg = over.deleteOrg ?? vi.fn<DeleteOrgFn>().mockResolvedValue(null)
  const w = mount(TouchPresetEditor, {
    props: {
      builtins: BUILTIN_PRESETS,
      mine: over.mine ?? [],
      org: over.org ?? [],
      canPublish: over.canPublish ?? false,
      publish,
      deleteOrg,
    },
    global: { plugins: [vuetify] },
    attachTo: document.body,
  })
  wrapper = w
  return { w, publish, deleteOrg }
}

const by = (w: VueWrapper, id: string) => w.find(`[data-testid="${id}"]`)

function key(code: string) {
  window.dispatchEvent(new KeyboardEvent('keydown', { code, bubbles: true, cancelable: true }))
}

describe('FR-111 P4b-2 the layout editor', () => {
  it('copies a built-in before it changes, and saves the copy as a member’s own', async () => {
    const { w } = mountEditor()
    await by(w, `rc-tpe-item-${BUILTIN_PRESETS[0].id}`).trigger('click')
    expect(by(w, 'rc-tpe-readonly').exists()).toBe(true)
    expect(by(w, 'rc-tpe-save-mine').attributes('disabled')).toBeDefined()
    await by(w, 'rc-tpe-copy').trigger('click')
    expect(by(w, 'rc-tpe-readonly').exists()).toBe(false)
    await by(w, 'rc-tpe-save-mine').trigger('click')
    const saved = w.emitted('save-mine')?.[0]?.[0] as TouchPreset
    expect(saved.id.startsWith(MINE_PRESET_PREFIX)).toBe(true)
    expect(saved.name).toBe(`Copy of ${BUILTIN_PRESETS[0].name}`)
    expect(saved.controls).toEqual(BUILTIN_PRESETS[0].controls)
  })

  it('captures a chord from the keyboard and builds a macro, and what it saves is a layout the server takes', async () => {
    const { w } = mountEditor()
    await by(w, 'rc-tpe-new').trigger('click')
    // A key button: capture Ctrl + C.
    await by(w, 'rc-tpe-capture-0-0').trigger('click')
    expect(by(w, 'rc-tpe-capturing').exists()).toBe(true)
    key('ControlLeft')
    key('KeyC')
    key('KeyC') // a repeat of one key adds nothing
    await by(w, 'rc-tpe-capture-done').trigger('click')
    expect(by(w, 'rc-tpe-capture-0-0').text()).toBe('Ctrl + C')
    // A second button, made a macro with two steps.
    await by(w, 'rc-tpe-btn-add-0').trigger('click')
    await by(w, 'rc-tpe-btn-kind-0-1-macro').trigger('click')
    await by(w, 'rc-tpe-step-add-0-1').trigger('click')
    await by(w, 'rc-tpe-step-target-0-1-1').trigger('click')
    key('KeyV') // a step takes one key, and the capture ends by itself
    await flushPromises()
    expect(by(w, 'rc-tpe-capturing').exists()).toBe(false)
    await by(w, 'rc-tpe-add-dpad').trigger('click')
    await by(w, 'rc-tpe-save-mine').trigger('click')
    const saved = w.emitted('save-mine')?.[0]?.[0] as TouchPreset
    expect(validatePreset(saved)).toBeNull()
    const row = saved.controls[0] as Extract<TouchPreset['controls'][number], { kind: 'row' }>
    expect(row.buttons[0].action).toEqual({ kind: 'keys', keys: [HID.ControlLeft, HID.KeyC] })
    expect(row.buttons[1].action).toEqual({
      kind: 'macro',
      steps: [
        { op: 'tap', key: 0x2c, wait_ms: 0 },
        { op: 'tap', key: HID.KeyV, wait_ms: 0 },
      ],
    })
    expect(saved.controls.map((c) => c.kind)).toEqual(['row', 'dpad'])
  })

  it('says why a draft cannot be saved, and keeps the save off', async () => {
    const { w } = mountEditor()
    await by(w, 'rc-tpe-new').trigger('click')
    await by(w, 'rc-tpe-name').find('input').setValue('   ')
    expect(by(w, 'rc-tpe-error').text()).toBe(describeRefusal('name_empty'))
    expect(by(w, 'rc-tpe-save-mine').attributes('disabled')).toBeDefined()
    await by(w, 'rc-tpe-name').find('input').setValue('Fine')
    expect(by(w, 'rc-tpe-error').exists()).toBe(false)
    expect(by(w, 'rc-tpe-save-mine').attributes('disabled')).toBeUndefined()
  })

  it('publishes to the org only with the right, and says what the server refused', async () => {
    const plain = mountEditor()
    await by(plain.w, 'rc-tpe-new').trigger('click')
    expect(by(plain.w, 'rc-tpe-publish').exists()).toBe(false)
    plain.w.unmount()
    wrapper = null

    const publish = vi.fn<PublishFn>().mockResolvedValue('name_taken')
    const { w } = mountEditor({ canPublish: true, publish })
    await by(w, 'rc-tpe-new').trigger('click')
    await by(w, 'rc-tpe-publish').trigger('click')
    await flushPromises()
    expect(publish).toHaveBeenCalledTimes(1)
    const [body, serverId] = publish.mock.calls[0]
    expect(serverId).toBeNull()
    expect(validatePreset(body)).toBeNull()
    expect(Object.keys(body).sort()).toEqual(['controls', 'name'])
    expect(by(w, 'rc-tpe-error').text()).toBe(describeRefusal('name_taken'))
  })

  it('an org layout is changed only with the right, by its own server id', async () => {
    const org: TouchPreset = { id: 'org:abc123', name: 'Shared', controls: BUILTIN_PRESETS[1].controls }
    const ro = mountEditor({ org: [org] })
    await by(ro.w, 'rc-tpe-item-org:abc123').trigger('click')
    expect(by(ro.w, 'rc-tpe-readonly').exists()).toBe(true)
    expect(by(ro.w, 'rc-tpe-delete').exists()).toBe(false)
    ro.w.unmount()
    wrapper = null

    const { w, publish, deleteOrg } = mountEditor({ org: [org], canPublish: true })
    await by(w, 'rc-tpe-item-org:abc123').trigger('click')
    expect(by(w, 'rc-tpe-readonly').exists()).toBe(false)
    await by(w, 'rc-tpe-publish').trigger('click')
    await flushPromises()
    expect(publish.mock.calls[0][1]).toBe('abc123')
    await by(w, 'rc-tpe-delete').trigger('click')
    await flushPromises()
    expect(deleteOrg).toHaveBeenCalledWith('abc123')
  })

  it('deletes a member’s own layout', async () => {
    const mine: TouchPreset = { id: `${MINE_PRESET_PREFIX}m1`, name: 'Mine', controls: BUILTIN_PRESETS[0].controls }
    const { w } = mountEditor({ mine: [mine] })
    await by(w, `rc-tpe-item-${mine.id}`).trigger('click')
    await by(w, 'rc-tpe-delete').trigger('click')
    expect(w.emitted('delete-mine')?.[0]).toEqual([mine.id])
  })
})
