// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * FR-67 P5 / #3332 — the two host-health words a device's caps carry, as the
 * devices page shows them beside presence, never instead of it.
 *
 * - `last_install`: the last install's verdict, as the device's post-install
 *   watcher recorded it (`docs/update-outcome.md` §8). A marker only for what
 *   needs a look; a verified install says so in the version's sentence.
 * - `crash_health`: the crash ladder's state on the host: crashes inside the
 *   current window, and whether it rolled back or could not. Host-wide, so
 *   every org's row of a multi-org device shows the same.
 *
 * Three states stay apart: no word (an agent older than it, or one that could
 * not read its record) shows nothing new; nothing to report (`Absent`, `ok`)
 * shows no marker; something to report shows one. A word this page does not
 * know is ignored, as an unrecognised `update_mode` is: a newer agent's word
 * must never put a marker on a healthy device.
 */

/** `AgentCaps.last_install` (crates/remote_control/src/models.rs). */
export interface LastInstallWord {
  status: string
  /** `InProgress` older than any watcher's life: the install never reported. */
  never_reported?: boolean
  /** The release it was for, as the record names it (`agent-v0.4.240`). */
  expected_version?: string
  /** What the new binary answered (`0.4.240`). */
  new_binary_version?: string
  started_unix?: number
  finished_unix?: number
}

/** `AgentCaps.crash_health` (crates/remote_control/src/models.rs). */
export interface CrashHealth {
  /** `ok` · `crashing` · `rolled_back` · `rollback_failed`. */
  state: string
  /** Crashes inside the current window. */
  crashes: number
  window_secs: number
  last_crash_unix?: number
  /** With `rollback_failed`: the version the ladder accuses, the one running. */
  accused_version?: string
  /** What the ladder rolls back to. */
  last_known_good?: string
}

export interface HealthChip {
  text: string
  color: 'info' | 'warning' | 'error'
  tooltip: string
}

/** A version as a person reads it: a tag's `agent-v` or `v` taken off (FR-133). */
function shown(version: string | undefined): string {
  return (version ?? '').trim().replace(/^agent-v/, '').replace(/^v(?=\d)/, '')
}

/** " 12 min ago", " 3 h ago", " 2 d ago", or '' with no time. */
function ago(unix: number | undefined, now: number): string {
  if (!unix) return ''
  const secs = Math.max(0, Math.round(now / 1000 - unix))
  if (secs < 90) return ' just now'
  const min = Math.round(secs / 60)
  if (min < 90) return ` ${min} min ago`
  const h = Math.round(min / 60)
  if (h < 36) return ` ${h} h ago`
  return ` ${Math.round(h / 24)} d ago`
}

/** ", 12 min ago" after a clause, or '' with no time. */
function then(unix: number | undefined, now: number): string {
  const a = ago(unix, now)
  return a ? `,${a}` : ''
}

/** "to 0.4.240 " for the release an install was for, or '' with none. */
function target(w: LastInstallWord): string {
  const v = shown(w.expected_version)
  return v ? `to ${v} ` : ''
}

const INSTALL_CHIPS: Record<string, { text: string; color: HealthChip['color'] }> = {
  SucceededUnverified: { text: 'update unverified', color: 'warning' },
  InstallerFailed: { text: 'update failed', color: 'error' },
  Timeout: { text: 'update timed out', color: 'error' },
  WatcherStopped: { text: 'update not confirmed', color: 'warning' },
  Unreadable: { text: 'update record unreadable', color: 'warning' },
}

/** The sentence for the last install, or '' when the device says nothing this
 *  page knows. Shown in the version's title and the marker's meaning. */
export function installSentence(
  w: LastInstallWord | null | undefined,
  now: number = Date.now(),
): string {
  if (!w) return ''
  const to = target(w)
  const finished = then(w.finished_unix, now)
  switch (w.status) {
    case 'Absent':
      return 'Last update: none recorded on this device.'
    case 'InProgress':
      return w.never_reported
        ? `Last update ${to}NEVER REPORTED: it started${ago(w.started_unix, now)} and no verdict ` +
            'came. The watcher that records it was stopped first.'
        : `Last update ${to}in progress: it started${ago(w.started_unix, now)}.`
    case 'SucceededVerified':
      return `Last update ${to}verified: the new binary answered ${shown(w.new_binary_version) || shown(w.expected_version)}${finished}.`
    case 'SucceededUnverified':
      return (
        `Last update ${to}unverified: the installer succeeded${finished}, and the new binary ` +
        (w.new_binary_version
          ? `answered ${shown(w.new_binary_version)}.`
          : 'did not answer its version.')
      )
    case 'InstallerFailed':
      return `Last update ${to}failed: the installer exited with an error${finished}.`
    case 'Timeout':
      return `Last update ${to}timed out: the installer did not finish in time${finished}.`
    case 'WatcherStopped':
      return (
        `Last update ${to}not confirmed: nothing wrote its verdict, and the device runs ` +
        'another version.'
      )
    case 'Unreadable':
      return "Last update: the device's install record could not be read."
    default:
      return ''
  }
}

/** The install marker for a device, or `null` when there is nothing to look
 *  at: no word, no install on record, a verified one, or a word this page does
 *  not know. */
export function installChip(
  w: LastInstallWord | null | undefined,
  now: number = Date.now(),
): HealthChip | null {
  if (!w) return null
  if (w.status === 'InProgress') {
    return w.never_reported
      ? { text: 'update never reported', color: 'warning', tooltip: installSentence(w, now) }
      : { text: 'updating', color: 'info', tooltip: installSentence(w, now) }
  }
  const chip = INSTALL_CHIPS[w.status]
  return chip ? { ...chip, tooltip: installSentence(w, now) } : null
}

/** "10 min" for a window in seconds, or '' with none. */
function windowText(secs: number): string {
  if (!secs || secs <= 0) return ''
  return secs >= 60 ? `${Math.round(secs / 60)} min` : `${secs} s`
}

/** "2 crashes in 10 min", the marker's text. */
function crashCount(c: CrashHealth): string {
  const n = c.crashes ?? 0
  const win = windowText(c.window_secs)
  return `${n} ${n === 1 ? 'crash' : 'crashes'}${win ? ` in ${win}` : ''}`
}

/** "2 times in 10 min", for a sentence. */
function crashTimes(c: CrashHealth): string {
  const n = c.crashes ?? 0
  const win = windowText(c.window_secs)
  return `${n === 1 ? 'once' : `${n} times`}${win ? ` in ${win}` : ''}`
}

/** What the crash word means, or '' when there is nothing to report: no word,
 *  `ok`, or a state this page does not know. The crash reports dialog shows it
 *  at its top, since a tap on a touch screen shows no title. */
export function crashSentence(
  c: CrashHealth | null | undefined,
  now: number = Date.now(),
): string {
  if (!c) return ''
  const last = ago(c.last_crash_unix, now)
  const lastSaid = last ? ` The last crash was${last}.` : ''
  const good = shown(c.last_known_good)
  switch (c.state) {
    case 'crashing':
      return (
        `The daemon crashed ${crashTimes(c)}.${lastSaid} Three inside the window trip its ` +
        `crash ladder${good ? `, whose last good version is ${good}` : ''}.`
      )
    case 'rolled_back':
      return (
        `The daemon crashed ${crashTimes(c)} and rolled itself back${good ? ` to ${good}` : ''}.` +
        `${lastSaid} This clears once the rolled-back build has run clean for 5 min.`
      )
    case 'rollback_failed': {
      const accused = shown(c.accused_version)
      return (
        'The crash ladder tripped and could not roll back (rollback_failed): ' +
        `${accused || 'the running build'} crash-looped and is still the one running.` +
        `${lastSaid} Install a good release on it by hand` +
        `${good ? ` (its last good one was ${good})` : ''}.`
      )
    }
    default:
      return ''
  }
}

const REPORTS = 'Tap for the crash reports.'

/** The crash marker for a device, or `null` when there is nothing to report
 *  ({@link crashSentence}'s rule). Its tap opens the device's crash reports. */
export function crashChip(
  c: CrashHealth | null | undefined,
  now: number = Date.now(),
): HealthChip | null {
  const said = crashSentence(c, now)
  if (!c || !said) return null
  const tooltip = `${said} ${REPORTS}`
  switch (c.state) {
    case 'crashing':
      return { text: crashCount(c), color: 'warning', tooltip }
    case 'rolled_back': {
      const good = shown(c.last_known_good)
      return { text: good ? `rolled back to ${good}` : 'rolled back', color: 'warning', tooltip }
    }
    case 'rollback_failed': {
      const accused = shown(c.accused_version)
      return { text: accused ? `crash loop: ${accused}` : 'crash loop', color: 'error', tooltip }
    }
    default:
      return null
  }
}
