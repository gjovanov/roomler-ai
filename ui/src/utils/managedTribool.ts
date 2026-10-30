// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * FR-130 P1b — a TRIBOOL device key under remote configuration
 * (`color_description`), as the device dialog shows it.
 *
 * The device's key has three values: unset (its built-in default), off and
 * on. A request adds a fourth, not managed at all. On the wire those are an
 * absent key, `''` (clear the device's key), `'off'` and `'on'`; in the
 * dialog's select they are no selection, "default", "off" and "on".
 *
 * ⚠️ Not managed is NOT a revert. A device keeps what it was pushed last, so
 * undoing an `off` means asking for the default (`''`), and only then
 * leaving the key alone.
 */
import type { PushedTribool } from '@/stores/agents'

/** What the select shows; `null` is no selection: not managed. */
export type TriboolChoice = 'default' | 'off' | 'on'

/** The select's choice for a requested word; `null` when not managed. */
export function triboolChoice(word: PushedTribool | undefined): TriboolChoice | null {
  if (word === undefined) return null
  return word === '' ? 'default' : word
}

/** The word a choice requests. A cleared select (`null`), or anything the
 *  select does not offer, is NOT MANAGED, never a value: reading a stray
 *  value as `''` would clear the device's key, and as `'off'` switch it off. */
export function triboolWord(choice: unknown): PushedTribool | undefined {
  switch (choice) {
    case 'default':
      return ''
    case 'off':
      return 'off'
    case 'on':
      return 'on'
    default:
      return undefined
  }
}
