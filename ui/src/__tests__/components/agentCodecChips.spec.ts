// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
import { describe, it, expect } from 'vitest'
import { codecChips, permissionWarnings } from '@/components/admin/agentCodecChips'
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
      consent_mode: null,
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

describe('permissionWarnings', () => {
  const caps = (permissions?: string[]): Agent['capabilities'] => ({
    codecs: [],
    hw_encoders: [],
    has_input_permission: true,
    supports_clipboard: false,
    supports_file_transfer: false,
    max_simultaneous_sessions: 1,
    permissions,
  })

  // The distinction this whole field exists for. A pre-rc.454 agent cannot
  // report, so it must produce NO warnings; an agent that reports an empty
  // list is saying it holds neither permission, which is the loudest case
  // there is. A falsy check would treat them identically and silence it.
  it('says nothing about an agent too old to report', () => {
    expect(permissionWarnings(makeAgent(caps(undefined)))).toEqual([])
  })

  it('warns about BOTH when the agent reports an empty list', () => {
    const w = permissionWarnings(makeAgent(caps([])))
    expect(w.map((x) => x.label)).toEqual(['No screen access', 'No input access'])
  })

  it('warns only about what is actually missing', () => {
    expect(permissionWarnings(makeAgent(caps(['screen-capture']))).map((w) => w.label)).toEqual([
      'No input access',
    ])
    expect(permissionWarnings(makeAgent(caps(['input']))).map((w) => w.label)).toEqual([
      'No screen access',
    ])
  })

  it('is silent when both are granted', () => {
    expect(permissionWarnings(makeAgent(caps(['screen-capture', 'input'])))).toEqual([])
  })

  it('says nothing when the agent has no capabilities at all', () => {
    expect(permissionWarnings(makeAgent(undefined))).toEqual([])
  })

  // The THIRD state. macOS's root LaunchDaemon has no GUI session, so capture
  // and input are impossible there regardless of grants — it is not a device
  // with missing permissions, it is not a capture target. Warning about it
  // sends the operator after a toggle that would change nothing, which is
  // exactly what the device list did on a real two-half Mac.
  it('says nothing about a device that has no GUI session', () => {
    expect(permissionWarnings(makeAgent(caps(['no-gui-session'])))).toEqual([])
  })
})

describe('FR-77 — codecChips reads video_cells when the agent sends them', () => {
  it('marks a codec HW when any cell is verified hardware, and shows a 4:4:4 cell', () => {
    const agent = {
      id: 'a',
      name: 'x',
      capabilities: {
        codecs: ['h264', 'h265', 'av1'],
        hw_encoders: ['openh264-sw', 'ffmpeg-hevc_nvenc', 'ffmpeg-av1_nvenc', 'libvpx-vp9-444-sw'],
        has_input_permission: true,
        supports_clipboard: true,
        supports_file_transfer: true,
        max_simultaneous_sessions: 2,
        video_cells: [
          { codec: 'h264', backend: 'openh264', chroma: ['yuv420'], hw: false },
          { codec: 'h264', backend: 'nvenc', chroma: ['yuv420', 'yuv444'], hw: true },
          { codec: 'hevc', backend: 'nvenc', chroma: ['yuv420', 'yuv444'], hw: true },
          { codec: 'av1', backend: 'nvenc', chroma: ['yuv420'], hw: true },
          { codec: 'vp9', backend: 'libvpx', chroma: ['yuv420', 'yuv444'], hw: false },
        ],
      },
    } as unknown as Parameters<typeof codecChips>[0]
    const labels = codecChips(agent).map((c) => c.label)
    expect(labels).toEqual(['H.264 HW 4:4:4', 'H.265 HW 4:4:4', 'AV1 HW', 'VP9 SW 4:4:4'])
    const hevc = codecChips(agent).find((c) => c.label.startsWith('H.265'))
    expect(hevc?.color).toBe('primary')
    expect(hevc?.tooltip).toContain('nvenc')
  })

  it('an ffmpeg-only HEVC host used to chip as software; the cells fix that', () => {
    const legacy = {
      id: 'a',
      name: 'x',
      capabilities: {
        codecs: ['h265'],
        hw_encoders: ['ffmpeg-hevc_qsv'],
        has_input_permission: true,
        supports_clipboard: true,
        supports_file_transfer: true,
        max_simultaneous_sessions: 2,
      },
    } as unknown as Parameters<typeof codecChips>[0]
    expect(codecChips(legacy).map((c) => c.label)).toEqual(['H.265 SW'])
    const withCells = {
      ...legacy,
      capabilities: {
        ...legacy.capabilities,
        video_cells: [{ codec: 'hevc', backend: 'qsv', chroma: ['yuv420'], hw: true }],
      },
    } as unknown as Parameters<typeof codecChips>[0]
    expect(codecChips(withCells).map((c) => c.label)).toEqual(['H.265 HW'])
  })
})
