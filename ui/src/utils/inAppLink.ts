// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
import type { RouteLocationNormalizedLoaded, Router } from 'vue-router'

/**
 * #2841 - whether the page this tab shows holds a live session that leaving
 * would end or hide: a remote-control viewer (it disconnects in
 * `onBeforeUnmount`) or a call. Declared per route as `meta.liveSession`.
 */
export function holdsLiveSession(route: Pick<RouteLocationNormalizedLoaded, 'meta'>): boolean {
  return route.meta.liveSession === true
}

export type InAppLinkOutcome = 'here' | 'new-tab' | 'navigated'

/**
 * Follow an in-app link, such as a notification's, without ever taking this
 * tab away from a live session: the rule #2828 gave a push card's click,
 * applied to links clicked inside the app. On a live-session page the link
 * opens in a new tab. Anywhere else it navigates in place, as it always did.
 * A link to the page already showing does nothing.
 *
 * Measured 2026-10-24: a consent request clicked in the bell moved a live
 * viewer to the consent page, and the device hung its session up 3 s later.
 */
export function openInAppLink(router: Router, link: string): InAppLinkOutcome {
  const current = router.currentRoute.value
  const target = router.resolve(link)
  if (target.fullPath === current.fullPath) return 'here'
  if (holdsLiveSession(current)) {
    window.open(target.href, '_blank', 'noopener')
    return 'new-tab'
  }
  router.push(link)
  return 'navigated'
}
