// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
import { describe, it, expect } from 'vitest'
import {
  LEGACY_MAC_PERMISSION_ADVICE,
  LEGACY_MAC_PERMISSION_HINT_START,
  mediaUnavailableHint,
} from '@/composables/macPermissionHint'

// What agents up to 0.4.188 send, byte for byte (`capture/mod.rs` from FR-80).
const LEGACY =
  "Grant Screen Recording to Roomler in System Settings → Privacy & Security, then reconnect. " +
  "A macOS update or a change to the app's code signature invalidates an existing grant."

// What an agent with #3164 sends (`MAC_PERMISSION_HINT`).
const CURRENT =
  'Turn on roomlerd in System Settings → Privacy & Security → Screen & System Audio Recording. ' +
  'macOS applies that grant only when roomlerd restarts, so reconnecting alone does not help: ' +
  'on the Mac, choose Quit & Reopen when macOS offers it, or Restart in the Roomler menu-bar app, ' +
  "then reconnect. A macOS update or a change to the app's code signature invalidates an existing grant."

describe('#3164 — a Mac without Screen Recording is told about the restart', () => {
  it("replaces the older agents' sentence, which said to reconnect", () => {
    expect(LEGACY.startsWith(LEGACY_MAC_PERMISSION_HINT_START)).toBe(true)
    const shown = mediaUnavailableHint('permission', LEGACY)
    expect(shown).toBe(LEGACY_MAC_PERMISSION_ADVICE)
    expect(shown).toContain('only when roomlerd restarts')
    // The operator, 2026-10-30: nothing a person reads calls the daemon an "agent".
    // `com.roomler.agent` is the launchd label the kickstart names, not a word.
    expect(shown).not.toMatch(/(?<!\.)\bagent\b/i)
    expect(shown).toContain('reconnecting alone does not help')
    expect(shown).toContain('Turn on roomlerd')
    expect(shown).not.toContain('Grant Screen Recording to Roomler')
  })

  it('never sends an older agent to Quit & Reopen, which takes it offline', () => {
    const shown = mediaUnavailableHint('permission', LEGACY)
    expect(shown).toContain('launchctl kickstart -k gui/$(id -u)/com.roomler.agent')
    expect(shown).toContain('Quit & Reopen takes the device offline')
  })

  it("shows a newer agent's own sentence as written", () => {
    expect(mediaUnavailableHint('permission', CURRENT)).toBe(CURRENT)
    expect(CURRENT).toContain('Quit & Reopen')
  })

  it('leaves every other reason and host alone', () => {
    const linux = 'The host denied this agent permission to capture the screen. Grant it on the host, then reconnect.'
    expect(mediaUnavailableHint('permission', linux)).toBe(linux)
    expect(mediaUnavailableHint('backend_error', LEGACY)).toBe(LEGACY)
    expect(mediaUnavailableHint('no_display', '')).toBe('')
    expect(mediaUnavailableHint('permission', undefined)).toBe('')
  })
})
