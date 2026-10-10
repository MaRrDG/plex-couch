'use strict';

(function () {
  const POLL_MS = 5000;
  const TIMEOUT_MS = 8000;
  const PIN_LEN = 6;
  const PIN_MIN = 4;
  const CSRF = { 'X-Requested-With': 'plex-couch' };
  const STORE_KEY = 'plex-couch-last';
  const TABS = ['acasa', 'filme', 'pc'];
  const SERVICES = [
    ['plex', 'Plex'], ['radarr', 'Radarr'], ['prowlarr', 'Prowlarr'],
    ['bazarr', 'Bazarr'], ['qbittorrent', 'qBittorrent'],
  ];
  const OFFLINE_TEXT = 'PC-ul nu răspunde. Verifică dacă e pornit.';
  const reducedMotion = window.matchMedia('(prefers-reduced-motion: reduce)');

  const $ = (id) => document.getElementById(id);
  let timer = null;
  let last = null;
  let offline = false;
  let clockOffset = 0;
  let pendingPower = null;
  let afterMovie = null;
  let countdownTimer = null;
  let segment = 'lib';

  // ---------- formatting (Romanian: decimal comma, tabular figures in CSS) ----------
  const nf = (d) => new Intl.NumberFormat('ro-RO', { minimumFractionDigits: d, maximumFractionDigits: d });
  const n0 = nf(0), n1 = nf(1), n2 = nf(2);
  const NBSP = ' ';
  const hhmm = (d) => d.toLocaleTimeString('ro-RO', { hour: '2-digit', minute: '2-digit' });
  function clock(sec) {
    const s = Math.max(0, Math.floor(sec || 0));
    const h = Math.floor(s / 3600), m = Math.floor((s % 3600) / 60), r = s % 60;
    const ss = String(r).padStart(2, '0');
    return h ? `${h}:${String(m).padStart(2, '0')}:${ss}` : `${m}:${ss}`;
  }
  function waitText(sec) {
    if (sec < 60) return `${Math.ceil(sec)} s`;
    return `${Math.ceil(sec / 60)} min`;
  }
  function uptime(s) {
    if (s == null) return '';
    const d = Math.floor(s / 86400), h = Math.floor((s % 86400) / 3600), m = Math.floor((s % 3600) / 60);
    if (d) return `${d} ${d === 1 ? 'zi' : 'zile'} și ${h} h`;
    return h ? `${h} h și ${m} min` : `${m} min`;
  }
  const down = (name) => `${name} nu răspunde. Repornește PC-ul sau verifică serviciul.`;

  // ---------- DOM helpers ----------
  function h(tag, cls, ...children) {
    const e = document.createElement(tag);
    if (cls) e.className = cls;
    for (const c of children) {
      if (c == null || c === false) continue;
      e.appendChild(typeof c === 'string' ? document.createTextNode(c) : c);
    }
    return e;
  }
  const fill = (node, ...nodes) => node.replaceChildren(...nodes.filter(Boolean));
  function bar(percent, tone) {
    const v = Math.max(0, Math.min(100, Number(percent) || 0));
    const b = h('div', 'bar' + (tone ? ' bar-' + tone : ''));
    b.setAttribute('role', 'progressbar');
    b.setAttribute('aria-valuemin', '0');
    b.setAttribute('aria-valuemax', '100');
    b.setAttribute('aria-valuenow', String(Math.round(v)));
    const i = h('i');
    i.style.width = v.toFixed(1) + '%';
    b.appendChild(i);
    return b;
  }
  const card = (text, warn) => h('div', 'card body empty' + (warn ? ' warn' : ''), text);

  // ---------- icons: 24px line glyphs, 1.8px stroke, currentColor ----------
  const ICONS = {
    home: '<path d="M4 10.5 12 4l8 6.5V20h-5.5v-5.5h-5V20H4z" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linejoin="round"/>',
    film: '<rect x="4" y="5" width="16" height="14" rx="2.5" fill="none" stroke="currentColor" stroke-width="1.8"/><path d="M8 5v14M16 5v14M4 9.5h4M4 14.5h4M16 9.5h4M16 14.5h4" stroke="currentColor" stroke-width="1.8"/>',
    pc: '<rect x="3.5" y="4.5" width="17" height="11.5" rx="2" fill="none" stroke="currentColor" stroke-width="1.8"/><path d="M9 20h6M12 16v4" stroke="currentColor" stroke-width="1.8" stroke-linecap="round"/>',
    moff: '<rect x="3.5" y="4.5" width="17" height="11.5" rx="2" fill="none" stroke="currentColor" stroke-width="1.8"/><path d="M9 20h6M12 16v4M4 4l16 13" stroke="currentColor" stroke-width="1.8" stroke-linecap="round"/>',
    mon: '<rect x="3.5" y="4.5" width="17" height="11.5" rx="2" fill="none" stroke="currentColor" stroke-width="1.8"/><path d="M9 20h6M12 16v4M12 7.5v1.2M12 12.6v1.2M8.6 10.3h1.2M14.2 10.3h1.2" stroke="currentColor" stroke-width="1.8" stroke-linecap="round"/>',
    auto: '<path d="M8 6.5v11l9-5.5z" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linejoin="round"/><path d="M3.5 12a8.5 8.5 0 0 1 2.2-5.7M20.5 12a8.5 8.5 0 0 1-2.2 5.7" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" fill="none"/>',
    power: '<path d="M12 3.5v8" stroke="currentColor" stroke-width="2" stroke-linecap="round"/><path d="M7.2 6.6a7.5 7.5 0 1 0 9.6 0" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"/>',
  };
  const SVG_NS = 'http://www.w3.org/2000/svg';
  function icon(name) {
    const svg = document.createElementNS(SVG_NS, 'svg');
    svg.setAttribute('class', 'ic');
    svg.setAttribute('viewBox', '0 0 24 24');
    svg.setAttribute('aria-hidden', 'true');
    svg.innerHTML = ICONS[name]; // constant markup, never data
    return svg;
  }
  for (const node of document.querySelectorAll('[data-icon]')) node.prepend(icon(node.dataset.icon));

  // ---------- posters: real image over a drawn placeholder ----------
  // Placeholder art (the only colours outside tokens.css): a gradient, a soft circle, the title.
  const ART = [
    ['#0B0A09', '#3A2A1A', '#E8DCC4', true], ['#C9772E', '#3B1D10', '#FFF1DE', false],
    ['#2E4A66', '#B9C6D2', '#0F1B27', true], ['#D9D4CA', '#7A7468', '#1C1B19', true],
    ['#0D2233', '#5E7F86', '#EAF1F2', true], ['#3E5A3A', '#C9D3B0', '#14200F', true],
    ['#4A1F2C', '#C98A7A', '#FBEDE6', false], ['#1F1B3A', '#6F6AA8', '#EEEAFB', true],
  ];
  let artId = 0;
  function hashText(s) {
    let x = 0;
    for (const ch of s) x = (x * 31 + ch.codePointAt(0)) >>> 0;
    return x;
  }
  function svgEl(tag, attrs) {
    const e = document.createElementNS(SVG_NS, tag);
    for (const [k, v] of Object.entries(attrs)) e.setAttribute(k, String(v));
    return e;
  }
  function drawnPoster(title) {
    const [top, bottom, ink, serif] = ART[hashText(title || '') % ART.length];
    const id = 'pg' + (artId++);
    const svg = svgEl('svg', { viewBox: '0 0 100 150', 'aria-hidden': 'true', preserveAspectRatio: 'xMidYMid slice' });
    const grad = svgEl('linearGradient', { id, x1: 0, y1: 0, x2: 0, y2: 1 });
    grad.append(svgEl('stop', { offset: 0, 'stop-color': top }), svgEl('stop', { offset: 1, 'stop-color': bottom }));
    const defs = svgEl('defs', {});
    defs.appendChild(grad);
    svg.append(defs, svgEl('rect', { width: 100, height: 150, fill: `url(#${id})` }),
      svgEl('circle', { cx: 50, cy: 56, r: 22, fill: ink, opacity: 0.14 }));
    const words = String(title || '').toLocaleUpperCase('ro-RO').split(/\s+/).filter(Boolean);
    const lines = [];
    let cur = '';
    for (const w of words) {
      if (cur && (cur + ' ' + w).length > 10) { lines.push(cur); cur = w; } else cur = (cur + ' ' + w).trim();
    }
    if (cur) lines.push(cur);
    const shown = lines.slice(0, 4);
    // Fit the longest line into ~88 of the 100 units (uppercase glyphs are about 0.72em wide).
    const longest = Math.max(1, ...shown.map((l) => l.length));
    const fs = Math.max(8, Math.min(17, (88 / longest - 1) / 0.72));
    const y0 = 110 - (shown.length - 1) * fs * 0.55;
    shown.forEach((line, i) => {
      const t = svgEl('text', {
        x: 50, y: (y0 + i * fs * 1.1).toFixed(1), 'text-anchor': 'middle', 'font-size': fs.toFixed(1), 'font-weight': 700,
        'letter-spacing': 1, fill: ink, 'font-family': serif ? 'Georgia, serif' : 'Arial Black, Arial, sans-serif',
      });
      t.textContent = line;
      svg.appendChild(t);
    });
    return svg;
  }
  // One <img> per URL, reused across renders so a poll never reloads or flickers a poster.
  const images = new Map();
  function posterImg(url) {
    if (!url || !/^\/img\/(movie|plex|movie-art|plex-art)\/\d+$/.test(url)) return null;
    let img = images.get(url);
    if (!img) {
      img = new Image();
      img.alt = '';
      img.decoding = 'async';
      img.addEventListener('error', () => { img.dataset.failed = '1'; img.remove(); });
      img.src = url;
      images.set(url, img);
    }
    return img.dataset.failed ? null : img;
  }
  function poster(url, title, size, badge) {
    const p = h('span', 'poster poster-' + size);
    p.appendChild(drawnPoster(title));
    const img = posterImg(url);
    if (img) p.appendChild(img);
    if (badge) p.appendChild(badge);
    return p;
  }

  // ---------- toast ----------
  let toastTimer = null;
  function toast(text, isErr) {
    const t = $('toast');
    fill(t, isErr ? h('i', 'dot dot-danger') : null, h('span', '', text));
    t.hidden = false;
    clearTimeout(toastTimer);
    toastTimer = setTimeout(() => { t.hidden = true; }, 3000);
  }

  // ---------- local copy of the last status (shown while the PC is off) ----------
  function saveLast(s) { try { localStorage.setItem(STORE_KEY, JSON.stringify(s)); } catch (e) { /* ignore */ } }
  function loadLast() {
    try { return JSON.parse(localStorage.getItem(STORE_KEY) || 'null'); } catch (e) { return null; }
  }
  function forgetLast() { try { localStorage.removeItem(STORE_KEY); } catch (e) { /* ignore */ } }

  // ---------- network ----------
  // err.offline = the PC did not answer (network error or timeout).
  async function api(path, body) {
    const ctrl = new AbortController();
    const to = setTimeout(() => ctrl.abort(), TIMEOUT_MS);
    const opts = { credentials: 'same-origin', cache: 'no-store', headers: {}, signal: ctrl.signal };
    if (body !== undefined) {
      opts.method = 'POST';
      opts.headers = Object.assign({ 'Content-Type': 'application/json' }, CSRF);
      opts.body = JSON.stringify(body);
    }
    let r;
    try {
      r = await fetch(path, opts);
    } catch (e) {
      const err = new Error(OFFLINE_TEXT);
      err.offline = true;
      throw err;
    } finally {
      clearTimeout(to);
    }
    let data = null;
    try { data = await r.json(); } catch (e) { data = null; }
    if (r.status === 503 && !data) { // the service worker's answer while the PC is off
      const err = new Error(OFFLINE_TEXT);
      err.offline = true;
      throw err;
    }
    setOffline(false);
    if (r.status === 401 && path !== '/api/login') {
      forgetLast();
      showLogin();
      throw new Error('auth');
    }
    if (!r.ok) {
      const err = new Error((data && data.error) || `Eroare ${r.status}. Reîncearcă.`);
      err.data = data;
      throw err;
    }
    return data;
  }

  // ---------- enabled / disabled controls ----------
  const busy = new Set();
  function applyDisabled() {
    const off = offline;
    for (const id of ['mon-off', 'mon-on', 'auto-off', 'logout', 'countdown-cancel', 'after-movie-cancel',
      'notify-test']) $(id).disabled = off || busy.has(id);
    for (const id of ['tile-shutdown', 'btn-shutdown', 'btn-restart']) $(id).disabled = off || !!pendingPower;
    $('sheet-confirm').disabled = off;
    $('sheet-after').disabled = off;
  }
  function setOffline(on) {
    if (offline === on) return;
    offline = on;
    $('offline').hidden = !on;
    document.body.classList.toggle('is-offline', on);
    if (on) closeSheet(true);
    applyDisabled();
    if (last) renderFooter(last);
  }

  // ---------- login (PinPad) ----------
  let pin = '';
  let lockUntil = 0;
  let lockTimer = null;
  let loginBusy = false;

  function drawDots() {
    const dots = $('pin-dots').children;
    for (let i = 0; i < dots.length; i++) dots[i].classList.toggle('on', i < pin.length);
    $('pin-dots').setAttribute('aria-label', `${pin.length} din ${PIN_LEN} cifre`);
    const canSubmit = pin.length >= PIN_MIN && pin.length < PIN_LEN;
    $('key-ok').hidden = !canSubmit;
    $('key-gap').hidden = canSubmit;
  }
  function setKeypad(enabled) {
    for (const b of $('keypad').querySelectorAll('button')) b.disabled = !enabled;
  }
  function shake() {
    if (reducedMotion.matches) return;
    const d = $('pin-dots');
    d.classList.remove('shake');
    void d.offsetWidth;
    d.classList.add('shake');
  }
  function startLockout(seconds) {
    lockUntil = Date.now() + seconds * 1000;
    setKeypad(false);
    clearInterval(lockTimer);
    const tick = () => {
      const left = (lockUntil - Date.now()) / 1000;
      if (left <= 0) {
        clearInterval(lockTimer);
        setKeypad(true);
        $('login-msg').textContent = '';
        return;
      }
      $('login-msg').textContent = `Prea multe încercări. Încearcă din nou peste ${waitText(left)}.`;
    };
    tick();
    lockTimer = setInterval(tick, 1000);
  }
  async function submitPin() {
    if (loginBusy || pin.length < PIN_MIN || Date.now() < lockUntil) return;
    loginBusy = true;
    const tryPin = pin;
    try {
      await api('/api/login', { pin: tryPin });
      pin = '';
      drawDots();
      $('login-msg').textContent = '';
      showApp();
    } catch (e) {
      pin = '';
      drawDots();
      shake();
      const d = e.data || {};
      if (d.lockout_seconds) startLockout(d.lockout_seconds);
      else if (typeof d.attempts_left === 'number') {
        const n = d.attempts_left;
        $('login-msg').textContent = `PIN greșit. Mai ai ${n} ${n === 1 ? 'încercare' : 'încercări'}.`;
      } else {
        $('login-msg').textContent = e.message || OFFLINE_TEXT;
      }
    } finally {
      loginBusy = false;
    }
  }
  function pressKey(k) {
    if (Date.now() < lockUntil || loginBusy) return;
    if (k === 'del') pin = pin.slice(0, -1);
    else if (k === 'ok') return submitPin();
    else if (/^\d$/.test(k) && pin.length < PIN_LEN) pin += k;
    if (pin.length && $('login-msg').textContent.startsWith('PIN greșit')) $('login-msg').textContent = '';
    drawDots();
    if (pin.length === PIN_LEN) submitPin();
  }
  $('keypad').addEventListener('click', (e) => {
    const b = e.target.closest('button[data-k]');
    if (b) pressKey(b.dataset.k);
  });
  document.addEventListener('keydown', (e) => {
    if ($('login').hidden) return;
    let k = null;
    if (/^\d$/.test(e.key)) k = e.key;
    else if (e.key === 'Backspace') k = 'del';
    else if (e.key === 'Enter') k = 'ok';
    if (!k) return;
    e.preventDefault();
    const b = $('keypad').querySelector(`.key[data-k="${k}"]`);
    if (b) { b.classList.add('pressed'); setTimeout(() => b.classList.remove('pressed'), 120); }
    pressKey(k);
  });

  function showLogin(msg) {
    stopPolling();
    closeSheet(true);
    setOffline(false);
    $('app').hidden = true;
    $('login').hidden = false;
    pin = '';
    drawDots();
    if (msg) $('login-msg').textContent = msg;
  }
  function showApp() {
    $('login').hidden = true;
    $('app').hidden = false;
    route();
    refresh();
  }

  // ---------- tabs (hash routes) ----------
  function currentTab() {
    const t = location.hash.slice(1);
    return TABS.includes(t) ? t : 'acasa';
  }
  function route() {
    const tab = currentTab();
    for (const v of document.querySelectorAll('.view')) v.hidden = v.dataset.view !== tab;
    for (const a of document.querySelectorAll('.tabbar a')) {
      if (a.dataset.tab === tab) a.setAttribute('aria-current', 'page');
      else a.removeAttribute('aria-current');
    }
  }
  window.addEventListener('hashchange', () => {
    if (location.hash === '#login') return showLogin();
    route();
    window.scrollTo(0, 0);
  });

  // SegmentedControl: Bibliotecă / Urmează
  function setSegment(which, focus) {
    segment = which;
    for (const [id, panel, key] of [['seg-lib', 'panel-lib', 'lib'], ['seg-next', 'panel-next', 'next']]) {
      const on = key === which;
      $(id).setAttribute('aria-selected', String(on));
      $(id).tabIndex = on ? 0 : -1;
      $(panel).hidden = !on;
      if (on && focus) $(id).focus();
    }
  }
  $('seg-lib').addEventListener('click', () => setSegment('lib'));
  $('seg-next').addEventListener('click', () => setSegment('next'));
  $('seg').addEventListener('keydown', (e) => {
    if (e.key === 'ArrowLeft' || e.key === 'ArrowRight') {
      e.preventDefault();
      setSegment(segment === 'lib' ? 'next' : 'lib', true);
    }
  });

  // ---------- polling ----------
  function stopPolling() { clearTimeout(timer); timer = null; }
  function schedule() {
    stopPolling();
    if (document.visibilityState === 'visible') timer = setTimeout(refresh, POLL_MS);
  }
  async function refresh() {
    stopPolling();
    try {
      const s = await api('/api/status');
      $('login').hidden = true;
      $('app').hidden = false;
      render(s);
      saveLast(s);
    } catch (e) {
      if (e.message === 'auth') return;
      if (e.offline) {
        $('login').hidden = true;
        $('app').hidden = false;
        setOffline(true);
        if (!last) {
          const cached = loadLast();
          if (cached) render(cached, true);
          else renderEmpty();
        }
      } else if (last) {
        $('updated').textContent = e.message;
      }
    }
    schedule();
  }
  document.addEventListener('visibilitychange', () => {
    if (document.visibilityState === 'visible') { if (!$('app').hidden) refresh(); } else stopPolling();
  });
  window.addEventListener('online', () => { if (!$('app').hidden) refresh(); });

  // ---------- render ----------
  function render(s, fromCache) {
    s._at = fromCache ? (s._at || null) : Date.now();
    last = s;
    if (!fromCache) clockOffset = (s.server_time || Date.now() / 1000) - Date.now() / 1000;
    const svc = s.services || {};
    renderPlaying(s.now_playing, svc);
    renderAuto(s.settings);
    renderDownload(s.downloads);
    renderLibrary(s.library, svc);
    renderNext(s.waiting, s.downloads);
    renderSeed(s.torrents);
    renderDisk(s.pc || {});
    renderServices(svc);
    renderNotify(s.notifications);
    renderFooter(s);
    setPower(fromCache ? null : s.power);
    setAfterMovie(fromCache ? null : s.power && s.power.after_movie);
  }

  function renderEmpty() {
    const none = () => card('Fără date de la PC.');
    fill($('playing'), none());
    $('playing-device').textContent = '';
    $('sec-download').hidden = true;
    fill($('panel-lib'), none());
    fill($('panel-next'), none());
    fill($('seed'), none());
    fill($('disk'), none());
    $('disk-meta').textContent = '';
    $('services-meta').textContent = '';
    fill($('services'), ...SERVICES.map(([, label]) => h('span', 'chip', label)));
    $('updated').textContent = '';
    $('pc-line').hidden = true;
  }

  function renderPlaying(list, svc) {
    const dev = $('playing-device');
    dev.textContent = '';
    if (list == null || (!list.length && !svc.plex)) return fill($('playing'), card(down('Plex'), true));
    if (!list.length) return fill($('playing'), card('Nimic pe TV acum.'));
    if (list.length === 1) dev.textContent = list[0].player || '';
    fill($('playing'), ...list.map((p) => {
      const sub = [];
      if (p.year) sub.push(String(p.year));
      if (p.state && p.state !== 'playing') sub.push(p.state_ro || p.state);
      if (p.decision_ro) sub.push(p.decision_ro);
      if (list.length > 1 && p.player) sub.push(p.player);
      // Backdrop: blurred art under a scrim; if the image fails the card is plain surface.
      const art = posterImg(p.backdrop);
      return h('div', 'np',
        art ? h('div', 'np-backdrop', art) : null,
        poster(p.poster, p.title, 'hero'),
        h('div', 'np-body',
          h('span', 'title-card np-title', p.title),
          h('span', 'meta muted', sub.join(', ')),
          bar(p.progress),
          h('div', 'np-times data', h('span', '', clock(p.position)), h('span', '', clock(p.duration)))));
    }));
  }

  let autoBusy = false;
  function renderAuto(settings) {
    if (autoBusy) return;
    setAutoTile(!!(settings && settings.auto_monitors_off));
  }
  function setAutoTile(on) {
    $('auto-off').setAttribute('aria-pressed', String(on));
    $('auto-off-state').textContent = on ? 'Pornit' : 'Oprit';
  }

  function downloadText(d) {
    const parts = [`${n0.format(d.percent || 0)}%`];
    if (d.time_left) parts.push(d.time_left);
    const st = (d.status || '').toLocaleLowerCase('ro-RO');
    if (st && st !== 'se descarcă') parts.push(st);
    return parts.join(', ');
  }
  function renderDownload(list) {
    const sec = $('sec-download');
    if (!list || !list.length) { sec.hidden = true; return; }
    sec.hidden = false;
    const d = list[0];
    $('download-meta').textContent = downloadText(d);
    const more = list.length > 1
      ? h('p', 'meta muted', `Încă ${list.length - 1} în Filme, la Urmează.`) : null;
    fill($('download'),
      h('div', 'mini', poster(d.poster, d.title, 'mini'),
        h('div', 'mini-body', h('span', 'tile-label', d.title), bar(d.percent))),
      more);
  }

  function posterCard(m, sub, badge) {
    return h('div', 'pcard',
      poster(m.poster, m.title, 'grid', badge),
      h('span', 'data pcard-title', m.title),
      h('span', 'data pcard-sub', sub));
  }
  function renderLibrary(list, svc) {
    const panel = $('panel-lib');
    if (list == null) return fill(panel, card(down('Radarr'), true));
    if (!list.length) return fill(panel, card('Biblioteca e goală. Filmele descărcate apar aici.'));
    const grid = h('div', 'grid', ...list.map((m) => {
      let badge = null;
      if (m.ro_subs === true) badge = h('span', 'badge poster-badge', 'RO');
      else if (m.ro_subs === false) badge = h('span', 'badge poster-badge missing', 'fără RO');
      return posterCard(m, `${n1.format(m.size_gb || 0)} GB`, badge);
    }));
    const note = svc.bazarr ? null
      : h('p', 'meta muted panel-note', 'Bazarr nu răspunde, așa că nu știu ce subtitrări există.');
    fill(panel, grid, note);
  }
  function renderNext(list, downloads) {
    const panel = $('panel-next');
    if (list == null) return fill(panel, card(down('Radarr'), true));
    if (!list.length) return fill(panel, card('Nimic în așteptare. Adaugă un film în Watchlist-ul Plex.'));
    const byId = new Map((downloads || []).map((d) => [d.radarr_id != null ? d.radarr_id : d.movie_id, d]));
    const items = list.map((m) => {
      const d = byId.get(m.radarr_id);
      if (d) return { m, sub: `Se descarcă, ${n0.format(d.percent || 0)}%`, rank: 0 };
      return { m, sub: m.status, rank: m.downloading ? 0 : 1 };
    });
    items.sort((a, b) => a.rank - b.rank);
    fill(panel, h('div', 'grid', ...items.map(({ m, sub }) => posterCard(m, sub, null))));
  }

  function seedRow(t) {
    let value, pct, tone = '';
    if (t.hr === 'downloading') {
      value = `se descarcă, ${n0.format(t.progress || 0)}%`;
      pct = t.progress;
    } else if (t.hr === 'safe') {
      value = 'gata';
      pct = 100;
      tone = 'ok';
    } else {
      value = `ratio ${n2.format(t.ratio || 0)}, ${n0.format(Math.min(48, Math.floor(t.seed_hours || 0)))} din 48 h`;
      pct = (t.rule_progress || 0) * 100;
    }
    return h('div', 'li',
      h('div', 'li-head', h('span', 'tile-label', t.title || ''), h('span', 'data', value)),
      bar(pct, tone));
  }
  function renderSeed(list) {
    const box = $('seed');
    if (list == null) return fill(box, card(down('qBittorrent'), true));
    if (!list.length) return fill(box, card('Niciun torrent FileList la seed.'));
    fill(box, h('div', 'group', ...list.map(seedRow)));
  }

  function renderDisk(pc) {
    const disks = pc.disks || [];
    const main = disks.find((d) => d.drive === 'D:') || disks[0];
    const meta = $('disk-meta');
    if (!main) {
      $('h-disk').textContent = 'Spațiu';
      meta.textContent = '';
      return fill($('disk'), card('Nu pot citi discurile.'));
    }
    $('h-disk').textContent = `Spațiu pe ${main.drive}`;
    meta.textContent = `${n0.format(main.free_gb)} GB liberi`;
    const other = disks.filter((d) => d !== main)
      .map((d) => `${d.drive} ${n0.format(d.free_gb)} GB liberi`).join(', ');
    fill($('disk'), h('div', 'disk',
      bar(main.used_pct),
      h('div', 'bar-line data', h('span', '', other), h('span', '', `din ${n0.format(main.total_gb)} GB`))));
  }

  function renderServices(svc) {
    const downCount = SERVICES.filter(([k]) => !svc[k]).length;
    $('services-meta').textContent = downCount === 0 ? 'toate pornite'
      : `${downCount} ${downCount === 1 ? 'oprit' : 'oprite'}`;
    fill($('services'), ...SERVICES.map(([k, label]) => {
      const up = !!svc[k];
      return h('span', 'chip' + (up ? '' : ' chip-down'),
        h('i', 'dot' + (up ? '' : ' dot-danger')), label,
        up ? null : h('span', 'chip-word', 'oprit'));
    }));
  }

  // ---------- Notificări (ntfy) ----------
  function renderNotify(n) {
    const on = !!(n && n.enabled && n.topic);
    $('notify-off').hidden = on || !n;
    $('notify-on').hidden = !on;
    // Only touch the text when it changes, so a selection made for copying survives the poll.
    if (on && $('notify-topic').textContent !== n.topic) $('notify-topic').textContent = n.topic;
  }
  function selectTopic() {
    const range = document.createRange();
    range.selectNodeContents($('notify-topic'));
    const sel = window.getSelection();
    sel.removeAllRanges();
    sel.addRange(range);
  }
  $('notify-copy').addEventListener('click', async () => {
    const topic = $('notify-topic').textContent;
    if (!topic) return;
    try {
      if (!navigator.clipboard || !window.isSecureContext) throw new Error('no clipboard');
      await navigator.clipboard.writeText(topic);
      toast('Numele a fost copiat.');
    } catch (e) {
      selectTopic();
      let copied = false;
      try { copied = document.execCommand('copy'); } catch (err) { copied = false; }
      toast(copied ? 'Numele a fost copiat.' : 'Numele e selectat. Copiază-l din meniul telefonului.');
    }
  });
  $('notify-test').addEventListener('click', () =>
    action('notify-test', '/api/notify/test', 'Notificarea de test a fost trimisă.'));

  function renderFooter(s) {
    const at = s._at ? hhmm(new Date(s._at)) : '';
    $('updated').textContent = offline ? (at ? `Ultima actualizare la ${at}` : '') : `Actualizat la ${at}`;
    const pc = s.pc || {};
    const parts = [];
    if (pc.hostname) parts.push(`${pc.hostname}, pornit de ${uptime(pc.uptime)}`);
    if (s.demo) parts.push('Mod demo: date de test, comenzile nu se execută.');
    else if (s.dry_run) parts.push('Mod de probă: comenzile doar se notează în jurnal.');
    $('pc-line').hidden = !parts.length;
    $('pc-line').textContent = parts.join('. ');
  }

  // ---------- power: CountdownBanner ----------
  function nowSec() { return Date.now() / 1000 + clockOffset; }
  function setPower(p) {
    pendingPower = p && p.kind ? { kind: p.kind, at: p.at } : null;
    const on = !!pendingPower;
    $('countdown').hidden = !on;
    applyDisabled();
    if (!on) {
      clearInterval(countdownTimer);
      countdownTimer = null;
      return;
    }
    const restart = pendingPower.kind === 'restart';
    $('countdown-label').textContent = restart ? 'PC-ul repornește în' : 'PC-ul se oprește în';
    $('countdown-cancel').textContent = restart ? 'Anulează repornirea' : 'Anulează oprirea';
    const tick = () => {
      if (!pendingPower) return;
      const left = Math.max(0, pendingPower.at - nowSec());
      $('countdown-secs').textContent = `${Math.ceil(left)}${NBSP}s`;
      $('countdown-bar').style.width = (Math.min(1, left / 60) * 100).toFixed(1) + '%';
    };
    tick();
    if (!countdownTimer) countdownTimer = setInterval(tick, 1000);
  }
  $('countdown-cancel').addEventListener('click', async () => {
    const restart = pendingPower && pendingPower.kind === 'restart';
    busy.add('countdown-cancel');
    applyDisabled();
    try {
      await api('/api/power/cancel', {});
      setPower(null);
      toast(restart ? 'Repornirea a fost anulată.' : 'Oprirea a fost anulată.');
    } catch (e) {
      if (e.message !== 'auth') toast(e.message, true);
    } finally {
      busy.delete('countdown-cancel');
      applyDisabled();
    }
  });

  // ---------- power: after the movie (banner without a countdown) ----------
  function setAfterMovie(a) {
    afterMovie = a && a.armed ? a : null;
    $('after-movie').hidden = !afterMovie;
    if (afterMovie) $('after-movie-text').textContent = `PC-ul se oprește după ${afterMovie.title || 'film'}.`;
  }
  function tvMovie() {
    return ((last && last.now_playing) || []).find((p) => p.on_tv) || null;
  }
  async function armAfterMovie() {
    closeSheet();
    try {
      const r = await api('/api/power/after-movie', {});
      setAfterMovie(r.power && r.power.after_movie);
      if (afterMovie) $('after-movie-cancel').focus({ preventScroll: true });
      window.scrollTo({ top: 0, behavior: reducedMotion.matches ? 'auto' : 'smooth' });
      toast('PC-ul se oprește după film.');
    } catch (e) {
      if (e.message !== 'auth') toast(e.message, true);
    }
  }
  $('after-movie-cancel').addEventListener('click', async () => {
    busy.add('after-movie-cancel');
    applyDisabled();
    try {
      await api('/api/power/after-movie/cancel', {});
      setAfterMovie(null);
      toast('Oprirea după film a fost anulată.');
    } catch (e) {
      if (e.message !== 'auth') toast(e.message, true);
    } finally {
      busy.delete('after-movie-cancel');
      applyDisabled();
    }
  });

  // ---------- ActionSheet ----------
  let lastFocus = null;
  let sheetKind = null;
  let sheetTimer = null;
  function openSheet(kind) {
    if (pendingPower || offline) return;
    clearTimeout(sheetTimer);
    sheetKind = kind;
    const restart = kind === 'restart';
    $('sheet-title').textContent = restart ? 'Repornești PC-ul?' : 'Oprești PC-ul?';
    $('sheet-text').textContent = restart
      ? 'Repornește în 60 de secunde. Poți anula până atunci.'
      : 'Se oprește în 60 de secunde. Poți anula până atunci.';
    $('sheet-confirm').textContent = restart ? 'Repornește PC-ul' : 'Oprește PC-ul';
    // Second option while a movie plays on the TV: shut down when it ends.
    const movie = !restart && !afterMovie ? tvMovie() : null;
    $('sheet-after-wrap').hidden = !movie;
    if (movie) {
      $('sheet-after-meta').textContent =
        `Când se termină ${movie.title}. Dacă îl oprești mai devreme, PC-ul rămâne pornit.`;
    }
    const warns = (last && last.hr_warnings) || [];
    $('sheet-warn').hidden = !warns.length;
    const lead = warns.length === 1 ? 'Un torrent n-a terminat seed-ul pe FileList.'
      : `${warns.length} torrente n-au terminat seed-ul pe FileList.`;
    $('sheet-warn-text').textContent =
      `${lead} Ai voie să închizi, dar în 7 zile trebuie să ajungă la ratio 1 sau 48${NBSP}h.`;
    fill($('sheet-warn-list'), ...warns.map((w) => h('li', '',
      h('span', 'tile-label', w.title || ''), ' ',
      h('span', 'meta', (w.remaining || '').replace(/ (?=h)|(?<=ratio) /g, NBSP)))));
    lastFocus = document.activeElement;
    $('app').inert = true;
    $('scrim').hidden = false;
    $('sheet').hidden = false;
    if (reducedMotion.matches) {
      $('scrim').classList.add('open');
      $('sheet').classList.add('open');
    } else {
      requestAnimationFrame(() => requestAnimationFrame(() => {
        $('scrim').classList.add('open');
        $('sheet').classList.add('open');
      }));
    }
    $('sheet-title').focus({ preventScroll: true });
  }
  function closeSheet(instant) {
    if ($('sheet').hidden) return;
    $('app').inert = false;
    $('scrim').classList.remove('open');
    $('sheet').classList.remove('open');
    const done = () => { $('scrim').hidden = true; $('sheet').hidden = true; };
    clearTimeout(sheetTimer);
    if (instant || reducedMotion.matches) done();
    else sheetTimer = setTimeout(done, 240);
    if (lastFocus && lastFocus.focus && !lastFocus.disabled) lastFocus.focus({ preventScroll: true });
    lastFocus = null;
  }
  $('tile-shutdown').addEventListener('click', () => openSheet('shutdown'));
  $('btn-shutdown').addEventListener('click', () => openSheet('shutdown'));
  $('btn-restart').addEventListener('click', () => openSheet('restart'));
  $('sheet-cancel').addEventListener('click', () => closeSheet());
  $('scrim').addEventListener('click', () => closeSheet());
  $('sheet-confirm').addEventListener('click', () => power(sheetKind));
  $('sheet-after').addEventListener('click', () => armAfterMovie());
  document.addEventListener('keydown', (e) => {
    if ($('sheet').hidden) return;
    if (e.key === 'Escape') { e.preventDefault(); closeSheet(); return; }
    if (e.key === 'Tab') { // keep focus inside the sheet
      const items = [$('sheet-confirm'), $('sheet-after'), $('sheet-cancel')]
        .filter((b) => !b.disabled && !b.closest('[hidden]'));
      const i = items.indexOf(document.activeElement);
      e.preventDefault();
      const next = i < 0 ? (e.shiftKey ? items.length - 1 : 0) : (i + (e.shiftKey ? items.length - 1 : 1)) % items.length;
      items[next].focus();
    }
  });

  async function power(kind) {
    closeSheet();
    try {
      const r = await api('/api/power/' + kind, {});
      setPower(r.power);
      if (r.power) $('countdown-cancel').focus({ preventScroll: true });
      window.scrollTo({ top: 0, behavior: reducedMotion.matches ? 'auto' : 'smooth' });
      toast(kind === 'restart' ? 'PC-ul repornește în 60 de secunde.' : 'PC-ul se oprește în 60 de secunde.');
    } catch (e) {
      if (e.message !== 'auth') toast(e.message, true);
    }
  }

  // ---------- monitors and settings ----------
  async function action(id, path, okText) {
    busy.add(id);
    applyDisabled();
    try {
      await api(path, {});
      toast(okText);
    } catch (e) {
      if (e.message !== 'auth') toast(e.message, true);
    } finally {
      setTimeout(() => { busy.delete(id); applyDisabled(); }, 500);
    }
  }
  $('mon-off').addEventListener('click', () => action('mon-off', '/api/monitors/off', 'Monitoarele s-au stins.'));
  $('mon-on').addEventListener('click', () => action('mon-on', '/api/monitors/on', 'Monitoarele s-au aprins.'));
  $('auto-off').addEventListener('click', async () => {
    const v = $('auto-off').getAttribute('aria-pressed') !== 'true';
    autoBusy = true;
    busy.add('auto-off');
    setAutoTile(v);
    applyDisabled();
    try {
      await api('/api/settings', { auto_monitors_off: v });
      toast(v ? 'Monitoarele se vor stinge la play pe TV.' : 'Monitoarele nu se mai sting la play.');
    } catch (err) {
      setAutoTile(!v);
      if (err.message !== 'auth') toast(err.message, true);
    } finally {
      autoBusy = false;
      busy.delete('auto-off');
      applyDisabled();
    }
  });
  $('logout').addEventListener('click', async () => {
    try { await api('/api/logout', {}); } catch (e) { /* ignore */ }
    forgetLast();
    last = null;
    showLogin();
  });

  // ---------- service worker (HTTPS / localhost only) ----------
  if ('serviceWorker' in navigator && window.isSecureContext) {
    window.addEventListener('load', () => {
      navigator.serviceWorker.register('/sw.js', { scope: '/' }).catch(() => { /* app works without it */ });
    });
  }

  // ---------- boot ----------
  drawDots();
  route();
  if (location.hash === '#login') showLogin(); // for screenshots of the login screen
  else refresh();
})();
