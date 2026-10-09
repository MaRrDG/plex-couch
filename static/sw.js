'use strict';
// Plex Couch service worker: caches the app shell so the app opens even when the PC is off.
// /api/* is never cached (always network); the page shows "PC-ul nu răspunde" when it fails.

const VERSION = 'plex-couch-v2';
const SHELL = [
  '/',
  '/app.js',
  '/style.css',
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
      .then((keys) => Promise.all(keys.filter((k) => k !== VERSION).map((k) => caches.delete(k))))
      .then(() => self.clients.claim()),
  );
});

self.addEventListener('fetch', (event) => {
  const req = event.request;
  if (req.method !== 'GET') return;
  const url = new URL(req.url);
  if (url.origin !== self.location.origin) return;
  if (url.pathname.startsWith('/api/')) return; // network only, never cached

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
