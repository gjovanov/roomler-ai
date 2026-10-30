// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
import { defineStore } from 'pinia'
import { computed, ref } from 'vue'
import { api } from '@/api/client'

/** `GET /api/setup/features` (FR-129 B1-s) — unauthenticated, one request per
 *  page load. Only what the server sent as a JSON boolean counts. */
export interface SetupFeatures {
  /** `[pairing] enabled`: `/api/pair/*` answers — a fact about the SERVER,
   *  on by default. */
  pairing?: boolean
  /** FR-129 B1-s2 — the released Roomler Setup installs the full daemon on
   *  Linux (B1), so the Linux cards may offer it. */
  wizard_linux_daemon?: boolean
  /** FR-129 B1-s2 — the same for macOS. */
  wizard_macos_daemon?: boolean
  /** Both OSes at once: the one switch before B1-s2. A server from before
   *  the split sends it alone; a split server sends `linux && macos`, the
   *  answer the wizards released before the split read for both OSes. */
  wizard_unix_daemon?: boolean
  /** `[setup] wizard_pairing`: the released Roomler Setup pairs (FR-129 A3),
   *  so the homepages may say "sign in from your browser" — with `pairing`. */
  wizard_pairing?: boolean
}

/**
 * FR-129 B1-s — the server's install switches, the one gate for the download
 * cards (`installCards`: the landing page) and the enrollment dialog.
 *
 * Deliberately FAIL-CLOSED, the opposite of `useCapabilitiesStore.has` and the
 * same rule as its `feature()`: before the request lands, when it fails (an
 * older server answers 404, a server without the fleet module 503, a proxy an
 * HTML error page), and for anything the server did not send as a JSON
 * `true`, every switch reads `false`. A switch here is something the page
 * OFFERS — a wizard download for a daemon the released wizard cannot install,
 * a "sign in from your browser" the server or the released wizard cannot
 * honour — and offering what cannot be honoured is the mistake the switch
 * exists to prevent. Failing closed only hides a button the terminal command
 * beside it already covers.
 */
export const useSetupFeaturesStore = defineStore('setupFeatures', () => {
  const pairing = ref(false)
  /** FR-129 B1-s2 — the Linux cards may offer the daemon wizard. */
  const wizardLinuxDaemon = ref(false)
  /** FR-129 B1-s2 — the macOS cards may offer the daemon wizard. */
  const wizardMacosDaemon = ref(false)
  const wizardPairing = ref(false)
  /** True once the server has answered, or refused to. */
  const loaded = ref(false)
  const failed = ref(false)

  /**
   * The homepages' question: can the download the visitor is about to take
   * sign them in from the browser against THIS server? Both must hold —
   * `pairing` is the server's fact (on by default), `wizardPairing` the
   * operator's word on the released wizard. Either alone keeps the token
   * lead: a server promoted before the first `setup-v*` carrying A3 must
   * not promise a flow the download cannot do.
   */
  const wizardPairs = computed(() => pairing.value && wizardPairing.value)

  let inflight: Promise<void> | null = null

  /** Only a JSON `true` switches anything on: never `"true"`, `1` or a missing key. */
  function apply(features: unknown): void {
    const f = (features && typeof features === 'object' ? features : {}) as Record<string, unknown>
    pairing.value = f.pairing === true
    // FR-129 B1-s2 — each OS reads its own switch, or `wizard_unix_daemon`:
    // a server from before the split sends that one key for both OSes, and a
    // split server sends it as `linux && macos`, so it never turns on an OS
    // whose own key the server left off.
    const both = f.wizard_unix_daemon === true
    wizardLinuxDaemon.value = f.wizard_linux_daemon === true || both
    wizardMacosDaemon.value = f.wizard_macos_daemon === true || both
    wizardPairing.value = f.wizard_pairing === true
    loaded.value = true
    failed.value = false
  }

  /** Fetch once; concurrent callers share the request. Never throws. */
  function load(): Promise<void> {
    if (loaded.value) return Promise.resolve()
    if (inflight) return inflight
    inflight = api
      .get<unknown>('/setup/features')
      .then(apply)
      .catch((e: unknown) => {
        // Fail CLOSED — every switch stays false. Quiet for the one expected
        // case, an older server with no such route (404): it offers what it
        // always offered. Anything else is worth a line in the console.
        failed.value = true
        loaded.value = true
        if ((e as { status?: number } | null)?.status !== 404) {
          console.warn('[setup] /api/setup/features unavailable — offering the daemon wizard on Windows only', e)
        }
      })
      .finally(() => {
        inflight = null
      })
    return inflight
  }

  /** The request that is (or was) in flight. */
  function ready(): Promise<void> {
    return loaded.value ? Promise.resolve() : load()
  }

  /** Test seam / logout reset. */
  function reset(): void {
    pairing.value = false
    wizardLinuxDaemon.value = false
    wizardMacosDaemon.value = false
    wizardPairing.value = false
    loaded.value = false
    failed.value = false
    inflight = null
  }

  return {
    pairing,
    wizardLinuxDaemon,
    wizardMacosDaemon,
    wizardPairing,
    wizardPairs,
    loaded,
    failed,
    load,
    ready,
    apply,
    reset,
  }
})
