// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// FR-88 (#1790) §3b — `install-copy` in the app: copying an install or
// enroll COMMAND counts, copying the bare token does not, a failed clipboard
// write does not, and without purestat the copy still works.
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { flushPromises, mount } from '@vue/test-utils'
import { createVuetify } from 'vuetify'
import * as components from 'vuetify/components'
import * as directives from 'vuetify/directives'
import EnrollmentDialog from '@/components/enroll/EnrollmentDialog.vue'

const vuetify = createVuetify({ components, directives })
type W = Window & { purestat?: unknown }
const purestat = vi.fn()
let writeText: ReturnType<typeof vi.fn>

async function open() {
  const w = mount(EnrollmentDialog, {
    props: { modelValue: true, kind: 'agent', token: 'tok-123', expiresIn: 600, loading: false, error: null },
    // The overlay machinery needs browser APIs jsdom lacks; what is under
    // test is the dialog's content, so the dialog renders it in place.
    global: { plugins: [vuetify], stubs: { VDialog: { template: '<div><slot /></div>' } } },
    attachTo: document.body,
  })
  await flushPromises()
  return w
}

const buttons = (label: string) =>
  [...document.body.querySelectorAll('button')].filter((b) => b.textContent!.trim() === label) as HTMLButtonElement[]

beforeEach(() => {
  vi.stubGlobal(
    'ResizeObserver',
    class {
      observe() {}
      unobserve() {}
      disconnect() {}
    },
  )
  writeText = vi.fn(() => Promise.resolve())
  Object.defineProperty(navigator, 'clipboard', { value: { writeText }, configurable: true })
  purestat.mockReset()
  ;(window as W).purestat = purestat
})

afterEach(() => {
  delete (window as W).purestat
  vi.unstubAllGlobals()
  document.body.innerHTML = ''
})

describe('EnrollmentDialog — install-copy (FR-88)', () => {
  it('fires once when a command is copied', async () => {
    const w = await open()
    const copy = buttons('Copy')
    expect(copy.length).toBeGreaterThan(0)
    copy[0]!.click()
    await flushPromises()
    expect(writeText).toHaveBeenCalledTimes(1)
    expect(writeText.mock.calls[0]![0]).toMatch(/install\.(?:ps1|sh)|enroll --server/)
    expect(purestat.mock.calls).toEqual([['install-copy']])
    w.unmount()
  })

  it('does not fire for the bare token', async () => {
    const w = await open()
    buttons('Copy token')[0]!.click()
    await flushPromises()
    expect(writeText).toHaveBeenCalledWith('tok-123')
    expect(purestat).not.toHaveBeenCalled()
    w.unmount()
  })

  it('does not fire when the clipboard refuses', async () => {
    writeText.mockRejectedValueOnce(new Error('NotAllowedError'))
    const w = await open()
    buttons('Copy')[0]!.click()
    await flushPromises()
    expect(purestat).not.toHaveBeenCalled()
    w.unmount()
  })

  it('still copies without purestat', async () => {
    delete (window as W).purestat
    const w = await open()
    buttons('Copy')[0]!.click()
    await flushPromises()
    expect(writeText).toHaveBeenCalledTimes(1)
    expect(document.body.textContent).toContain('Copied')
    w.unmount()
  })
})
