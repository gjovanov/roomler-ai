import { computed, ref, watch } from 'vue'

/**
 * FR-12 (#788) — per-user tutorial state, entirely client-side.
 *
 * Two independent things live here, both keyed by user id so a shared
 * machine doesn't leak one person's state to the next:
 *
 *  - **seen**: has the welcome tour ever been auto-opened for this user
 *    (`roomler-tour-seen:<userId>`). Set on the FIRST auto-open and never
 *    cleared by normal use, so the tour can never ambush someone twice.
 *  - **progress**: which chapters they ticked off
 *    (`roomler:tour-progress:<userId>`) — same storage conventions as
 *    `useGridColumns`.
 *
 * Every access is try/catch'd: localStorage throws in private windows and
 * under some enterprise policies, and a tutorial must never be the thing
 * that breaks the app shell.
 */

export const TOUR_SEEN_PREFIX = 'roomler-tour-seen:'
export const TOUR_PROGRESS_PREFIX = 'roomler:tour-progress:'

export function tourSeenKey(userId: string): string {
  return `${TOUR_SEEN_PREFIX}${userId || 'anon'}`
}

export function tourProgressKey(userId: string): string {
  return `${TOUR_PROGRESS_PREFIX}${userId || 'anon'}`
}

/** Has the tour already been shown to this user? Unreadable storage reads
 *  as "seen" — failing that way means we never auto-navigate someone we
 *  can't remember having done it to. */
export function hasSeenTour(userId: string): boolean {
  try {
    return globalThis.localStorage?.getItem(tourSeenKey(userId)) != null
  } catch {
    return true
  }
}

export function markTourSeen(userId: string): void {
  try {
    globalThis.localStorage?.setItem(tourSeenKey(userId), new Date().toISOString())
  } catch {
    /* private window / policy — the tour just may open again next session */
  }
}

export function readTourProgress(userId: string): string[] {
  try {
    const raw = globalThis.localStorage?.getItem(tourProgressKey(userId))
    const parsed = raw ? JSON.parse(raw) : null
    return Array.isArray(parsed) ? parsed.filter((c: unknown) => typeof c === 'string') : []
  } catch {
    return []
  }
}

export function writeTourProgress(userId: string, done: string[]): void {
  try {
    globalThis.localStorage?.setItem(tourProgressKey(userId), JSON.stringify(done))
  } catch {
    /* non-fatal — progress ticks are a convenience, not state we own */
  }
}

/**
 * Should the tour auto-open for this user right now?
 *
 * Deliberately narrow: an established org never surprises its members. The
 * "fresh" heuristic is the org having no devices AND at most one room —
 * i.e. nothing has been set up yet. `devices`/`rooms` are counts the
 * caller already has; pass `null` for a count that hasn't loaded, which
 * suppresses the auto-open (we only navigate on positive evidence).
 */
export function shouldAutoOpenTour(opts: {
  userId: string | undefined
  devices: number | null
  rooms: number | null
}): boolean {
  if (!opts.userId) return false
  if (opts.devices == null || opts.rooms == null) return false
  if (hasSeenTour(opts.userId)) return false
  return opts.devices === 0 && opts.rooms <= 1
}

/** Reactive per-user chapter checklist for the Tutorial view. */
export function useTutorialProgress(userId: () => string | undefined) {
  const done = ref<string[]>(readTourProgress(userId() ?? ''))

  // A tenant/user switch reloads from that identity's own bucket.
  watch(
    () => userId(),
    (u) => {
      done.value = readTourProgress(u ?? '')
    },
  )

  function isDone(chapterId: string): boolean {
    return done.value.includes(chapterId)
  }

  function toggle(chapterId: string, value?: boolean): void {
    const next = value ?? !isDone(chapterId)
    done.value = next
      ? [...new Set([...done.value, chapterId])]
      : done.value.filter((c) => c !== chapterId)
    writeTourProgress(userId() ?? '', done.value)
  }

  function reset(): void {
    done.value = []
    writeTourProgress(userId() ?? '', [])
  }

  const doneCount = computed(() => done.value.length)

  return { done, doneCount, isDone, toggle, reset }
}
