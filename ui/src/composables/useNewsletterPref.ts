// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// FR-58 P4 — the signed-in newsletter preference. A thin wrapper over
// GET/PUT /api/user/newsletter (which writes the same `subscribers` store the
// public form does — a different door, not a second list).
//
// `subscribed` is three-state on purpose: `null` = not loaded (or the load
// failed), and every asking surface must treat `null` as "do not prompt" —
// the only-prompt-on-positive-evidence idiom from the tutorial auto-open.
import { ref } from 'vue'
import { api } from '@/api/client'

interface NewsletterPref {
  subscribed: boolean
}

export function useNewsletterPref() {
  const subscribed = ref<boolean | null>(null)
  const busy = ref(false)

  async function load(): Promise<void> {
    try {
      subscribed.value = (await api.get<NewsletterPref>('/user/newsletter')).subscribed
    } catch {
      // Leave `null`; surfaces reading it must not prompt on it.
    }
  }

  async function set(value: boolean): Promise<boolean> {
    busy.value = true
    try {
      subscribed.value = (
        await api.put<NewsletterPref>('/user/newsletter', { subscribed: value })
      ).subscribed
      return true
    } catch {
      return false
    } finally {
      busy.value = false
    }
  }

  return { subscribed, busy, load, set }
}
