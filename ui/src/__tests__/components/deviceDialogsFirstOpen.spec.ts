// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// #3156 in Device configuration's four sibling dialogs. AgentsSection mounts
// each with `v-if` on its target and sets the target and the open flag in the
// same tick, so a first open after a page load MOUNTS the dialog already open,
// and a lazy watch on `modelValue` never ran for it. The three policy dialogs
// showed their closed default instead of the device's stored policy, and their
// Save (a full replace, with no dirty check) wrote that default over it: exec
// or SSH switched off, an external-access approval revoked. The console never
// read the org's exec switch, so a closed org gate went unexplained.
//
// #3182, in External access: it copied the connect code from the store at the
// open, before its own fetch landed, so a fresh page load showed "Not yet
// created" and offered Create, which rotates the code the device already has.
//
// #3197, also in External access: the list it read holds approved devices only,
// so a code on an unapproved device still read as none. It now asks for the
// device's own code.
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest'
import { flushPromises, mount, type VueWrapper } from '@vue/test-utils'
import { reactive } from 'vue'
import { createVuetify } from 'vuetify'
import * as components from 'vuetify/components'
import * as directives from 'vuetify/directives'
import type {
  Agent,
  AgentCapabilities,
  ExecPolicy,
  ExternalAccessPolicy,
  SshPolicy,
} from '@/stores/agents'

const hoisted = vi.hoisted(() => ({ agent: {} as Record<string, unknown> }))

vi.mock('@/stores/agents', () => ({ useAgentStore: () => hoisted.agent }))

import DeviceConsoleDialog from '@/components/admin/DeviceConsoleDialog.vue'
import ExecPolicyDialog from '@/components/admin/ExecPolicyDialog.vue'
import ExternalAccessDialog from '@/components/admin/ExternalAccessDialog.vue'
import SshPolicyDialog from '@/components/admin/SshPolicyDialog.vue'

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

/** The store as a fresh page load has it: nothing org-wide read yet. Reactive,
 *  as a Pinia store is, so a fetch that lands re-renders what reads it. */
function agentStore() {
  return reactive({
    orgExecEnabled: null as boolean | null,
    orgSshEnabled: null as boolean | null,
    orgExternalAccessEnabled: null as boolean | null,
    tenantMembers: [] as { user_id: string; display_name: string; nickname: string | null }[],
    externalDevices: [] as { id: string; connect_code: string | null }[],
    fetchOrgExecEnabled: vi.fn(),
    fetchOrgSshEnabled: vi.fn(),
    fetchTenantMembers: vi.fn(),
    fetchExternalAccess: vi.fn(),
    fetchConnectCode: vi.fn().mockResolvedValue(null),
    rotateConnectCode: vi.fn(),
    updateExecPolicy: vi.fn().mockResolvedValue(undefined),
    updateSshPolicy: vi.fn().mockResolvedValue(undefined),
    updateExternalAccessPolicy: vi.fn().mockResolvedValue(undefined),
    execOnAgent: vi.fn(),
  })
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

function track<W extends VueWrapper>(w: W): W {
  wrapper = w
  return w
}

const TID = 'aaaaaaaaaaaaaaaaaaaaaaaa'
const AGENT_ID = 'c00000000000000000000001'

function device(over: Partial<Agent>): Agent {
  return {
    id: AGENT_ID,
    name: 'build-01',
    os: 'linux',
    agent_version: '0.4.189',
    is_online: true,
    presence: 'online',
    ...over,
  } as Agent
}

/** Capabilities that say only the given rpc words. */
function caps(...rpc: string[]): AgentCapabilities {
  return {
    codecs: [],
    hw_encoders: [],
    has_input_permission: true,
    supports_clipboard: false,
    supports_file_transfer: false,
    max_simultaneous_sessions: 1,
    rpc,
  }
}

/** Mount options as the Devices page passes them: open, unless told not. */
function opened(agent: Agent, modelValue = true) {
  return {
    props: { modelValue, tenantId: TID, agent },
    global: { plugins: [vuetify] },
    attachTo: document.body,
  }
}

// A dialog's content is teleported to <body>, outside its wrapper.

/** Is the switch whose label holds `label` on? */
function switchOn(label: string): boolean | undefined {
  return [...document.body.querySelectorAll('.v-switch')]
    .find((s) => s.textContent?.includes(label))
    ?.querySelector<HTMLInputElement>('input[type="checkbox"]')?.checked
}

/** What the select labelled `label` shows as chosen. */
function chosen(label: string): string[] | undefined {
  const select = [...document.body.querySelectorAll('.v-select')].find(
    (s) => s.querySelector('.v-field-label')?.textContent?.trim() === label,
  )
  if (!select) return undefined
  return [...select.querySelectorAll('.v-select__selection')].map(
    (e) => e.textContent?.trim() ?? '',
  )
}

function button(text: string): HTMLButtonElement | undefined {
  return [...document.body.querySelectorAll<HTMLButtonElement>('.v-card-actions button')].find(
    (b) => b.textContent?.trim() === text,
  )
}

function alertShown(text: string): boolean {
  return [...document.body.querySelectorAll('.v-alert')].some((a) =>
    a.textContent?.replace(/\s+/g, ' ').includes(text),
  )
}

describe('ExecPolicyDialog on its first open (#3156)', () => {
  /** Unattended commands in bash, and the device may originate. */
  const STORED: ExecPolicy = {
    mode: 'on',
    can_originate: true,
    allowed_user_ids: [],
    allowed_role_ids: [],
    consent_mode: 'auto',
    shells: ['bash'],
  }

  it('shows the stored policy, not the closed default', async () => {
    const w = track(mount(ExecPolicyDialog, opened(device({ exec_policy: STORED }))))
    await flushPromises()
    expect(switchOn('Accept remote commands on this device')).toBe(true)
    expect(switchOn('run commands on OTHER devices')).toBe(true)
    expect(chosen('Consent')).toEqual(['Unattended — run immediately'])
    expect(chosen('Allowed shells (empty = any the host supports)')).toEqual(['bash'])
    // It reads what every later open reads, and writes nothing.
    expect(store.fetchTenantMembers).toHaveBeenCalledWith(TID)
    expect(store.fetchOrgExecEnabled).toHaveBeenCalledWith(TID)
    expect(store.updateExecPolicy).not.toHaveBeenCalled()
    expect(w.emitted('update:modelValue')).toBeUndefined()
  })

  it('so a Save from it keeps the stored policy instead of switching exec off', async () => {
    track(mount(ExecPolicyDialog, opened(device({ exec_policy: STORED }))))
    await flushPromises()
    button('Save')!.click()
    await flushPromises()
    expect(store.updateExecPolicy).toHaveBeenCalledWith(TID, AGENT_ID, STORED)
  })
})

describe('SshPolicyDialog on its first open (#3156)', () => {
  /** Unattended sessions as SYSTEM. */
  const STORED: SshPolicy = {
    mode: 'on',
    can_originate: false,
    allowed_user_ids: [],
    allowed_role_ids: [],
    account_mode: 'daemon',
    account: null,
    consent_mode: 'auto',
  }
  /** A Windows device whose SYSTEM service serves SSH. */
  const windows = (ssh_policy?: SshPolicy) =>
    device({ os: 'windows', ssh_policy, capabilities: caps('ssh-system') })

  it('shows the stored policy, not the closed Unix default', async () => {
    const w = track(mount(SshPolicyDialog, opened(windows(STORED))))
    await flushPromises()
    expect(switchOn('Accept SSH sessions on this device')).toBe(true)
    expect(chosen('Sessions run as')).toEqual(['The daemon account (SYSTEM)'])
    expect(chosen('Consent')).toEqual(['Unattended — open immediately'])
    // It reads what every later open reads, and writes nothing.
    expect(store.fetchTenantMembers).toHaveBeenCalledWith(TID)
    expect(store.fetchOrgSshEnabled).toHaveBeenCalledWith(TID)
    expect(store.updateSshPolicy).not.toHaveBeenCalled()
    expect(w.emitted('update:modelValue')).toBeUndefined()
  })

  it('so a Save from it, a full replace, keeps the stored policy', async () => {
    track(mount(SshPolicyDialog, opened(windows(STORED))))
    await flushPromises()
    button('Save')!.click()
    await flushPromises()
    expect(store.updateSshPolicy).toHaveBeenCalledWith(TID, AGENT_ID, STORED)
  })

  it('opens and saves a Windows device with no policy as the signed-in user, never a named Unix account', async () => {
    track(mount(SshPolicyDialog, opened(windows())))
    await flushPromises()
    expect(chosen('Sessions run as')).toEqual(['The signed-in user at the device'])
    button('Save')!.click()
    await flushPromises()
    expect(store.updateSshPolicy).toHaveBeenCalledWith(
      TID,
      AGENT_ID,
      expect.objectContaining({ mode: 'off', account_mode: 'console_user' }),
    )
  })

  it('corrects a stored signed-in user that its Unix agent cannot honour, and says why', async () => {
    // Linux, and an agent that does not say `console-user`.
    const stored: SshPolicy = { ...STORED, account_mode: 'console_user' }
    track(mount(SshPolicyDialog, opened(device({ ssh_policy: stored }))))
    await flushPromises()
    expect(switchOn('Accept SSH sessions on this device')).toBe(true)
    expect(chosen('Sessions run as')).toEqual(['A named Unix account…'])
    expect(alertShown('This device had been set to the signed-in user')).toBe(true)
  })
})

describe('ExternalAccessDialog on its first open (#3156)', () => {
  /** A standing approval: view only, until the end of the year. */
  const STORED: ExternalAccessPolicy = {
    approved: true,
    max_permissions: 'VIEW',
    expires_at: '2026-12-31T16:00:00.000Z',
  }
  const approved = () =>
    device({ external_access_policy: STORED, capabilities: caps('external-access') })

  it('shows the standing approval and its connect code, not a revoked one', async () => {
    store.fetchConnectCode.mockResolvedValue('K7QX-2M9P-4HTD')
    const w = track(mount(ExternalAccessDialog, opened(approved())))
    await flushPromises()
    expect(switchOn('Allow people outside this organization to control this device')).toBe(true)
    expect(chosen('An outside session may at most')).toEqual(['View only — no keyboard or mouse'])
    expect(document.body.querySelector('code.text-h6')?.textContent?.trim()).toBe('K7QX-2M9P-4HTD')
    // It reads what every later open reads, and writes nothing.
    expect(store.fetchExternalAccess).toHaveBeenCalledWith(TID)
    expect(store.fetchConnectCode).toHaveBeenCalledWith(TID, AGENT_ID)
    expect(store.updateExternalAccessPolicy).not.toHaveBeenCalled()
    expect(w.emitted('update:modelValue')).toBeUndefined()
  })

  it('so a Save from it keeps the approval instead of revoking it', async () => {
    track(mount(ExternalAccessDialog, opened(approved())))
    await flushPromises()
    button('Save')!.click()
    await flushPromises()
    expect(store.updateExternalAccessPolicy).toHaveBeenCalledWith(TID, AGENT_ID, STORED)
  })
})

describe("ExternalAccessDialog reads the device's own connect code (#3182, #3197)", () => {
  const CODE = 'K7QX-2M9P-4HTD'
  const approved = () =>
    device({
      external_access_policy: { approved: true, max_permissions: null, expires_at: null },
      capabilities: caps('external-access'),
    })
  const unapproved = () => device({ capabilities: caps('external-access') })

  /** The code row's one button, labelled Rotate or Create. */
  function codeButton(): HTMLButtonElement | undefined {
    return [...document.body.querySelectorAll<HTMLButtonElement>('.v-card-text button')].find(
      (b) => ['Rotate', 'Create'].includes(b.textContent?.trim() ?? ''),
    )
  }
  const shownCode = () => document.body.querySelector('code.text-h6')?.textContent?.trim()
  const textShown = (text: string) => document.body.textContent?.includes(text) ?? false

  it('on a fresh page load, shows the code once its read lands, and offers Rotate', async () => {
    store.fetchConnectCode.mockResolvedValue(CODE)
    track(mount(ExternalAccessDialog, opened(approved())))
    await flushPromises()
    expect(store.fetchConnectCode).toHaveBeenCalledWith(TID, AGENT_ID)
    expect(shownCode()).toBe(CODE)
    expect(codeButton()?.textContent?.trim()).toBe('Rotate')
    expect(store.rotateConnectCode).not.toHaveBeenCalled()
  })

  it('offers no Create while the code is still being read', async () => {
    store.fetchConnectCode.mockImplementation(() => new Promise(() => {}))
    track(mount(ExternalAccessDialog, opened(approved())))
    await flushPromises()
    expect(shownCode()).toBeUndefined()
    expect(textShown('Reading…')).toBe(true)
    expect(textShown('Not yet created')).toBe(false)
    expect(codeButton()?.disabled).toBe(true)
  })

  it('says it could not read the code when the read fails, and still offers no Create', async () => {
    store.fetchConnectCode.mockRejectedValue(new Error('503'))
    track(mount(ExternalAccessDialog, opened(approved())))
    await flushPromises()
    expect(textShown('Could not read the code.')).toBe(true)
    expect(textShown('Not yet created')).toBe(false)
    expect(codeButton()?.disabled).toBe(true)
  })

  it('offers Create only once the read says there is no code, and shows the code it mints', async () => {
    store.fetchConnectCode.mockResolvedValue(null)
    store.rotateConnectCode.mockResolvedValue(CODE)
    track(mount(ExternalAccessDialog, opened(unapproved())))
    await flushPromises()
    expect(textShown('Not yet created')).toBe(true)
    expect(codeButton()?.textContent?.trim()).toBe('Create')
    expect(codeButton()?.disabled).toBe(false)
    codeButton()!.click()
    await flushPromises()
    expect(store.rotateConnectCode).toHaveBeenCalledWith(TID, AGENT_ID)
    expect(shownCode()).toBe(CODE)
    expect(codeButton()?.textContent?.trim()).toBe('Rotate')
  })

  it('shows the code of a device that is not approved, and offers Rotate, not Create', async () => {
    // Approval withdrawn: the code stayed on the row, and the approved list,
    // which is all the dialog used to read, does not hold the device.
    store.orgExternalAccessEnabled = true
    store.externalDevices = []
    store.fetchConnectCode.mockResolvedValue(CODE)
    track(mount(ExternalAccessDialog, opened(unapproved())))
    await flushPromises()
    expect(shownCode()).toBe(CODE)
    expect(textShown('Not yet created')).toBe(false)
    expect(codeButton()?.textContent?.trim()).toBe('Rotate')
  })

  it('still shows a code minted before approval on the next open', async () => {
    store.orgExternalAccessEnabled = true
    store.fetchConnectCode.mockResolvedValueOnce(null)
    store.rotateConnectCode.mockResolvedValue(CODE)
    const w = track(mount(ExternalAccessDialog, opened(unapproved())))
    await flushPromises()
    codeButton()!.click()
    await flushPromises()

    // Closed without approving, then opened again.
    store.fetchConnectCode.mockResolvedValueOnce(CODE)
    await w.setProps({ modelValue: false } as unknown as Parameters<typeof w.setProps>[0])
    await w.setProps({ modelValue: true } as unknown as Parameters<typeof w.setProps>[0])
    await flushPromises()
    expect(shownCode()).toBe(CODE)
    expect(codeButton()?.textContent?.trim()).toBe('Rotate')
  })

  it("does not show one device's code on another when a read lands late", async () => {
    let answerFirst: (code: string) => void = () => {}
    store.fetchConnectCode.mockImplementationOnce(
      () => new Promise<string>((resolve) => (answerFirst = resolve)),
    )
    const w = track(mount(ExternalAccessDialog, opened(unapproved())))
    await flushPromises()

    // The dialog moves to another device, which has none, before the first
    // device's answer lands.
    store.fetchConnectCode.mockResolvedValueOnce(null)
    await w.setProps({ modelValue: false } as unknown as Parameters<typeof w.setProps>[0])
    await w.setProps({
      modelValue: true,
      agent: device({ id: 'c00000000000000000000002', capabilities: caps('external-access') }),
    } as unknown as Parameters<typeof w.setProps>[0])
    await flushPromises()
    answerFirst(CODE)
    await flushPromises()
    expect(shownCode()).toBeUndefined()
    expect(textShown('Not yet created')).toBe(true)
  })

  it('does not put a replaced code back when an old read lands after a mint', async () => {
    const OLD = 'ABCD-EFGH-JKMN'
    let answerFirst: (code: string) => void = () => {}
    store.fetchConnectCode.mockImplementationOnce(
      () => new Promise<string>((resolve) => (answerFirst = resolve)),
    )
    const w = track(mount(ExternalAccessDialog, opened(unapproved())))
    await flushPromises()

    // Closed and opened on the same device with the first read still out, so
    // comparing device ids cannot tell the two reads apart.
    store.fetchConnectCode.mockResolvedValueOnce(OLD)
    await w.setProps({ modelValue: false } as unknown as Parameters<typeof w.setProps>[0])
    await w.setProps({ modelValue: true } as unknown as Parameters<typeof w.setProps>[0])
    await flushPromises()
    expect(shownCode()).toBe(OLD)

    store.rotateConnectCode.mockResolvedValue(CODE)
    codeButton()!.click()
    await flushPromises()
    answerFirst(OLD)
    await flushPromises()
    expect(shownCode()).toBe(CODE)
  })

  it("does not show a code minted for one device on another when the mint lands late", async () => {
    let answerMint: (code: string) => void = () => {}
    store.rotateConnectCode.mockImplementationOnce(
      () => new Promise<string>((resolve) => (answerMint = resolve)),
    )
    const w = track(mount(ExternalAccessDialog, opened(unapproved())))
    await flushPromises()
    codeButton()!.click()
    await flushPromises()

    // The dialog moves to another device, which has none, with the mint out.
    await w.setProps({ modelValue: false } as unknown as Parameters<typeof w.setProps>[0])
    await w.setProps({
      modelValue: true,
      agent: device({ id: 'c00000000000000000000002', capabilities: caps('external-access') }),
    } as unknown as Parameters<typeof w.setProps>[0])
    await flushPromises()
    answerMint(CODE)
    await flushPromises()
    expect(shownCode()).toBeUndefined()
    expect(textShown('Not yet created')).toBe(true)
  })
})

describe('DeviceConsoleDialog on its first open (#3156)', () => {
  it('reads the org exec switch, so a closed org gate is explained before a command is refused', async () => {
    // What the real fetch does for an org with remote execution off.
    store.fetchOrgExecEnabled.mockImplementation(async () => {
      await Promise.resolve()
      store.orgExecEnabled = false
    })
    // A device that would otherwise run the command: policy on, online, able.
    const ready = device({
      exec_policy: {
        mode: 'on',
        can_originate: false,
        allowed_user_ids: [],
        allowed_role_ids: [],
        consent_mode: null,
        shells: [],
      },
      capabilities: caps('exec'),
    })
    const w = track(mount(DeviceConsoleDialog, opened(ready)))
    await flushPromises()
    expect(store.fetchOrgExecEnabled).toHaveBeenCalledWith(TID)
    expect(alertShown('Remote execution is switched off for this organization')).toBe(true)
    expect(store.execOnAgent).not.toHaveBeenCalled()
    expect(w.emitted('update:modelValue')).toBeUndefined()
  })
})

describe('the four, mounted closed (#3156)', () => {
  it.each([
    ['ExecPolicyDialog', () => mount(ExecPolicyDialog, opened(device({}), false))],
    ['SshPolicyDialog', () => mount(SshPolicyDialog, opened(device({}), false))],
    ['ExternalAccessDialog', () => mount(ExternalAccessDialog, opened(device({}), false))],
    ['DeviceConsoleDialog', () => mount(DeviceConsoleDialog, opened(device({}), false))],
  ] as const)('%s asks for nothing and emits nothing until it opens', async (_name, mountClosed) => {
    track(mountClosed())
    await flushPromises()
    for (const [name, fn] of Object.entries(store)) {
      if (vi.isMockFunction(fn)) expect(fn, name).not.toHaveBeenCalled()
    }
    expect(wrapper!.emitted('update:modelValue')).toBeUndefined()
  })
})
