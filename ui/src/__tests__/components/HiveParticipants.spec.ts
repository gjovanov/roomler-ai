// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// FR-90 P1c — who takes part in a session. The owner adds people as readers
// or drivers, changes their part and takes them out; anyone else only sees
// who takes part. The server's refusal (a driver without HIVE_RUN) is shown
// in its own words.
import { afterEach, describe, expect, it, vi } from 'vitest'
import { flushPromises, mount } from '@vue/test-utils'
import { createVuetify } from 'vuetify'
import * as components from 'vuetify/components'
import * as directives from 'vuetify/directives'
import { createI18n } from 'vue-i18n'
import en from '@/locales/en.json'

const hoisted = vi.hoisted(() => ({
  fetchParticipants: vi.fn(),
  setParticipant: vi.fn(),
  removeParticipant: vi.fn(),
  apiGet: vi.fn(),
}))
vi.mock('@/stores/hive', () => ({
  useHiveStore: () => ({
    fetchParticipants: hoisted.fetchParticipants,
    setParticipant: hoisted.setParticipant,
    removeParticipant: hoisted.removeParticipant,
  }),
}))
vi.mock('@/api/client', () => ({ api: { get: hoisted.apiGet } }))

import HiveParticipants from '@/components/hive/HiveParticipants.vue'

const vuetify = createVuetify({ components, directives })

/** The dialog inline: its activator opens it, its content is always there. */
const DialogStub = {
  props: ['modelValue'],
  emits: ['update:modelValue'],
  template:
    '<div><slot name="activator" :props="{ onClick: () => $emit(\'update:modelValue\', true) }" /><slot /></div>',
}

const owner = { user_id: 'u-owner', display_name: 'Olga', role: 'owner' }
const reader = { user_id: 'u-reader', display_name: 'Ray', role: 'reader' }

async function render(mayManage: boolean, adopted = false) {
  hoisted.fetchParticipants.mockResolvedValue({ items: [owner, reader], may_manage: mayManage })
  hoisted.apiGet.mockResolvedValue({
    items: [
      { user_id: 'u-owner', display_name: 'Olga', email: 'olga@example.com' },
      { user_id: 'u-reader', display_name: 'Ray', email: 'ray@example.com' },
      { user_id: 'u-carol', display_name: 'Carol', email: 'carol@example.com' },
    ],
  })
  const i18n = createI18n({ legacy: false, locale: 'en', messages: { en } })
  const w = mount(HiveParticipants, {
    props: { tenantId: 't1', sessionId: 's1', adopted },
    global: { plugins: [vuetify, i18n], stubs: { VDialog: DialogStub } },
  })
  await w.find('[data-testid="hive-people"]').trigger('click')
  await flushPromises()
  return w
}

const byTestId = (w: Awaited<ReturnType<typeof render>>, id: string) => w.findAll(`[data-testid="${id}"]`)

afterEach(() => {
  vi.clearAllMocks()
})

describe('HiveParticipants (FR-90 P1c)', () => {
  it('lists who takes part, the owner first, and lets the owner change a part', async () => {
    const w = await render(true)
    expect(hoisted.fetchParticipants).toHaveBeenCalledWith('t1', 's1')
    const people = byTestId(w, 'hive-person')
    expect(people.map((p) => p.attributes('data-role'))).toEqual(['owner', 'reader'])
    // The owner's own row has no controls: the owner always drives.
    expect(people[0].find('[data-testid="hive-person-driver"]').exists()).toBe(false)

    hoisted.setParticipant.mockResolvedValue({ items: [owner, { ...reader, role: 'driver' }], may_manage: true })
    await people[1].find('[data-testid="hive-person-driver"]').trigger('click')
    await flushPromises()
    expect(hoisted.setParticipant).toHaveBeenCalledWith('t1', 's1', 'u-reader', 'driver')
    expect(byTestId(w, 'hive-person').map((p) => p.attributes('data-role'))).toEqual(['owner', 'driver'])
  })

  it("shows the server's refusal in its own words, and changes nothing", async () => {
    const w = await render(true)
    hoisted.setParticipant.mockRejectedValue(
      new Error('they cannot drive agent sessions: their role does not include "Run agent sessions" (HIVE_RUN)'),
    )
    await byTestId(w, 'hive-person')[1].find('[data-testid="hive-person-driver"]').trigger('click')
    await flushPromises()
    expect(byTestId(w, 'hive-people-error')[0].text()).toContain('HIVE_RUN')
    expect(byTestId(w, 'hive-person').map((p) => p.attributes('data-role'))).toEqual(['owner', 'reader'])
  })

  it('adds someone from the org who does not take part yet, and takes someone out', async () => {
    const w = await render(true)
    expect(hoisted.apiGet).toHaveBeenCalledWith(expect.stringMatching(/^\/tenant\/t1\/member\?/))
    const pick = w.findComponent({ name: 'VAutocomplete' })
    const offered = (pick.props('items') as { user_id: string }[]).map((c) => c.user_id)
    expect(offered, 'nobody already in it is offered').toEqual(['u-carol'])

    hoisted.setParticipant.mockResolvedValue({
      items: [owner, reader, { user_id: 'u-carol', display_name: 'Carol', role: 'driver' }],
      may_manage: true,
    })
    pick.vm.$emit('update:modelValue', 'u-carol')
    await flushPromises()
    await byTestId(w, 'hive-people-add-driver')[0].trigger('click')
    await flushPromises()
    expect(hoisted.setParticipant).toHaveBeenCalledWith('t1', 's1', 'u-carol', 'driver')

    hoisted.removeParticipant.mockResolvedValue({ items: [owner, reader], may_manage: true })
    const carol = byTestId(w, 'hive-person').find((p) => p.attributes('data-user') === 'u-carol')!
    await carol.find('[data-testid="hive-person-remove"]').trigger('click')
    await flushPromises()
    expect(hoisted.removeParticipant).toHaveBeenCalledWith('t1', 's1', 'u-carol')
    expect(byTestId(w, 'hive-person').length).toBe(2)
  })

  it('shows anyone but the owner who takes part, with nothing to change', async () => {
    const w = await render(false)
    expect(byTestId(w, 'hive-person').length).toBe(2)
    expect(byTestId(w, 'hive-person-driver').length).toBe(0)
    expect(byTestId(w, 'hive-person-remove').length).toBe(0)
    expect(byTestId(w, 'hive-people-pick').length).toBe(0)
    expect(hoisted.apiGet, "a reader does not page through the org's members").not.toHaveBeenCalled()
  })

  it('offers readers only for an adopted session (P1j): it runs in a terminal', async () => {
    const w = await render(true, true)
    const people = byTestId(w, 'hive-person')
    expect(people[1].find('[data-testid="hive-person-reader"]').exists()).toBe(true)
    expect(people[1].find('[data-testid="hive-person-driver"]').exists()).toBe(false)
    expect(byTestId(w, 'hive-people-add-reader').length).toBe(1)
    expect(byTestId(w, 'hive-people-add-driver').length).toBe(0)
    expect(byTestId(w, 'hive-people-readers-only')[0].text()).toBe('It runs in a terminal, so it takes readers only.')
  })
})
