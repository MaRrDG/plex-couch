'use strict';
// Plex Couch service worker: caches the app shell so the app opens even when the PC is off.
// /api/* is never cached (always network); the page shows "PC-ul nu răspunde" when it fails.
// Posters (/img/*) are cached first-come in their own small cache, so they show offline too.

const VERSION = 'plex-couch-v3';
const IMG_CACHE = 'plex-couch-img-v1';
const IMG_MAX = 150;
const SHELL = [
  '/',
  '/app.js',
  '/style.css',
  '/tokens.css',
  '/fonts/figtree-latin.woff2',
  '/fonts/figtree-latin-ext.woff2',
  '/manifest.webmanifest',
  '/icon.svg',
  '/icon-192.png',
  '/icon-512.png',
  '/icon-maskable-512.png',
  '/apple-touch-icon.png',
];

self.addEventListener('install', (event) => {
  event.waitUntil(
    caches.open(VERSION)
      .then((cache) => cache.addAll(SHELL.map((u) => new Request(u, { cache: 'reload' }))))
      .then(() => self.skipWaiting()),
  );
});

self.addEventListener('activate', (event) => {
  event.waitUntil(
    caches.keys()
      .then((keys) => Promise.all(keys.filter((k) => k !== VERSION && k !== IMG_CACHE).map((k) => caches.delete(k))))
      .then(() => self.clients.claim()),
  );
});

// Keep the poster cache small: drop the oldest entries beyond IMG_MAX.
async function trimImages(cache) {
  const keys = await cache.keys();
  for (let i = 0; i < keys.length - IMG_MAX; i++) await cache.delete(keys[i]);
}

function posterFromCacheFirst(event, url) {
  event.respondWith(
    caches.open(IMG_CACHE).then(async (cache) => {
      const cached = await cache.match(url.pathname);
      if (cached) return cached;
      let res;
      try {
        res = await fetch(event.request);
      } catch (e) {
        return new Response('', { status: 504 });
      }
      // Only real images; a 401 or 404 must not stick.
      const type = res.headers.get('Content-Type') || '';
      if (res.ok && res.status === 200 && type.startsWith('image/')) {
        event.waitUntil(cache.put(url.pathname, res.clone()).then(() => trimImages(cache)));
      }
      return res;
    }),
  );
}

self.addEventListener('fetch', (event) => {
  const req = event.request;
  if (req.method !== 'GET') return;
  const url = new URL(req.url);
  if (url.origin !== self.location.origin) return;
  if (url.pathname.startsWith('/api/')) return; // network only, never cached
  if (url.pathname.startsWith('/img/')) return posterFromCacheFirst(event, url);

  const key = req.mode === 'navigate' ? '/' : url.pathname;
  if (!SHELL.includes(key)) return;

  // Stale-while-revalidate: answer from the cache at once (instant start, works offline),
  // refresh the cached copy in the background for the next launch.
  event.respondWith(
    caches.open(VERSION).then(async (cache) => {
      const cached = await cache.match(key);
      const network = fetch(key, { cache: 'no-cache' })
        .then((res) => {
          if (res && res.ok) cache.put(key, res.clone());
          return res;
        })
        .catch(() => null);
      if (cached) {
        event.waitUntil(network);
        return cached;
      }
      const res = await network;
      return res || new Response('PC-ul nu răspunde.', {
        status: 503, headers: { 'Content-Type': 'text/plain; charset=utf-8' },
      });
    }),
  );
});
