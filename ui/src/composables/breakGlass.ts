// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
// #3258 — where the viewer's ended page offers Force control (admin
// break-glass), as a plain function so the rule can be tested without
// mounting the viewer. The dialog and the request live in RemoteControl.vue.

/**
 * Does the ended page offer Force control (admin break-glass)?
 *
 * - Not when the person at the device ended the session (FR-27 P10): their
 *   Disconnect is an explicit "stop", and the next step offered is Reconnect,
 *   which asks them again (operator, 2026-10-10).
 * - Never on the external viewer (FR-52, #3258). Break-glass is an override
 *   for a tenant administrator, and its reason rides `rc:session.request`.
 *   An outsider's session starts from `rc:extauth.session` and never sends
 *   that frame, so the button could only ever do nothing there.
 */
export function offersBreakGlass(s: { external: boolean; endedBy: 'host' | null }): boolean {
  return !s.external && s.endedBy !== 'host'
}
