// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
import { computed, ref, watch, type ComputedRef, type Ref } from 'vue'
import { useDisplay } from 'vuetify'

/**
 * FR-116 — a long list on a phone shows a screenful at a time.
 *
 * Below 600 px (`xs`) a `table-stack-xs` row becomes a card, and a card is
 * several lines tall. A list that fetched its last 100 entries was then 100
 * cards in one page: the exec audit measured 43,398 px at 360 px on
 * production (2026-10-20), a long scroll to reach anything but the newest.
 * On a phone the list shows `step` rows, then "Show more" adds the next
 * `step`. Wider screens see every row, as before. A reload (a new array)
 * starts over at the first `step`.
 *
 * ⚠️ `xs`, never `useDisplay().mobile` (see `useResponsiveDialog`).
 */
export function usePhoneSlice<T>(
  items: Ref<T[]>,
  step = 20,
): {
  visible: ComputedRef<T[]>
  remaining: ComputedRef<number>
  nextCount: ComputedRef<number>
  showMore: () => void
} {
  const { xs } = useDisplay()
  const shown = ref(step)
  watch(items, () => {
    shown.value = step
  })
  const visible = computed(() => (xs.value ? items.value.slice(0, shown.value) : items.value))
  const remaining = computed(() => (xs.value ? Math.max(0, items.value.length - shown.value) : 0))
  const nextCount = computed(() => Math.min(step, remaining.value))
  return {
    visible,
    remaining,
    nextCount,
    showMore: () => {
      shown.value += step
    },
  }
}
