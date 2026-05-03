// claws phone — service worker. Phase 1: offline shell only.
// Phase 3 wires Web Push (push, notificationclick) here.

const CACHE = "claws-shell-v1";
const SHELL = ["/", "/index.html", "/app.js", "/manifest.webmanifest", "/icon.svg"];

self.addEventListener("install", (e) => {
  e.waitUntil(caches.open(CACHE).then((c) => c.addAll(SHELL)).then(() => self.skipWaiting()));
});

self.addEventListener("activate", (e) => {
  e.waitUntil((async () => {
    const keys = await caches.keys();
    await Promise.all(keys.filter((k) => k !== CACHE).map((k) => caches.delete(k)));
    await self.clients.claim();
  })());
});

self.addEventListener("fetch", (e) => {
  const url = new URL(e.request.url);
  if (url.origin !== location.origin) return;
  // Never cache API/WS — always live.
  if (url.pathname.startsWith("/api/")) return;
  // Network-first for HTML so deploys take effect immediately.
  if (e.request.mode === "navigate" || url.pathname === "/" || url.pathname.endsWith(".html")) {
    e.respondWith((async () => {
      try {
        const resp = await fetch(e.request);
        const c = await caches.open(CACHE);
        c.put(e.request, resp.clone());
        return resp;
      } catch {
        const cached = await caches.match(e.request);
        return cached || caches.match("/index.html");
      }
    })());
    return;
  }
  // Cache-first for static assets.
  e.respondWith((async () => {
    const cached = await caches.match(e.request);
    if (cached) return cached;
    try {
      const resp = await fetch(e.request);
      if (resp.ok) {
        const c = await caches.open(CACHE);
        c.put(e.request, resp.clone());
      }
      return resp;
    } catch {
      return cached || new Response("offline", { status: 503 });
    }
  })());
});
