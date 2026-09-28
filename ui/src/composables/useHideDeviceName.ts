// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
import { computed, ref, type ComputedRef, type Ref, type WritableComputedRef } from 'vue'

/**
 * "Hide device name when a display name is set" — ONE per-user, per-org
 * preference, read by every surface that labels a device next to its display
 * name: the Devices grid (the checkbox in its column picker) and the
 * remote-control page (the same checkbox under Settings › Display). Default
 * ON: once someone has named a device, the machine-reported title is noise.
 *
 * Storage: `roomler:grid-name-pref:<scope>:devices` → '1' | '0', where the
 * caller's scope is `${userId}:${tenantId}` (as for useGridColumns). The key
 * predates this composable — the Devices grid kept it inline — and stays
 * byte-identical so every saved choice carries over.
 *
 * localStorage stays the one source of truth. A flip made through any
 * instance bumps `revision`, so every other surface already mounted in the
 * tab re-reads at once; a flip in another tab arrives as a `storage` event.
 */

/** The storage key for one scope (`${userId}:${tenantId}`). */
export function hideDeviceNameKey(scope: string): string {
  return `roomler:grid-name-pref:${scope || 'anon'}:devices`
}

function read(key: string): boolean {
  try {
    const v = localStorage.getItem(key)
    return v === null ? true : v === '1'
  } catch {
    return true
  }
}

const revision = ref(0)
let listening = false

function listenAcrossTabs() {
  if (listening || typeof window === 'undefined') return
  listening = true
  window.addEventListener('storage', (e: StorageEvent) => {
    // `key === null` is another tab's localStorage.clear().
    if (e.key === null || e.key.startsWith('roomler:grid-name-pref:')) revision.value++
  })
}

export function useHideDeviceName(
  /** Per-user/per-org partition — `() => \`${userId}:${tenantId}\``. */
  scope: Ref<string> | ComputedRef<string> | (() => string),
): WritableComputedRef<boolean> {
  const scopeRef = typeof scope === 'function' ? computed(scope) : scope
  const key = computed(() => hideDeviceNameKey(scopeRef.value))
  listenAcrossTabs()
  return computed<boolean>({
    get: () => {
      void revision.value
      return read(key.value)
    },
    set: (v: boolean) => {
      try {
        localStorage.setItem(key.value, v ? '1' : '0')
      } catch {
        /* private browsing */
      }
      revision.value++
    },
  })
}

/**
 * The machine-reported name to show BESIDE a device's display name, or null
 * when there is nothing to add: no display name (the name already IS the
 * title), a display name equal to the name, or the viewer hides it.
 */
export function secondaryDeviceName(
  device: { name?: string | null; display_name?: string | null } | null | undefined,
  hideWhenDisplayName: boolean,
): string | null {
  if (!device?.name || !device.display_name || device.display_name === device.name) return null
  return hideWhenDisplayName ? null : device.name
}
