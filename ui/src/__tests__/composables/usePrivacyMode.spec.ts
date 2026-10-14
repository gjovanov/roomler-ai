// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * FR-99 P1c — the viewer's side of the `privacy` DataChannel, driven through
 * a fake channel: the protocol code itself, with no PeerConnection.
 */
import { describe, expect, it } from 'vitest'
import {
  describePrivacyReason,
  deviceServesPrivacy,
  grantHoldsPrivacy,
  openPrivacyChannel,
  showsPrivacyRefusal,
  usePrivacyMode,
  withPrivacyAsked,
  type PrivacyChannel,
} from '@/composables/usePrivacyMode'

/** A `privacy` channel whose sent messages the test reads, and whose inbound
 *  messages the test plays. */
function fakeChannel(open = true) {
  const sent: Record<string, unknown>[] = []
  const ch: PrivacyChannel & { deliver(v: unknown): void; close(): void; reopen(): void } = {
    readyState: open ? 'open' : 'connecting',
    onmessage: null,
    onopen: null,
    onclose: null,
    send(data: string) {
      sent.push(JSON.parse(data))
    },
    deliver(v: unknown) {
      this.onmessage?.({ data: typeof v === 'string' ? v : JSON.stringify(v) })
    },
    close() {
      this.readyState = 'closed'
      this.onclose?.()
    },
    reopen() {
      this.readyState = 'open'
      this.onopen?.()
    },
  }
  return { ch, sent }
}

describe('the grant and the device', () => {
  it('reads PRIVACY in a grant by equality only', () => {
    expect(grantHoldsPrivacy('VIEW | INPUT | PRIVACY')).toBe(true)
    expect(grantHoldsPrivacy('VIEW|PRIVACY')).toBe(true)
    expect(grantHoldsPrivacy('VIEW | INPUT')).toBe(false)
    expect(grantHoldsPrivacy('VIEW | PRIVACY_PLUS')).toBe(false)
    expect(grantHoldsPrivacy(undefined)).toBe(false)
    expect(grantHoldsPrivacy(null)).toBe(false)
  })

  it('asks only a device that SERVES privacy mode, never one that merely could', () => {
    expect(deviceServesPrivacy({ rpc: ['exec', 'privacy-available', 'privacy-mode'] })).toBe(true)
    expect(deviceServesPrivacy({ rpc: ['privacy-available'] })).toBe(false)
    expect(deviceServesPrivacy({ rpc: ['privacy-mode-2', 'privacy'] })).toBe(false)
    expect(deviceServesPrivacy({})).toBe(false)
    expect(deviceServesPrivacy(null)).toBe(false)
    const base = 'VIEW | INPUT | CLIPBOARD | FILES | RECORD'
    expect(withPrivacyAsked(base, { rpc: ['privacy-mode'] })).toBe(`${base} | PRIVACY`)
    expect(withPrivacyAsked(base, { rpc: ['privacy-available'] })).toBe(base)
    expect(withPrivacyAsked(base, undefined)).toBe(base)
    // Never asked twice.
    expect(withPrivacyAsked(`${base} | PRIVACY`, { rpc: ['privacy-mode'] })).toBe(`${base} | PRIVACY`)
  })

  it('opens the channel once, and only on a grant that holds PRIVACY', () => {
    const made: string[] = []
    const pc = {
      createDataChannel(label: string) {
        made.push(label)
        return { label } as unknown as RTCDataChannel
      },
    }
    const channels: Record<string, RTCDataChannel> = {}
    expect(openPrivacyChannel(pc, channels, 'VIEW | INPUT')).toBeNull()
    expect(openPrivacyChannel(pc, channels, 'VIEW | PRIVACY')).not.toBeNull()
    expect(openPrivacyChannel(pc, channels, 'VIEW | PRIVACY')).toBeNull()
    expect(openPrivacyChannel(null, {}, 'VIEW | PRIVACY')).toBeNull()
    expect(made).toEqual(['privacy'])
  })

  it('explains a refusal, offers nothing where no curtain could ever be', () => {
    expect(showsPrivacyRefusal('device_not_opted_in')).toBe(true)
    expect(showsPrivacyRefusal('break_glass')).toBe(true)
    expect(showsPrivacyRefusal('something_new')).toBe(true)
    expect(showsPrivacyRefusal('device_cannot_curtain')).toBe(false)
    expect(showsPrivacyRefusal(null)).toBe(false)
    expect(describePrivacyReason('disabled_on_device')).toMatch(/owner/)
    expect(describePrivacyReason('a_code_from_the_future')).toBe('a_code_from_the_future')
    expect(describePrivacyReason(null)).toBe('')
  })
})

describe('the channel', () => {
  it('asks for the status as it opens', () => {
    const p = usePrivacyMode()
    const { ch, sent } = fakeChannel(false)
    p.attach(ch)
    expect(p.ready.value).toBe(false)
    ch.reopen()
    expect(p.ready.value).toBe(true)
    expect(sent[0]?.t).toBe('rc:privacy.status')
  })

  it('follows the device: on, then off', () => {
    const p = usePrivacyMode()
    const { ch, sent } = fakeChannel()
    p.attach(ch)
    p.set(true)
    expect(sent.at(-1)).toMatchObject({ t: 'rc:privacy.set', on: true })
    expect(p.pending.value).toBe(true)
    // Nothing is shown as covered until the DEVICE says so.
    expect(p.on.value).toBe(false)
    ch.deliver({ t: 'rc:privacy.state', id: String(sent.at(-1)?.id), on: true })
    expect(p.on.value).toBe(true)
    expect(p.pending.value).toBe(false)
    p.set(false)
    expect(sent.at(-1)).toMatchObject({ t: 'rc:privacy.set', on: false })
    ch.deliver({ t: 'rc:privacy.state', id: String(sent.at(-1)?.id), on: false })
    expect(p.on.value).toBe(false)
    expect(p.reason.value).toBeNull()
  })

  it('keeps a refusal with its reason', () => {
    const p = usePrivacyMode()
    const { ch } = fakeChannel()
    p.attach(ch)
    p.set(true)
    ch.deliver({ t: 'rc:privacy.state', id: 'x', on: false, reason: 'disabled_on_device' })
    expect(p.on.value).toBe(false)
    expect(p.reason.value).toBe('disabled_on_device')
  })

  it('shows the curtain coming down unasked, with the owner as the reason', () => {
    const p = usePrivacyMode()
    const { ch } = fakeChannel()
    p.attach(ch)
    ch.deliver({ t: 'rc:privacy.state', id: 'a', on: true })
    ch.deliver({ t: 'rc:privacy.state', id: '', on: false, reason: 'disabled_on_device' })
    expect(p.on.value).toBe(false)
    expect(p.reason.value).toBe('disabled_on_device')
  })

  it('reads a closed channel as the session ending the curtain', () => {
    const p = usePrivacyMode()
    const { ch } = fakeChannel()
    p.attach(ch)
    ch.deliver({ t: 'rc:privacy.state', id: 'a', on: true })
    ch.close()
    expect(p.on.value).toBe(false)
    expect(p.ready.value).toBe(false)
    expect(p.reason.value).toBe('session_ended')
  })

  it('a request with no open channel is a refusal, never a hang', () => {
    const p = usePrivacyMode()
    p.set(true)
    expect(p.pending.value).toBe(false)
    expect(p.reason.value).toBe('session_ended')
  })

  it('ignores what is not its own', () => {
    const p = usePrivacyMode()
    const { ch } = fakeChannel()
    p.attach(ch)
    ch.deliver('not json')
    ch.deliver({ t: 'rc:record.state', state: 'recording' })
    expect(p.on.value).toBe(false)
  })
})
