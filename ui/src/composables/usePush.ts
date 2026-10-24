// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
import { api } from '@/api/client'

function urlBase64ToUint8Array(base64String: string): Uint8Array {
  const padding = '='.repeat((4 - (base64String.length % 4)) % 4)
  const base64 = (base64String + padding).replace(/-/g, '+').replace(/_/g, '/')
  const rawData = atob(base64)
  const outputArray = new Uint8Array(rawData.length)
  for (let i = 0; i < rawData.length; i++) {
    outputArray[i] = rawData.charCodeAt(i)
  }
  return outputArray
}

/** #2817 - where this browser stands on desktop notifications, as the opt-in
 *  needs it: `unsupported` (no service worker or Push API), `not_configured`
 *  (the server has no push service), or the browser's own permission. */
export type PushState = 'unsupported' | 'not_configured' | NotificationPermission

function pushSupported(): boolean {
  return 'serviceWorker' in navigator && 'PushManager' in window && 'Notification' in window
}

/**
 * #2817 - may a page LOAD subscribe? Only where this browser already granted
 * permission: then it is a silent re-subscribe. Never prompt from a load or a
 * sign-in. An unasked "Allow notifications?" asks before the person has any
 * reason to say yes, and browsers quietly suppress the prompts of sites that
 * ask on load - spending the one chance that matters.
 */
export function shouldSubscribeOnLoad(permission: PushState): boolean {
  return permission === 'granted'
}

async function pushConfig(): Promise<string> {
  const config = await api.get<{ vapid_public_key: string }>('/push/config')
  return config.vapid_public_key
}

async function subscribeWith(vapidPublicKey: string): Promise<void> {
  const registration = await navigator.serviceWorker.register('/sw.js')
  await navigator.serviceWorker.ready

  const keyArray = urlBase64ToUint8Array(vapidPublicKey)
  const subscription = await registration.pushManager.subscribe({
    userVisibleOnly: true,
    applicationServerKey: keyArray.buffer as ArrayBuffer,
  })

  const json = subscription.toJSON()
  await api.post('/push/subscribe', {
    endpoint: json.endpoint,
    keys: {
      auth: json.keys?.auth,
      p256dh: json.keys?.p256dh,
    },
  })
}

/**
 * Called on sign-in and on every load of a signed-in app. Re-subscribes
 * SILENTLY where permission is already granted (a subscription can lapse or
 * belong to another account), and otherwise does nothing: the prompt belongs
 * to {@link enablePushNotifications}, behind a click.
 */
export async function subscribePush(): Promise<void> {
  if (!pushSupported()) return
  if (!shouldSubscribeOnLoad(Notification.permission)) return

  try {
    const key = await pushConfig()
    if (!key) return
    await subscribeWith(key)
  } catch (e) {
    console.warn('Push subscription failed:', e)
  }
}

/** #2817 - this browser's push state, for the opt-in to decide what to show. */
export async function pushState(): Promise<PushState> {
  if (!pushSupported()) return 'unsupported'
  try {
    if (!(await pushConfig())) return 'not_configured'
  } catch {
    return 'not_configured'
  }
  return Notification.permission
}

/**
 * #2817 - the opt-in, from a click: ask the browser, and subscribe on a yes.
 * Returns where things stand afterwards, so the caller can say so.
 */
export async function enablePushNotifications(): Promise<PushState> {
  if (!pushSupported()) return 'unsupported'
  try {
    const key = await pushConfig()
    if (!key) return 'not_configured'
    const permission = await Notification.requestPermission()
    if (permission !== 'granted') return permission
    await subscribeWith(key)
    return 'granted'
  } catch (e) {
    console.warn('Push subscription failed:', e)
    return Notification.permission
  }
}

export async function unsubscribePush(): Promise<void> {
  if (!('serviceWorker' in navigator) || !('PushManager' in window)) return

  try {
    const registration = await navigator.serviceWorker.getRegistration()
    if (!registration) return

    const subscription = await registration.pushManager.getSubscription()
    if (!subscription) return

    await api.post('/push/unsubscribe', { endpoint: subscription.endpoint })
    await subscription.unsubscribe()
  } catch (e) {
    console.warn('Push unsubscribe failed:', e)
  }
}
