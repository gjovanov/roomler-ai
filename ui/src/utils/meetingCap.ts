// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * FR-115 P5 — what a participant reads when the plan's meeting-length cap
 * (Free: 60 minutes) warns, five minutes before its end, and when it ends the
 * meeting. The server sends `room:call_ending_soon {ends_at, max_minutes,
 * plan}` and `room:call_ended {reason: "plan_limit", max_minutes, plan}`
 * (crates/modules/conference/src/meeting_cap.rs).
 *
 * The minutes left are counted from `ends_at`, not from the sweep's
 * moment, so a warning that lands late still tells the truth.
 */

function planWord(plan?: string): string {
  return plan ? plan.charAt(0).toUpperCase() + plan.slice(1) : ''
}

export function callEndingSoonMessage(
  endsAt: number,
  maxMinutes: number,
  plan?: string,
  now: number = Date.now(),
): string {
  const left = Math.max(1, Math.ceil((endsAt - now) / 60_000))
  const minutes = left === 1 ? '1 minute' : `${left} minutes`
  const where = plan ? `on the ${planWord(plan)} plan` : 'on your plan'
  return `This meeting ends in ${minutes}. Meetings ${where} run up to ${maxMinutes} minutes; upgrade for meetings with no time limit.`
}

export function callEndedByPlanMessage(maxMinutes: number, plan?: string): string {
  const whose = plan ? `the ${planWord(plan)} plan's` : "your plan's"
  return `This meeting reached ${whose} ${maxMinutes}-minute limit. Upgrade for meetings with no time limit.`
}
