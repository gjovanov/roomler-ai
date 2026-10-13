// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD

/**
 * FR-93 — a device's own words, shown as a sentence after one of the viewer's.
 *
 * An agent writes its reasons to follow a dash in its own log ("input reaches
 * none of this desktop — this is a Wayland desktop …"), so they start in lower
 * case and often end with no full stop. Placed after the viewer's own sentence
 * they read as a broken one. Every agent already in the field sends them this
 * way, so the viewer fixes it as it shows them, and the wire stays the agent's
 * words.
 */
export function asSentence(text: string): string {
  const t = text.trim()
  if (!t) return ''
  const capitalised = t.charAt(0).toUpperCase() + t.slice(1)
  return /[.!?…]$/.test(capitalised) ? capitalised : `${capitalised}.`
}
