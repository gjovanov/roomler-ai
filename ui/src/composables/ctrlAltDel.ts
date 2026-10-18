// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
// FR-97 P4a — what the Ctrl+Alt+Del button sends, as a plain function so it can
// be tested without a data channel. The sending lives in useRemoteControl.ts.

/** The agent's word for "I send Windows' secure attention sequence" in
 *  `AgentCaps.input`. Matched by equality, never as a prefix: an unknown word
 *  is ignored. */
export const SAS_CAP = 'sas'

/** Does this agent send the real Ctrl+Alt+Del for `{t: 'sas'}`? Only one that
 *  says so: an older agent cannot parse the frame. */
export function agentSendsSas(input: readonly string[] | null | undefined): boolean {
  return Array.isArray(input) && input.includes(SAS_CAP)
}

/** The input frames one press of the button sends. Where the agent sends the
 *  real sequence, one `sas` frame: the Windows lock and sign-in screens answer
 *  only that. Elsewhere the three keys, downs in the canonical order
 *  (Ctrl, Alt, Del) and releases in reverse, as a person presses them.
 *  HID usages: LeftCtrl 0xe0, LeftAlt 0xe2, Delete 0x4c; mods: ctrl 1, alt 4. */
export function ctrlAltDelFrames(input: readonly string[] | null | undefined): Record<string, unknown>[] {
  if (agentSendsSas(input)) return [{ t: 'sas' }]
  return [
    { t: 'key', code: 0xe0, down: true, mods: 1 },
    { t: 'key', code: 0xe2, down: true, mods: 1 | 4 },
    { t: 'key', code: 0x4c, down: true, mods: 1 | 4 },
    { t: 'key', code: 0x4c, down: false, mods: 1 | 4 },
    { t: 'key', code: 0xe2, down: false, mods: 1 },
    { t: 'key', code: 0xe0, down: false, mods: 0 },
  ]
}
