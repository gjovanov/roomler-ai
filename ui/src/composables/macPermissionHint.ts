// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
// #3164 — the sentence under "This device cannot capture its screen.", for a
// Mac whose agent lacks Screen Recording.
//
// The sentence is the agent's: only it knows its OS (`rc:media-unavailable`,
// `capture/mod.rs`). But every agent up to 0.4.188 sends one that is wrong in
// two ways. It names an entry the System Settings pane does not show
// ("Roomler"; the pane lists the agent as `roomlerd`, and "Roomler" there is
// the menu-bar app), and it ends "then reconnect": macOS applies a Screen
// Recording grant only to a freshly started process, so a reconnect reaches
// the same agent and the same `permission`. On those agents macOS's own
// "Quit & Reopen" is no way out either: it ends the agent with exit 0, which
// launchd does not relaunch, and the device goes offline. So that exact
// sentence is replaced with advice that is true for them. A newer agent's
// sentence (Quit & Reopen restarts it there) is shown as written.

/** How every agent up to 0.4.188 begins its macOS `permission` sentence. It
 *  never changed after FR-80 introduced it, so the beginning identifies it. */
export const LEGACY_MAC_PERMISSION_HINT_START = 'Grant Screen Recording to Roomler in System Settings'

/** What an agent that sent the legacy sentence needs instead. Not "Quit &
 *  Reopen": on those agents that takes the device offline. */
export const LEGACY_MAC_PERMISSION_ADVICE =
  'Turn on roomlerd in System Settings → Privacy & Security → Screen & System Audio Recording ' +
  '(the Roomler entry there is the menu-bar app). macOS applies that grant only when the agent ' +
  'restarts, so reconnecting alone does not help. On this agent version macOS’s own Quit & ' +
  'Reopen takes the device offline: on the Mac, restart it with launchctl kickstart -k ' +
  'gui/$(id -u)/com.roomler.agent in Terminal, then reconnect.'

/** The hint to show for an `rc:media-unavailable` reason: the agent's own,
 *  except the legacy macOS `permission` sentence, which is corrected. */
export function mediaUnavailableHint(code: string, hint: string | null | undefined): string {
  const text = hint ?? ''
  if (code === 'permission' && text.startsWith(LEGACY_MAC_PERMISSION_HINT_START)) {
    return LEGACY_MAC_PERMISSION_ADVICE
  }
  return text
}
