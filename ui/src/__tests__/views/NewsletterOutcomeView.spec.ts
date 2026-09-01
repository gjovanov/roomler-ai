// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// FR-58 — the public confirm/unsubscribe outcome pages. The load-bearing
// assertions: only an explicit `status=ok` renders success (a missing or
// unknown status must NOT claim an action happened), and the two kinds render
// distinct success copy. These pages are the replacement for a handler that
// was dead code because its route was auth-gated — so what they render is the
// only outcome a subscriber ever sees.
import { describe, it, expect } from 'vitest'
import { mount } from '@vue/test-utils'
import { createVuetify } from 'vuetify'
import * as components from 'vuetify/components'
import * as directives from 'vuetify/directives'
import { createMemoryHistory, createRouter } from 'vue-router'
import NewsletterOutcomeView from '@/views/newsletter/NewsletterOutcomeView.vue'

const vuetify = createVuetify({ components, directives })

async function mountAt(kind: 'confirmed' | 'unsubscribed', query: string) {
  const router = createRouter({
    history: createMemoryHistory(),
    routes: [
      { path: '/', component: { template: '<div />' } },
      { path: '/newsletter/:page', component: { template: '<div />' } },
    ],
  })
  await router.push(`/newsletter/${kind}${query}`)
  return mount(NewsletterOutcomeView, {
    props: { kind },
    global: { plugins: [vuetify, router] },
  })
}

describe('NewsletterOutcomeView', () => {
  it('renders the confirmed success copy for status=ok', async () => {
    const w = await mountAt('confirmed', '?status=ok')
    expect(w.text()).toContain("You're on the list")
    expect(w.text()).toContain('one-click unsubscribe')
  })

  it('renders the unsubscribed success copy for status=ok', async () => {
    const w = await mountAt('unsubscribed', '?status=ok')
    expect(w.text()).toContain("You're unsubscribed")
    expect(w.text()).toContain('confirm again')
  })

  it('renders the invalid copy for status=invalid', async () => {
    const w = await mountAt('confirmed', '?status=invalid')
    expect(w.text()).toContain("That link didn't work")
  })

  it('treats a MISSING status as invalid, never as success', async () => {
    // Someone arriving without a real link must not be told an action
    // happened. `status=ok` is the only success spelling.
    for (const kind of ['confirmed', 'unsubscribed'] as const) {
      const w = await mountAt(kind, '')
      expect(w.text()).toContain("That link didn't work")
      expect(w.text()).not.toContain("You're on the list")
      expect(w.text()).not.toContain("You're unsubscribed")
    }
  })
})
