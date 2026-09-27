"use strict";
// intercom phone PWA service worker (issue 1345 M3b). It exists ONLY to satisfy the browser's
// PWA-install requirement (a registered service worker + a manifest), so a cameraman can install
// the Interkom page as a windowed app on the phone home screen. It deliberately does NO caching:
// the page is server-truth (a live intercom client — it must never serve a stale UI, a stale
// Janus config, or a frozen picture), so `fetch` is a pure passthrough to the network. The Cache
// Storage API is NOT used anywhere — a cached intercom client is a broken intercom
// client.
//
// Worker version: 2 (issue 1379, the picture stream bypasses the worker). Bump it on EVERY change
// to this file: an installed phone keeps its registered worker across hub versions and replaces it
// only when the bytes of /sw.js differ.

// Requests the worker leaves to the browser (no respondWith, so the browser fetches them itself):
// - the MJPEG picture stream (`/interkom.mjpeg?t=...`): WebKit (iPhone Safari) does not deliver an
//   endless multipart/x-mixed-replace response to the page's <img> through respondWith, so the
//   picture never appeared on the iPhone (issue 1379);
// - anything that is not a GET — there is nothing to gain from proxying it.
const STREAM_PATHS = new Set(["/interkom.mjpeg"]);

function leaveToBrowser(request) {
  if (request.method !== "GET") return true;
  return STREAM_PATHS.has(new URL(request.url).pathname);
}

self.addEventListener("install", () => self.skipWaiting());
self.addEventListener("activate", (event) => event.waitUntil(self.clients.claim()));
self.addEventListener("fetch", (event) => {
  if (leaveToBrowser(event.request)) return;
  event.respondWith(fetch(event.request));
});
