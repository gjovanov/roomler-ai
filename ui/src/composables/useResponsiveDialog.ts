// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
import { computed, type ComputedRef } from 'vue'
import { useDisplay } from 'vuetify'

/**
 * FR-116 — how a dialog sizes itself on a phone.
 *
 * Below 600 px (`xs`) a dialog of any substance goes full screen: Vuetify
 * keeps a 24 px margin around a centred dialog, which leaves 264 px of
 * content at 360 px, and a form, a list or a command block in that width is
 * unusable. A full-screen dialog has no scrim to tap, so it must carry its own
 * close control (a toolbar or title-row button).
 *
 * ⚠️ `xs`, never `useDisplay().mobile`: this app sets no `mobileBreakpoint`,
 * so Vuetify's `mobile` means "narrower than 1280 px" — a laptop window — and
 * a dialog bound to it covers the screen there too.
 */
export function useResponsiveDialog(): { fullscreen: ComputedRef<boolean> } {
  const { xs } = useDisplay()
  return { fullscreen: computed(() => xs.value) }
}
