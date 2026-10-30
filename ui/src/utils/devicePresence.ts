// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD

/**
 * How a device row's presence reads on the devices grid: the dot's colour and
 * icon, the status chip's word, and what a tap on the chip explains.
 *
 * FR-55 P4 — "offline" used to be one word for crashed, network gone, powered
 * off and asleep, and each has a different fix. A device that said goodbye
 * before a sleep (`rc:agent.sleeping`) carries `sleep`: since when, and its
 * power posture at that moment (`crates/remote_control/src/models.rs`
 * `SleepView`). `held: false` means its keeper held nothing, so the OS slept it
 * as its `power_policy` allows: asleep BY POLICY. `held: true` means it went
 * to sleep past the hold: a closed lid, a person, or the OS overruling it.
 * An offline row without `sleep` gave no reason, and still reads "offline".
 */

export type DevicePresence = 'online' | 'stale' | 'offline'

/** The device's power posture, as it reported it (a claim, shown only). */
export interface PowerBrief {
  /** `never` · `on-ac` · `always`, or a word a newer agent knows. */
  policy: string
  /** `true` on mains, `false` on battery, absent = the device cannot tell. */
  on_ac?: boolean
  /** The keeper was holding the machine awake. */
  held: boolean
}

/** FR-55 P4 — why an offline device is offline: it announced a sleep. */
export interface SleepView {
  /** RFC 3339. */
  since: string
  power: PowerBrief
}

/** The fields of a device row this needs. */
export interface PresenceRow {
  kind: 'agent' | 'tunnel_client'
  presence: DevicePresence
  is_online: boolean
  os?: string
  sleep?: SleepView
}

/** Asleep: an agent that is offline because it said it was going to sleep.
 *  Only a device that is not online can be, whatever its row still says. */
export function isAsleep(r: PresenceRow): boolean {
  return r.kind === 'agent' && r.presence === 'offline' && !!r.sleep
}

export function presenceColor(r: PresenceRow): string {
  if (isAsleep(r)) return 'blue-grey'
  return r.presence === 'online' ? 'success' : r.presence === 'stale' ? 'warning' : 'grey'
}

export function presenceIcon(r: PresenceRow): string {
  if (isAsleep(r)) return 'mdi-sleep'
  return r.presence === 'online'
    ? 'mdi-circle'
    : r.presence === 'stale'
      ? 'mdi-circle-half-full'
      : 'mdi-circle-outline'
}

/** The status chip's word. */
export function presenceLabel(r: PresenceRow): string {
  return isAsleep(r) ? 'asleep' : r.presence
}

/** What a tap on the status chip explains. `fmt` renders the sleep's moment. */
export function presenceTitle(
  r: PresenceRow,
  fmt: (iso: string) => string = defaultFmt,
): string {
  if (r.kind === 'tunnel_client')
    return r.is_online ? 'Online (stored status)' : 'Offline (stored status)'
  if (isAsleep(r)) return sleepTitle(r.sleep!, r.os, fmt)
  return r.presence === 'online'
    ? 'Online — a control socket is registered'
    : r.presence === 'stale'
      ? 'Stale — heartbeat fresh but no socket registered (a half-open network leg or a ' +
        'recent server roll). It should heal within minutes; otherwise restart the Roomler ' +
        'service on the machine.'
      : 'Offline'
}

/** FR-55 P4 — the asleep chip's explanation: since when, whether the power
 *  policy let it sleep, and what keeps it reachable if that is wanted. */
export function sleepTitle(
  s: SleepView,
  os?: string,
  fmt: (iso: string) => string = defaultFmt,
): string {
  const source = s.power.on_ac === true ? ', on mains' : s.power.on_ac === false ? ', on battery' : ''
  const posture = `power policy “${s.power.policy}”${source}`
  const back = ' It reconnects by itself when it wakes.'
  if (!s.power.held) {
    let advice = ''
    if (s.power.policy === 'never')
      advice = ' To keep it reachable, set power_policy to on-ac or always on the device.'
    else if (s.power.policy === 'on-ac' && s.power.on_ac === false)
      advice = ' on-ac holds only on mains: plug it in, or set power_policy to always on the device.'
    return (
      `Asleep since ${fmt(s.since)}, as its power policy allows: nothing was holding it ` +
      `awake (${posture}).${back}${advice}`
    )
  }
  const lid =
    os === 'macos'
      ? ' A Mac with its lid closed and no external display sleeps whatever the policy says.'
      : ''
  return (
    `Asleep since ${fmt(s.since)}, past its hold (${posture}): a closed lid, a person or ` +
    `the OS put it to sleep anyway.${lid}${back}`
  )
}

function defaultFmt(iso: string): string {
  const d = new Date(iso)
  return Number.isNaN(d.getTime()) ? iso : d.toLocaleString()
}
