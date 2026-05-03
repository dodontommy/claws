// claws phone — service worker. Offline shell + Web Push handler.
//
// Cache strategy:
//   - Our own HTML/JS/CSS/manifest: network-first. We're iterating fast;
//     cache-first locks users on stale code and the service worker never
//     sees our updates.
//   - Vendored third-party assets (/vendor/*): cache-first. xterm.js etc.
//     don't change between deploys; serving from cache is faster.
//   - /api/*: never cache, always live.

const CACHE = "claws-shell-v3";
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

  // Vendored third-party assets: cache-first (immutable between deploys).
  if (url.pathname.startsWith("/vendor/")) {
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
    return;
  }

  // Everything else (HTML, our app.js, manifest, icon): network-first.
  // Cache fallback only when offline. Means edits land on the next refresh.
  e.respondWith((async () => {
    try {
      const resp = await fetch(e.request);
      if (resp.ok) {
        const c = await caches.open(CACHE);
        c.put(e.request, resp.clone());
      }
      return resp;
    } catch {
      const cached = await caches.match(e.request);
      if (cached) return cached;
      // Last-resort SPA fallback for navigations.
      if (e.request.mode === "navigate") return caches.match("/index.html");
      return new Response("offline", { status: 503 });
    }
  })());
});

// ---- Web Push ---------------------------------------------------------------

self.addEventListener("push", (event) => {
  // Daemon payload: { kind, session_id, title, body }. Tag by session_id so a
  // burst of awaiting_permission events on the same session collapses into
  // one notification instead of stacking.
  let data = {};
  try { data = event.data ? event.data.json() : {}; } catch {}
  const title = data.title || "claws";
  const body = data.body || "";
  const sid = data.session_id || "";
  const tag = sid ? `claws:${sid}` : "claws";
  event.waitUntil(self.registration.showNotification(title, {
    body,
    tag,
    renotify: true,
    icon: "/icon.svg",
    badge: "/icon.svg",
    data: { session_id: sid, kind: data.kind || "" },
  }));
});

self.addEventListener("notificationclick", (event) => {
  event.notification.close();
  const sid = event.notification.data && event.notification.data.session_id;
  const url = sid ? `/?session=${encodeURIComponent(sid)}` : "/";
  event.waitUntil((async () => {
    const all = await self.clients.matchAll({ type: "window", includeUncontrolled: true });
    // If a claws tab is open, focus it and post the deep-link.
    for (const c of all) {
      if (c.url && new URL(c.url).origin === location.origin) {
        c.postMessage({ kind: "deeplink", session_id: sid });
        return c.focus();
      }
    }
    return self.clients.openWindow(url);
  })());
});
