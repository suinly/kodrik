self.addEventListener('install', event => event.waitUntil(self.skipWaiting()))
self.addEventListener('activate', event => event.waitUntil(self.clients.claim()))

self.addEventListener('push', event => {
  let data = {}
  try { data = event.data?.json() ?? {} } catch {}
  const url = new URL('app/chat', self.registration.scope)
  if (typeof data.session_id === 'string') url.pathname += `/${encodeURIComponent(data.session_id)}`
  const approval = typeof data.event_id === 'string' && data.event_id.startsWith('approval:')
  event.waitUntil(self.registration.showNotification(data.locale === 'ru' ? 'Кодрик' : 'Kodrik', {
    body: approval
      ? (data.locale === 'ru' ? 'Требуется подтверждение действия' : 'Action approval required')
      : (data.locale === 'ru' ? 'Новое сообщение в Kodrik' : 'New message in Kodrik'),
    icon: new URL('chat/icon-192.png', self.registration.scope).href,
    tag: `kodrik-${typeof data.event_id === 'string' ? data.event_id : 'message'}`,
    data: {url: url.href},
  }))
})

self.addEventListener('notificationclick', event => {
  event.notification.close()
  event.waitUntil((async () => {
    const fallback = new URL('app/chat', self.registration.scope)
    let url = fallback
    try {
      const target = new URL(event.notification.data?.url)
      if (target.origin === fallback.origin && (target.pathname === fallback.pathname || target.pathname.startsWith(`${fallback.pathname}/`))) url = target
    } catch {}
    const windows = await self.clients.matchAll({type: 'window', includeUncontrolled: true})
    const client = windows.find(client => client.url === url.href)
      ?? windows.find(client => client.url.startsWith(new URL('app/', self.registration.scope).href))
    if (client) {
      if (client.url !== url.href) await client.navigate(url.href)
      await client.focus()
    } else await self.clients.openWindow(url.href)
  })())
})
