// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
self.addEventListener('push', (event) => {
  if (!event.data) return

  let data
  try {
    data = event.data.json()
  } catch {
    data = { title: 'Roomler', body: event.data.text() }
  }

  const options = {
    body: data.body || '',
    icon: '/favicon.svg',
    badge: '/favicon.svg',
    data: { url: data.url || '/' },
  }

  event.waitUntil(self.registration.showNotification(data.title || 'Roomler', options))
})

// #2827 - this worker caches nothing, so a new version may take over at once.
// Waiting would leave the old click handler in charge until every roomler
// tab was closed.
self.addEventListener('install', () => self.skipWaiting())
self.addEventListener('activate', (event) => event.waitUntil(self.clients.claim()))

self.addEventListener('notificationclick', (event) => {
  event.notification.close()
  const url = new URL(event.notification.data?.url || '/', self.location.origin).href
  event.waitUntil(
    clients.matchAll({ type: 'window', includeUncontrolled: true }).then((clientList) => {
      // #2827 - focus a window ALREADY on this page; never navigate another
      // one away. The first roomler window is as likely to be a live remote
      // session or a call as anything else, and navigating it tears that
      // down: field 2026-10-24, a push consent request's own toast hung up
      // the very session it was asking about, a second before the approval.
      for (const client of clientList) {
        if (client.url === url && 'focus' in client) return client.focus()
      }
      return clients.openWindow(url)
    }),
  )
})
