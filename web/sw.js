"use strict";
// intercom phone PWA service worker (issue 1345 M3b). It exists ONLY to satisfy the browser's
// PWA-install requirement (a registered service worker + a manifest), so a cameraman can install
// the Interkom page as a windowed app on the phone home screen. It deliberately does NO caching:
// the page is server-truth (a live intercom client — it must never serve a stale UI, a stale
// Janus config, or a frozen picture), so `fetch` is a pure passthrough to the network. The Cache
// Storage API is NOT used anywhere — a cached intercom client is a broken intercom
// client.
self.addEventListener("install", () => self.skipWaiting());
self.addEventListener("activate", (event) => event.waitUntil(self.clients.claim()));
self.addEventListener("fetch", (event) => {
  event.respondWith(fetch(event.request));
});
