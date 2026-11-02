// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * FR-129 C — the setup wizard's page: what a person sees on Welcome and Server
 * for each thing Rust can answer.
 *
 * Driven by the REAL `index.html` and `app.js` of `agents/roomler-setup`, in
 * jsdom, against a mocked Tauri `invoke`: the same approach as the companion's
 * specs. No other lane runs this file, and its gates (what is preselected, when
 * Continue opens, which cards show) are the product here: a wizard that passes
 * every Rust test can still lead a person through pairing into a dead end.
 */
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { readFileSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'

const HERE = dirname(fileURLToPath(import.meta.url))
const FRONT = join(HERE, '..', '..', '..', 'agents', 'roomler-setup', 'src', 'front')
const PAGE = readFileSync(join(FRONT, 'index.html'), 'utf8')
const SCRIPT = readFileSync(join(FRONT, 'app.js'), 'utf8')

type Dict = Record<string, unknown>
const WINDOWS_ROLES = ['daemon-system', 'daemon-machine', 'daemon-user']

const agent = (over: Dict = {}): Dict => ({
  supported: true,
  kind: 'clean',
  peruserVersion: null,
  permachineVersion: null,
  ambiguous: false,
  os: 'windows',
  daemonRoles: WINDOWS_ROLES,
  featuresServer: null,
  daemonOffered: false,
  featuresAnswered: true,
  roleLimit: 'none',
  tunnelOffered: true,
  defaultRole: 'daemon-system',
  ...over,
})
const unix = (os: string, over: Dict = {}): Dict =>
  agent({
    os,
    daemonRoles: os === 'linux' ? ['daemon-system', 'daemon-user'] : ['daemon-user'],
    featuresServer: 'https://roomler.ai',
    daemonOffered: true,
    defaultRole: os === 'linux' ? 'daemon-system' : 'daemon-user',
    ...over,
  })
const detection = (a: Dict) => ({
  agent: a,
  tunnel: { installed: false, machineName: null, configPath: null },
})

interface Run {
  agent?: Dict
  persisted?: Dict
  launch?: Dict
  needsElevation?: boolean
  relaunch?: Dict
  features?: Dict
}

let calls: Array<[string, Dict | undefined]> = []
let nextDetection: Dict | null = null
let nextFeatures: Dict | null = null

const flush = () => vi.advanceTimersByTimeAsync(0)

async function open(run: Run = {}) {
  calls = []
  nextDetection = null
  nextFeatures = run.features ?? null
  document.documentElement.innerHTML = PAGE.replace(/<!doctype[^>]*>/i, '')
  const answers: Record<string, () => unknown> = {
    cmd_load_state: () => run.persisted ?? {},
    cmd_launch_options: () => run.launch ?? { role: null, server: null, relaunched: false },
    cmd_default_server_url: () => 'https://roomler.ai',
    cmd_default_device_name: () => 'this-pc',
    cmd_detect_install: () => detection(nextDetection ?? run.agent ?? agent()),
    cmd_system_context_needs_elevation: () => run.needsElevation === true,
    cmd_relaunch_elevated: () => run.relaunch ?? { kind: 'cancelled' },
    cmd_setup_features: () => nextFeatures ?? { pairing: true, pairingOff: false, daemonOffered: true, answered: true },
  }
  const invoke = (name: string, payload?: Dict) => {
    calls.push([name, payload])
    return Promise.resolve(answers[name]?.())
  }
  ;(window as unknown as { __TAURI__: unknown }).__TAURI__ = {
    core: { invoke, Channel: class {} },
    event: { listen: () => Promise.resolve(() => {}) },
  }
  new Function(`"use strict";\n${SCRIPT}`)()
  await flush()
  await flush()
}

const el = <T extends HTMLElement = HTMLElement>(id: string) => document.getElementById(id) as T
const picked = () =>
  (document.querySelector("input[name='role']:checked") as HTMLInputElement | null)?.value ?? null
const canContinue = () => !el<HTMLButtonElement>('welcome-continue').disabled
const card = (role: string) =>
  document.querySelector(`label.role-daemon[data-role='${role}']`) as HTMLElement
const shownDaemonCards = () =>
  WINDOWS_ROLES.filter((role) => !card(role).hidden)
const countOf = (name: string) => calls.filter(([c]) => c === name).length
const pick = async (role: string) => {
  const radio = document.querySelector(`input[value='${role}']`) as HTMLInputElement
  radio.checked = true
  radio.dispatchEvent(new Event('change', { bubbles: true }))
  await flush()
}
const click = async (id: string) => {
  el(id).click()
  await flush()
  await flush()
}

beforeEach(() => {
  vi.useFakeTimers()
})
afterEach(() => {
  vi.clearAllTimers()
  vi.useRealTimers()
})

describe('what Welcome preselects', () => {
  it('a clean machine: the background service, Continue open, nothing saved before a click', async () => {
    await open()
    expect(picked()).toBe('daemon-system')
    expect(canContinue()).toBe(true)
    expect(countOf('cmd_save_state')).toBe(0)
    await click('welcome-continue')
    expect(el<HTMLInputElement>('adv-overlay').checked).toBe(true)
  })

  it("the operator's red: a finished tunnel run does not preselect the tunnel or keep its name", async () => {
    await open({ persisted: { step: 'done', role: 'tunnel-client', serverUrl: 'https://x.example', deviceName: 'old' } })
    expect(picked()).toBe('daemon-system')
    await click('welcome-continue')
    expect(el<HTMLInputElement>('device-name').value).toBe('this-pc')
    expect(el<HTMLInputElement>('server-url').value).toBe('https://x.example')
  })

  it('an existing install keeps its flavour', async () => {
    await open({ agent: agent({ kind: 'peruser', defaultRole: 'daemon-user' }) })
    expect(picked()).toBe('daemon-user')
    expect(el('cross-flavour-warning').hidden).toBe(true)
    await open({ agent: agent({ kind: 'permachine', defaultRole: 'daemon-machine' }) })
    expect(picked()).toBe('daemon-machine')
    expect(canContinue()).toBe(true)
  })

  it('a machine with only the tunnel client keeps it', async () => {
    await open({ agent: agent({ defaultRole: 'tunnel-client' }) })
    expect(picked()).toBe('tunnel-client')
    expect(canContinue()).toBe(true)
  })

  it('no default: nothing picked, Continue closed', async () => {
    await open({ agent: agent({ kind: 'permachine', defaultRole: null }) })
    expect(picked()).toBe(null)
    expect(canContinue()).toBe(false)
  })

  it('a role Rust names that this page does not show is not picked', async () => {
    await open({ agent: agent({ daemonRoles: ['daemon-user'], defaultRole: 'daemon-system' }) })
    expect(picked()).toBe(null)
  })

  it('a run stopped on Token resumes there with its role; any other run starts over', async () => {
    await open({ persisted: { step: 'token', role: 'daemon-user', serverUrl: 'https://x.example', deviceName: 'kept' } })
    expect(el('step-token').hidden).toBe(false)
    await click('token-back')
    expect(el<HTMLInputElement>('device-name').value).toBe('kept')
    await open({ persisted: { step: 'install', role: 'daemon-machine', serverUrl: 'https://x.example', deviceName: 'kept' } })
    expect(el('step-welcome').hidden).toBe(false)
    expect(picked()).toBe('daemon-system')
  })

  it('both flavours installed: the warning holds Continue until it is acknowledged', async () => {
    await open({ agent: agent({ kind: 'ambiguous', ambiguous: true, peruserVersion: '1', permachineVersion: '1', defaultRole: 'daemon-system' }) })
    expect(picked()).toBe('daemon-system')
    expect(el('cross-flavour-warning').hidden).toBe(false)
    expect(canContinue()).toBe(false)
    const ack = el<HTMLInputElement>('cross-flavour-ack')
    ack.checked = true
    ack.dispatchEvent(new Event('change', { bubbles: true }))
    expect(canContinue()).toBe(true)
  })

  it('Linux and macOS: the card copy per OS, and the default', async () => {
    await open({ agent: unix('linux') })
    expect(picked()).toBe('daemon-system')
    expect(shownDaemonCards()).toEqual(['daemon-system', 'daemon-user'])
    expect(el('role-title-daemon-system').textContent).toContain('recommended')
    await open({ agent: unix('macos') })
    expect(picked()).toBe('daemon-user')
    expect(el('role-title-daemon-user').textContent).toContain('recommended')
  })
})

describe('Restart as administrator', () => {
  const needs = { needsElevation: true }

  it('shows with #3116, holds Continue, and asks Rust with the role and the server', async () => {
    await open(needs)
    expect(el('elevation-note').hidden).toBe(false)
    expect(canContinue()).toBe(false)
    await click('elevation-relaunch')
    const asked = calls.find(([c]) => c === 'cmd_relaunch_elevated')
    expect(asked?.[1]).toEqual({ role: 'daemon-system', server: 'https://roomler.ai' })
  })

  it('a declined prompt stays on Welcome and says so; another role clears it', async () => {
    await open(needs)
    await click('elevation-relaunch')
    expect(el('step-welcome').hidden).toBe(false)
    expect(el('elevation-relaunch-status').textContent).toContain('cancelled')
    await pick('daemon-user')
    expect(el('elevation-note').hidden).toBe(true)
    expect(canContinue()).toBe(true)
    await pick('daemon-system')
    expect(el('elevation-relaunch-status').hidden).toBe(true)
  })

  it('a failed start says why', async () => {
    await open({ ...needs, relaunch: { kind: 'failed', message: 'it could not' } })
    await click('elevation-relaunch')
    expect(el('elevation-relaunch-status').textContent).toContain('it could not')
  })

  it('the elevated copy opens on the role it was handed, over the default', async () => {
    await open({
      launch: { role: 'daemon-machine', server: 'https://x.example', relaunched: true },
      agent: agent(),
    })
    expect(picked()).toBe('daemon-machine')
    expect(el('elevation-note').hidden).toBe(true)
    expect(canContinue()).toBe(true)
    await click('welcome-continue')
    expect(el<HTMLInputElement>('server-url').value).toBe('https://x.example')
  })
})

describe('what a run narrows the cards to (review findings 3 and 6)', () => {
  const another = agent({ roleLimit: 'another-account', daemonRoles: ['daemon-system'], tunnelOffered: false })
  const locked = agent({ roleLimit: 'no-elevation', daemonRoles: ['daemon-user'], defaultRole: 'daemon-user' })

  it("another account's password: only the machine-wide service, and why", async () => {
    await open({ agent: another, launch: { role: 'daemon-system', server: null, relaunched: true } })
    expect(shownDaemonCards()).toEqual(['daemon-system'])
    expect(el('role-card-tunnel').hidden).toBe(true)
    expect(picked()).toBe('daemon-system')
    expect(canContinue()).toBe(true)
    expect(el('role-limit-note').hidden).toBe(false)
    expect(el('role-limit-note').textContent).toContain('another account')
  })

  it('another account: a role handed in or left over is not kept', async () => {
    await open({ agent: another, launch: { role: 'tunnel-client', server: null, relaunched: true } })
    expect(picked()).toBe('daemon-system')
    await open({ agent: another, launch: { role: 'daemon-user', server: null, relaunched: true } })
    expect(picked()).toBe('daemon-system')
  })

  it('a locked desktop: what needs no prompt, preselected, and why', async () => {
    await open({ agent: locked })
    expect(shownDaemonCards()).toEqual(['daemon-user'])
    expect(el('role-card-tunnel').hidden).toBe(false)
    expect(picked()).toBe('daemon-user')
    expect(canContinue()).toBe(true)
    expect(el('role-limit-note').textContent).toContain('policy')
    expect(el('elevation-note').hidden).toBe(true)
  })

  it('no limit: no note', async () => {
    await open()
    expect(el('role-limit-note').hidden).toBe(true)
    expect(el('role-card-tunnel').hidden).toBe(false)
  })
})

describe('"could not ask" is not "switched off" (review finding 7)', () => {
  const unanswered = unix('linux', {
    supported: false,
    daemonOffered: false,
    featuresAnswered: false,
    defaultRole: null,
  })

  it('preselects nothing, says so, and keeps asking until the daemon cards can appear', async () => {
    await open({ agent: unanswered })
    expect(picked()).toBe(null)
    expect(canContinue()).toBe(false)
    expect(shownDaemonCards()).toEqual([])
    expect(el('role-card-tunnel').hidden).toBe(false)
    expect(el('platform-note').textContent).toContain('Could not reach')
    expect(countOf('cmd_detect_install')).toBe(1)
    await vi.advanceTimersByTimeAsync(10_000)
    expect(countOf('cmd_detect_install')).toBe(2)
    await vi.advanceTimersByTimeAsync(10_000)
    expect(countOf('cmd_detect_install')).toBe(3)
    nextDetection = unix('linux')
    await vi.advanceTimersByTimeAsync(10_000)
    expect(picked()).toBe('daemon-system')
    expect(shownDaemonCards()).toEqual(['daemon-system', 'daemon-user'])
    expect(el('platform-note').hidden).toBe(true)
    const asked = countOf('cmd_detect_install')
    await vi.advanceTimersByTimeAsync(60_000)
    expect(countOf('cmd_detect_install')).toBe(asked)
  })

  it("a person's own click is kept when the answer comes", async () => {
    await open({ agent: unanswered })
    await pick('tunnel-client')
    expect(canContinue()).toBe(true)
    nextDetection = unix('linux')
    await vi.advanceTimersByTimeAsync(10_000)
    expect(picked()).toBe('tunnel-client')
  })

  it("the server's own off is an answer: the tunnel is the default and nothing asks again", async () => {
    await open({
      agent: unix('linux', { supported: false, daemonOffered: false, defaultRole: 'tunnel-client' }),
    })
    expect(picked()).toBe('tunnel-client')
    expect(el('platform-note').textContent).toContain('does not offer')
    await vi.advanceTimersByTimeAsync(60_000)
    expect(countOf('cmd_detect_install')).toBe(1)
  })

  it('Windows asks no server, and never asks again', async () => {
    await open()
    await vi.advanceTimersByTimeAsync(60_000)
    expect(countOf('cmd_detect_install')).toBe(1)
  })

  it('on the Server page too: holds Continue, says so, and opens when it is answered', async () => {
    await open({ agent: unix('linux') })
    await click('welcome-continue')
    nextFeatures = { pairing: false, pairingOff: false, daemonOffered: false, answered: false }
    const url = el<HTMLInputElement>('server-url')
    url.value = 'https://other.example'
    url.dispatchEvent(new Event('input', { bubbles: true }))
    url.dispatchEvent(new Event('change', { bubbles: true }))
    await vi.advanceTimersByTimeAsync(2_000)
    expect(el<HTMLButtonElement>('server-continue').disabled).toBe(true)
    expect(el('server-features-note').textContent).toContain('Could not reach')
    nextFeatures = { pairing: true, pairingOff: false, daemonOffered: true, answered: true }
    await vi.advanceTimersByTimeAsync(10_000)
    expect(el('server-features-note').hidden).toBe(true)
    expect(el<HTMLButtonElement>('server-continue').disabled).toBe(false)
  })

  it("on the Server page: the server's own off keeps its own words", async () => {
    await open({ agent: unix('linux') })
    await click('welcome-continue')
    nextFeatures = { pairing: true, pairingOff: false, daemonOffered: false, answered: true }
    const url = el<HTMLInputElement>('server-url')
    url.value = 'https://other.example'
    url.dispatchEvent(new Event('input', { bubbles: true }))
    url.dispatchEvent(new Event('change', { bubbles: true }))
    await vi.advanceTimersByTimeAsync(2_000)
    expect(el('server-features-note').textContent).toContain('does not offer')
  })
})
