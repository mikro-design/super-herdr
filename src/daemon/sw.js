// This worker exists to be woken by a push with no payload, ask this same
// daemon what actually happened, and show that — never to interpret or store
// anything the push service itself sent, which is nothing.
//
// The scope this was registered with already carries whatever path prefix a
// bridge route adds in front of the daemon's own paths, exactly the way the
// page's own `endpoint()` helper does; deriving it from `self.registration`
// rather than hardcoding it is what keeps this worker correct behind a
// bridge route and on a direct connection alike.
const basePath = new URL(self.registration.scope).pathname.replace(/\/$/, '');
const endpoint = path => `${basePath}${path}`;

function paneKey(agent) {
  return `${agent.target}/${agent.session}/${agent.resource}`;
}

self.addEventListener('push', event => {
  event.waitUntil(handlePush());
});

async function handlePush() {
  // Credentials are same-origin by default for a same-origin fetch, which is
  // what carries the pairing cookie — nothing here reads or forwards it
  // itself.
  const response = await fetch(endpoint('/push/wake')).catch(() => null);
  if (!response || !response.ok) return;
  const message = await response.json().catch(() => null);
  if (!message || !message.agent) return;

  // A visible window already has this over its live connection; a push
  // notification on top of that would be the same alert twice.
  const clientList = await self.clients.matchAll({ type: 'window', includeUncontrolled: true });
  if (clientList.some(client => client.visibilityState === 'visible' || client.focused)) return;

  await self.registration.showNotification(message.title, {
    body: message.body,
    tag: paneKey(message.agent),
    data: { agent: message.agent },
  });
}

self.addEventListener('notificationclick', event => {
  event.notification.close();
  event.waitUntil(focusAndOpenAgent(event.notification.data?.agent));
});

async function focusAndOpenAgent(agent) {
  const clientList = await self.clients.matchAll({ type: 'window', includeUncontrolled: true });
  for (const client of clientList) {
    if ('focus' in client) await client.focus();
    client.postMessage({ type: 'open-agent-from-alert', agent });
    return;
  }
  // Nothing open to post to; opening the app is the most this can do, and the
  // inbox is where whoever opens it would look next anyway.
  if (self.clients.openWindow) await self.clients.openWindow(endpoint('/'));
}
