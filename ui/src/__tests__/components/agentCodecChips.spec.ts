import { describe, it, expect } from 'vitest'
import { codecChips } from '@/components/admin/agentCodecChips'
import type { Agent } from '@/stores/agents'

function makeAgent(caps?: Agent['capabilities']): Agent {
  return {
    id: 'a1',
    tenant_id: 't1',
    owner_user_id: 'u1',
    name: 'Test',
    machine_id: 'm1',
    os: 'windows',
    agent_version: '0.1.27',
    status: 'online',
    is_online: true,
    last_seen_at: '2026-04-20T00:00:00Z',
    access_policy: {
      require_consent: false,
      allowed_role_ids: [],
      allowed_user_ids: [],
      auto_terminate_idle_minutes: null,
    },
    capabilities: caps,
  }
}

describe('codecChips', () => {
  it('returns empty when capabilities are absent', () => {
    expect(codecChips(makeAgent(undefined))).toEqual([])
  })

  it('marks codec HW when a hw_encoders entry contains -hw and the codec stem', () => {
    const chips = codecChips(
      makeAgent({
        codecs: ['h264'],
        hw_encoders: ['mf-h264-hw'],
        has_input_permission: true,
        supports_clipboard: false,
        supports_file_transfer: false,
        max_simultaneous_sessions: 1,
      }),
    )
    expect(chips).toHaveLength(1)
    expect(chips[0].label).toBe('H.264 HW')
    expect(chips[0].color).toBe('primary')
  })

  it('marks codec SW when only -sw backend is present', () => {
    const chips = codecChips(
      makeAgent({
        codecs: ['h264'],
        hw_encoders: ['openh264-sw'],
        has_input_permission: true,
        supports_clipboard: false,
        supports_file_transfer: false,
        max_simultaneous_sessions: 1,
      }),
    )
    expect(chips[0].label).toBe('H.264 SW')
    expect(chips[0].color).toBe('default')
  })

  it('renders multiple codecs each with the right HW/SW marker', () => {
    const chips = codecChips(
      makeAgent({
        codecs: ['h264', 'h265', 'av1'],
        hw_encoders: ['openh264-sw', 'mf-h264-hw', 'mf-h265-hw'],
        has_input_permission: true,
        supports_clipboard: false,
        supports_file_transfer: false,
        max_simultaneous_sessions: 1,
      }),
    )
    expect(chips.map((c) => c.label)).toEqual(['H.264 HW', 'H.265 HW', 'AV1 SW'])
  })

  it('treats h264 as HW if any backend with that stem is HW', () => {
    // openh264-sw + mf-h264-hw both present → should be HW.
    const chips = codecChips(
      makeAgent({
        codecs: ['h264'],
        hw_encoders: ['openh264-sw', 'mf-h264-hw'],
        has_input_permission: true,
        supports_clipboard: false,
        supports_file_transfer: false,
        max_simultaneous_sessions: 1,
      }),
    )
    expect(chips[0].label).toBe('H.264 HW')
  })

  it('returns empty for capabilities with no codecs', () => {
    const chips = codecChips(
      makeAgent({
        codecs: [],
        hw_encoders: [],
        has_input_permission: false,
        supports_clipboard: false,
        supports_file_transfer: false,
        max_simultaneous_sessions: 1,
      }),
    )
    expect(chips).toEqual([])
  })
})
