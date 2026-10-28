'use strict';

(function () {
  const POLL_MS = 5000;
  const TIMEOUT_MS = 8000;
  const PIN_LEN = 6;
  const PIN_MIN = 4;
  const CSRF = { 'X-Requested-With': 'plex-couch' };
  const STORE_KEY = 'plex-couch-last';
  const TABS = ['acasa', 'filme', 'seriale', 'pc'];
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
  let convertBusy = false;
  let subsBusy = false;

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
  // Re-render `node` without losing the focused control (matched by data-key).
  function keepFocus(node, render) {
    const a = document.activeElement;
    const key = a && node.contains(a) ? a.dataset.key : null;
    render();
    if (key) {
      const again = node.querySelector(`[data-key="${CSS.escape(key)}"]`);
      if (again && !again.disabled) again.focus({ preventScroll: true });
    }
  }
  const sr = (text) => h('span', 'sr', text); // read by screen readers only
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
    tv: '<rect x="3.5" y="7.5" width="17" height="12" rx="2" fill="none" stroke="currentColor" stroke-width="1.8"/><path d="M8.5 3.5 12 7.5l3.5-4" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"/>',
    pc: '<rect x="3.5" y="4.5" width="17" height="11.5" rx="2" fill="none" stroke="currentColor" stroke-width="1.8"/><path d="M9 20h6M12 16v4" stroke="currentColor" stroke-width="1.8" stroke-linecap="round"/>',
    back: '<path d="M14.5 5.5 8 12l6.5 6.5" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"/>',
    conv: '<path d="M5.5 12a6.5 6.5 0 0 1 11.3-4.4M18.5 12a6.5 6.5 0 0 1-11.3 4.4" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"/><path d="M17.5 4v3.8h-3.8M6.5 20v-3.8h3.8" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>',
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
    if (!url || !/^\/img\/(movie|plex|series|movie-art|plex-art|series-art)\/\d+$/.test(url)) return null;
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
  function poster(url, title, size, badge, flag) {
    const p = h('span', 'poster poster-' + size);
    p.appendChild(drawnPoster(title));
    const img = posterImg(url);
    if (img) p.appendChild(img);
    if (badge) p.appendChild(badge);
    if (flag) p.appendChild(flag);
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
  async function api(path, body, timeoutMs) {
    const ctrl = new AbortController();
    const to = setTimeout(() => ctrl.abort(), timeoutMs || TIMEOUT_MS);
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
    for (const id of ['mon-off', 'mon-on', 'auto-off', 'logout', 'logout-all', 'countdown-cancel', 'after-movie-cancel',
      'notify-test']) $(id).disabled = off || busy.has(id);
    for (const id of ['tile-shutdown', 'btn-shutdown', 'btn-restart']) $(id).disabled = off || !!pendingPower;
    $('sheet-confirm').disabled = off;
    $('sheet-after').disabled = off;
    // Conversion controls (Seriale, the sheets, the Acasă banner) carry data-net.
    for (const b of document.querySelectorAll('button[data-net]')) b.disabled = off || convertBusy || b.dataset.off === '1';
    // Translation controls (the season action, the sheets) carry data-subs.
    for (const b of document.querySelectorAll('button[data-subs]')) b.disabled = off || subsBusy || b.dataset.off === '1';
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

  // ---------- tabs (hash routes: #acasa, #filme, #seriale, #seriale/<id>, #pc) ----------
  function parseHash() {
    const [t, sub] = location.hash.slice(1).split('/');
    const tab = TABS.includes(t) ? t : 'acasa';
    const id = tab === 'seriale' && /^[1-9]\d{0,9}$/.test(sub || '') ? Number(sub) : null;
    return { tab, id };
  }
  function currentTab() { return parseHash().tab; }
  function route() {
    const { tab, id } = parseHash();
    for (const v of document.querySelectorAll('.view[data-view]')) v.hidden = v.dataset.view !== tab;
    for (const a of document.querySelectorAll('.tabbar a')) {
      if (a.dataset.tab === tab) a.setAttribute('aria-current', 'page');
      else a.removeAttribute('aria-current');
    }
    $('series-list').hidden = id != null;
    $('series-detail').hidden = id == null;
    $('view-seriale').setAttribute('aria-labelledby', id != null ? 't-series' : 't-seriale');
    if (id !== detailId) {
      detailId = id;
      detail = null;
      detailError = null;
      season = null;
      stepCount = null;
    }
    renderSeriesTab();
  }
  let lastRoute = parseHash();
  window.addEventListener('hashchange', () => {
    if (location.hash === '#login') return showLogin();
    const prev = lastRoute;
    const now = parseHash();
    lastRoute = now;
    if (prev.tab === 'seriale' && prev.id == null && now.id != null) gridScroll = window.scrollY;
    if (now.id == null) cameFromGrid = false;
    closeSheet(true);
    route();
    // Back to the grid: where it was; anything else starts at the top.
    window.scrollTo(0, prev.id != null && now.tab === 'seriale' && now.id == null ? gridScroll : 0);
    if (now.id != null) {
      loadDetail(true);
      const head = $('t-series');
      if (head) head.focus({ preventScroll: true });
    } else if (now.tab === 'seriale') loadSeries();
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
      loadConvert();
      if (currentTab() === 'seriale') { if (detailId != null) loadDetail(); else loadSeries(); }
      const movieOpen = movieInfo && !$('info-sheet').hidden;
      if ((currentTab() === 'seriale' && detailId != null) || movieOpen || subsActive()) loadSubs();
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
    if (!seriesList) fill($('series-grid'), none());
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

  // A PosterCard; with `opts.href` a link (Seriale), with `opts.onClick` a button (Filme sheet).
  function posterCard(m, sub, badge, opts) {
    const o = opts || {};
    const tag = o.href ? 'a' : o.onClick ? 'button' : 'div';
    const c = h(tag, 'pcard' + (tag !== 'div' ? ' pcard-tap' : ''),
      poster(m.poster, m.title, 'grid', badge, o.flag),
      h('span', 'data pcard-title', m.title),
      h('span', 'data pcard-sub', sub));
    if (o.href) c.href = o.href;
    if (o.onClick) { c.type = 'button'; c.addEventListener('click', o.onClick); }
    if (o.label) c.setAttribute('aria-label', o.label);
    if (o.key) c.dataset.key = o.key;
    return c;
  }
  function renderLibrary(list, svc) {
    const panel = $('panel-lib');
    if (list == null) return fill(panel, card(down('Radarr'), true));
    if (!list.length) return fill(panel, card('Biblioteca e goală. Filmele descărcate apar aici.'));
    const grid = h('div', 'grid', ...list.map((m) => {
      let badge = null;
      if (m.ro_subs === true) badge = h('span', 'badge poster-badge', 'RO');
      else if (m.ro_subs === false) badge = h('span', 'badge poster-badge missing', 'fără RO');
      const size = `${n1.format(m.size_gb || 0)} GB`;
      const ro = m.ro_subs === true ? ', subtitrare RO' : m.ro_subs === false ? ', fără subtitrare RO' : '';
      return posterCard(m, size, badge, m.radarr_id == null ? null : {
        onClick: () => openMovie(m), key: 'movie-' + m.radarr_id,
        label: `${m.title}${m.year ? ', ' + m.year : ''}, ${size}${ro}. Detalii`,
      });
    }));
    const note = svc.bazarr ? null
      : h('p', 'meta muted panel-note', 'Bazarr nu răspunde, așa că nu știu ce subtitrări există.');
    keepFocus(panel, () => fill(panel, grid, note));
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
      value = t.waiting_watch ? 'gata, aștept să-l vezi' : 'gata';
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

  // ---------- Conversie pentru TV: the batch status (GET /api/convert, cheap, every poll) ----------
  // It feeds the Acasă banner, the Seriale action bar, the episode pills and the open sheet.
  let job = null;
  let convertInfo = { max: 20, default: 2, dry_run: false, demo: false };
  let convertLoading = false;
  const epText = (n) => (n === 1 ? '1 episod' : n % 100 >= 20 || (n && n % 100 === 0) ? `${n} de episoade` : `${n} episoade`);
  const seasonText = (n) => (n === 1 ? '1 sezon' : n % 100 >= 20 ? `${n} de sezoane` : `${n} sezoane`);
  const gb = (bytes) => `${n1.format((bytes || 0) / 1073741824)} GB`;
  const pct = (p) => `${n0.format(Math.floor(p || 0))}%`;

  async function loadConvert() {
    if (convertLoading || $('app').hidden) return;
    convertLoading = true;
    try {
      const c = await api('/api/convert');
      const before = job;
      job = c.job || null;
      convertInfo = c;
      const ended = before && before.running && !(job && job.running);
      const itemsGrew = job && before && (job.items || []).length > (before.items || []).length;
      if (ended && job && job.summary && !c.demo) toast(job.summary);
      if (ended || itemsGrew) { seriesAt = 0; if (detailId != null) loadDetail(true); }
      renderConvertStatus();
    } catch (e) {
      /* the banner keeps its last state; offline is shown by setOffline */
    } finally {
      convertLoading = false;
    }
  }
  function renderConvertStatus() {
    renderBanner();
    if (currentTab() === 'seriale' && detailId != null) renderDetail();
    refreshInfo();
  }
  const running = () => !!(job && job.running);
  // This batch belongs to series `id`.
  const jobOf = (id) => !!(job && !job.movie_id && job.series_id === id);

  // A conversion button (disabled while offline or while a start/stop request runs).
  function netButton(cls, text, onClick, key, off) {
    const b = h('button', cls, text);
    b.type = 'button';
    b.dataset.net = '1';
    b.dataset.off = off ? '1' : '';
    if (key) b.dataset.key = key;
    b.disabled = offline || convertBusy || !!off;
    b.addEventListener('click', onClick);
    return b;
  }

  async function convertAction(path, body, okText) {
    if (convertBusy) return;
    convertBusy = true;
    applyDisabled();
    try {
      const r = await api(path, body);
      toast(r.demo ? 'Mod demo: nu se convertește nimic.' : okText);
      if (r.job) { job = r.job; seriesAt = 0; renderConvertStatus(); }
    } catch (e) {
      if (e.message !== 'auth') toast(e.message, true);
    } finally {
      convertBusy = false;
      applyDisabled();
      loadConvert();
    }
  }
  const stopConvert = () => convertAction('/api/convert/stop', {}, 'Conversia se oprește.');

  // The running batch as text: ["Se convertește S01E04 · 45%", "Frieren · încă 1 episod"].
  function jobLines(j) {
    const cur = j.current;
    let head;
    if (j.stopping) head = 'Se oprește conversia.';
    else if (cur) head = `Se convertește ${cur.label} · ${pct(cur.percent)}`;
    else head = 'Pornește conversia.';
    const sub = [];
    if (!j.movie_id || !cur) sub.push(j.title || '');
    if (j.remaining) sub.push(`încă ${epText(j.remaining)}`);
    if (j.dry_run && !convertInfo.demo) sub.push('mod de probă');
    return [head, sub.filter(Boolean).join(' · ')];
  }
  function jobBar(j) {
    const b = bar(j.current ? j.current.percent : 0);
    b.setAttribute('aria-label', 'Progresul conversiei');
    return b;
  }

  // ---------- Traduceri RO: the manual translation queue (GET /api/subs) ----------
  // Fed by the series screen, the movie sheet and its own poll while one is queued or runs.
  let subs = null;
  let subsLoading = false;
  let subsSeen = null; // `at` of the newest result already shown
  const subsActive = () => !!(subs && (subs.inflight || subs.queue_len));

  function resultText(o) {
    if (!o.ok) return `Traducerea a eșuat: ${o.label} (${o.text}).`;
    return o.text === 'tradusă' ? `Subtitrare RO tradusă: ${o.label}.` : `${o.label}: ${o.text}.`;
  }
  // A new status; a result that came in since is toasted. True when a translation ended
  // (the screens then reload what Bazarr says now).
  function setSubs(s) {
    if (!s) return false;
    const before = subs;
    subs = s;
    const recent = s.recent || [];
    const newest = recent.length ? recent[0].at : 0;
    let fresh = [];
    if (subsSeen == null) subsSeen = newest;
    else {
      fresh = recent.filter((o) => o.at > subsSeen);
      subsSeen = Math.max(subsSeen, newest);
      if (fresh.length) toast(resultText(fresh[0]), !fresh[0].ok);
    }
    const was = before && before.inflight ? before.inflight.key : null;
    const now = s.inflight ? s.inflight.key : null;
    return fresh.length > 0 || (was != null && was !== now);
  }
  function renderSubsViews() {
    if (currentTab() === 'seriale' && detailId != null) renderDetail();
    refreshInfo();
  }
  async function loadSubs() {
    if (subsLoading || $('app').hidden) return;
    subsLoading = true;
    let ended = false;
    try {
      ended = setSubs(await api('/api/subs'));
      renderSubsViews();
    } catch (e) {
      /* the last state stays; offline is shown by setOffline */
    } finally {
      subsLoading = false;
    }
    if (ended) {
      if (detailId != null) loadDetail(true);
      if (movieInfo && !$('info-sheet').hidden) fetchMovie(movieInfo.id);
    }
  }

  // The live RO state of `key` (`ep:<id>` / `movie:<id>`) from the screen's `ro`/`ro_pos`.
  function roState(key, base, pos) {
    if (subs) {
      if (subs.inflight && subs.inflight.key === key) return { state: 'translating' };
      const q = (subs.queue || []).find((x) => x.key === key);
      if (q) return { state: 'queued', pos: q.position };
      if (base === 'queued') return { state: 'missing' }; // left the queue since the screen loaded
    }
    return { state: base || null, pos: pos || null };
  }
  const translatorOff = () => !!(subs && subs.translator === false);

  // A translation button (disabled while offline or while a request runs).
  function subsButton(cls, text, onClick, key, off) {
    const b = h('button', cls, text);
    b.type = 'button';
    b.dataset.subs = '1';
    b.dataset.off = off ? '1' : '';
    if (key) b.dataset.key = key;
    b.disabled = offline || subsBusy || !!off;
    b.addEventListener('click', onClick);
    return b;
  }
  async function subsAction(path, body) {
    if (subsBusy) return;
    subsBusy = true;
    applyDisabled();
    try {
      const r = await api(path, body);
      toast(r.demo ? `${r.message} Mod demo: nu se traduce nimic.` : r.message);
      if (r.subs) setSubs(r.subs);
    } catch (e) {
      if (e.message !== 'auth') toast(e.message, true);
      if (e.data && e.data.translator === false && subs) subs.translator = false;
    } finally {
      subsBusy = false;
      renderSubsViews();
      applyDisabled();
    }
  }

  // The translation part of an episode / movie sheet; `body` names it for the API.
  function trControls(st, body, label) {
    if (st.state === 'translating') return [h('p', 'body-strong', 'Se traduce…')];
    if (st.state === 'queued') {
      const cancel = subsButton('btn-text body-strong accent', 'Anulează',
        () => subsAction('/api/subs/translate/cancel', body), 'tr-act');
      cancel.setAttribute('aria-label', `Anulează traducerea: ${label}`);
      return [h('div', 'tr-line', h('span', 'body-strong', st.pos ? `În coadă (#${st.pos})` : 'În coadă'),
        h('span', 'meta muted', '·'), cancel)];
    }
    if (st.state !== 'missing') return [];
    const off = translatorOff();
    const go = subsButton('btn btn-plain btn-on-surface', 'Tradu în română',
      () => subsAction('/api/subs/translate', body), 'tr-act', off);
    return [go, off ? h('p', 'meta muted', 'Configurează Gemini în Bazarr.') : null];
  }

  // The series screen: translate the season's episodes without RO, and today's count.
  function seasonTranslate(d, cur) {
    if (!d.bazarr || !cur) return null;
    const missing = cur.episodes.filter((e) => roState('ep:' + e.id, e.ro, e.ro_pos).state === 'missing').length;
    const off = translatorOff();
    const b = subsButton('btn btn-plain', `Tradu sezonul (${missing} fără RO)`,
      () => subsAction('/api/subs/translate', { seriesId: d.id, season: cur.number, missingOnly: true }),
      'tr-season', missing === 0 || off);
    b.setAttribute('aria-label', missing
      ? `Tradu în română ${epText(missing)} fără RO din ${cur.label}`
      : `Niciun episod fără RO în ${cur.label}`);
    let line = null;
    if (off) line = 'Configurează Gemini în Bazarr.';
    else if (subs) {
      const parts = [`Traduceri azi: ${subs.used} din ${subs.per_day}`];
      if (subs.queue_len) parts.push(`${subs.queue_len} în coadă`);
      if (subs.inflight) parts.push(`se traduce ${subs.inflight.label}`);
      line = parts.join(' · ');
    }
    return h('div', 'tr-season', b, line ? h('p', 'meta muted', line) : null);
  }

  // ---------- Acasă: the banner while a conversion runs ----------
  function renderBanner() {
    const box = $('convert-banner');
    box.hidden = !running();
    if (!running()) return fill(box);
    const [head, sub] = jobLines(job);
    const open = !job.movie_id && job.series_id
      ? h('a', 'btn-text body-strong', 'Vezi serialul') : null;
    if (open) open.href = '#seriale/' + job.series_id;
    keepFocus(box, () => fill(box,
      h('div', 'banner-lines', h('p', 'body-strong', head), sub ? h('p', 'meta muted', sub) : null),
      jobBar(job),
      job.stopping ? null : netButton('btn btn-danger', 'Oprește conversia', stopConvert, 'banner-stop'),
      open));
  }

  // ---------- Seriale: the poster grid (GET /api/series) ----------
  const SERIES_IDLE_MS = 60000;
  let seriesList = null;
  let seriesAt = 0;
  let seriesLoading = false;
  let gridScroll = 0;
  let cameFromGrid = false;

  async function loadSeries(force) {
    if (seriesLoading || $('app').hidden) return;
    const fast = seriesList && seriesList.loading;
    if (!force && !fast && seriesList && Date.now() - seriesAt < SERIES_IDLE_MS) return;
    seriesLoading = true;
    try {
      seriesList = await api('/api/series');
      seriesAt = Date.now();
      renderGrid();
    } catch (e) {
      if (e.message !== 'auth' && !e.offline && !seriesList) fill($('series-grid'), card(e.message, true));
    } finally {
      seriesLoading = false;
    }
  }

  function seriesCard(s) {
    const unwatched = s.unwatched;
    const badge = unwatched ? h('span', 'badge poster-badge', `${unwatched} ${unwatched === 1 ? 'nevăzut' : 'nevăzute'}`) : null;
    const flag = s.convert ? h('span', 'badge poster-flag', icon('conv'), String(s.convert)) : null;
    const parts = [s.title];
    if (unwatched != null) parts.push(unwatched ? `${unwatched} ${unwatched === 1 ? 'episod nevăzut' : 'episoade nevăzute'}` : 'totul văzut');
    if (s.convert) parts.push(`${epText(s.convert)} de convertit pentru TV`);
    return posterCard(s, epText(s.files || 0), badge, {
      href: '#seriale/' + s.id, flag, label: parts.join(', '), key: 'series-' + s.id,
    });
  }
  function renderGrid() {
    const box = $('series-grid');
    const c = seriesList;
    if (!c) return fill(box, card('Se încarcă serialele.'));
    if (c.error) return fill(box, card(c.error, true));
    if (c.series == null) return fill(box, card('Se încarcă serialele.'));
    if (!c.series.length) return fill(box, card('Niciun serial încă. Serialele din Watchlist-ul Plex apar aici după descărcare.'));
    const grid = h('div', 'grid', ...c.series.map(seriesCard));
    for (const a of grid.querySelectorAll('a.pcard')) a.addEventListener('click', () => { cameFromGrid = true; });
    const plexDown = c.series.some((s) => s.unwatched == null);
    const note = plexDown ? h('p', 'meta muted panel-note', 'Plex nu răspunde, nu știu ce episoade ai văzut.') : null;
    keepFocus(box, () => fill(box, grid, note));
  }
  function renderSeriesTab() {
    if (detailId != null) renderDetail();
    else { renderGrid(); renderBar(); }
  }

  // ---------- Seriale: one series (GET /api/series/<id>), its seasons and episodes ----------
  const DETAIL_IDLE_MS = 30000;
  const DETAIL_TIMEOUT_MS = 20000;
  let detail = null;
  let detailId = null;
  let detailError = null;
  let detailAt = 0;
  let detailLoading = false;
  let season = null;
  let stepCount = null;

  async function loadDetail(force) {
    const id = detailId;
    if (id == null || detailLoading || $('app').hidden) return;
    if (!force && detail && detail.id === id && Date.now() - detailAt < DETAIL_IDLE_MS) return;
    detailLoading = true;
    try {
      const d = await api('/api/series/' + id, undefined, DETAIL_TIMEOUT_MS);
      if (id !== detailId) return;
      detail = d;
      detailError = null;
      detailAt = Date.now();
      if (d.job !== undefined && !convertLoading) job = d.job;
      if (d.subs) setSubs(d.subs);
      if (season == null || !d.seasons.some((s) => s.number === season)) season = d.default_season;
    } catch (e) {
      if (e.message === 'auth' || id !== detailId) return;
      if (!detail) detailError = e.offline ? OFFLINE_TEXT : e.message;
    } finally {
      detailLoading = false;
    }
    if (currentTab() === 'seriale' && id === detailId) { renderDetail(); refreshInfo(); }
  }

  // The live state of an episode: the screen's data plus the batch from the last poll.
  function epState(e) {
    const mine = jobOf(detail.id);
    const ok = mine && (job.items || []).some((i) => i.result === 'ok' && i.episode_id === e.id);
    const converting = mine && job.running && job.current && job.current.episode_id === e.id;
    return {
      converting: converting ? job.current.percent : null,
      queued: mine && job.running && (job.queued || []).includes(e.id),
      converted: e.converted || ok,
      canConvert: e.can_convert && !ok && !converting,
      needs: e.needs_conversion && !ok,
    };
  }
  // [text, tone] of the video pill, or null (a watched episode that plays as it is).
  function videoPill(e, st) {
    if (st.converting != null) return [`Se convertește ${pct(st.converting)}`, 'live'];
    if (st.converted) return ['Convertit', 'ok'];
    if (st.queued) return ['În coadă', 'warn'];
    if (st.needs && e.watched !== true) return ['Trebuie convertit', 'warn'];
    if (st.needs) return null;
    return ['Merge direct', ''];
  }
  const RO_PILL = { ok: ['RO ✓', ''], missing: ['fără RO', 'warn'], translating: ['RO se traduce', ''], queued: ['RO în coadă', ''] };
  const pill = (p) => (p ? h('span', 'pill' + (p[1] ? ' pill-' + p[1] : ''), p[0]) : null);
  function roPill(st) {
    const p = RO_PILL[st.state];
    if (!p) return null;
    return pill(st.state === 'queued' && st.pos ? [`RO în coadă #${st.pos}`, p[1]] : p);
  }
  const epRo = (e) => roState('ep:' + e.id, e.ro, e.ro_pos);

  function episodeRow(e) {
    const st = epState(e);
    const seen = e.watched === true;
    const b = h('button', 'li li-btn ep-row',
      h('span', 'ep-head',
        h('span', 'data ep-num', String(e.episode)),
        h('span', 'tile-label ep-title', e.title || e.label),
        seen ? h('span', 'ep-seen', h('span', '', '✓'), sr(' văzut')) : null),
      h('span', 'pills', pill(videoPill(e, st)), roPill(epRo(e))));
    b.type = 'button';
    b.dataset.key = 'ep-' + e.id;
    b.setAttribute('aria-haspopup', 'dialog');
    b.addEventListener('click', () => openEpisode(e.id));
    return b;
  }

  function seasonChips(seasons) {
    const list = h('div', 'season-chips');
    list.setAttribute('role', 'tablist');
    list.setAttribute('aria-label', 'Sezoane');
    for (const s of seasons) {
      const on = s.number === season;
      const b = h('button', 'season-chip', s.label);
      b.type = 'button';
      b.id = 'season-' + s.number;
      b.dataset.key = 'season-' + s.number;
      b.setAttribute('role', 'tab');
      b.setAttribute('aria-selected', String(on));
      b.setAttribute('aria-controls', 'season-panel');
      b.tabIndex = on ? 0 : -1;
      b.addEventListener('click', () => { season = s.number; renderDetail(); });
      list.appendChild(b);
    }
    list.addEventListener('keydown', (ev) => {
      if (ev.key !== 'ArrowLeft' && ev.key !== 'ArrowRight') return;
      ev.preventDefault();
      const i = seasons.findIndex((s) => s.number === season);
      const next = seasons[(i + (ev.key === 'ArrowRight' ? 1 : seasons.length - 1)) % seasons.length];
      season = next.number;
      renderDetail();
      const chip = $('season-' + season);
      if (chip) { chip.focus(); chip.scrollIntoView({ block: 'nearest', inline: 'nearest' }); }
    });
    return list;
  }

  function backButton() {
    const b = h('button', 'btn-text body-strong back', icon('back'), 'Seriale');
    b.type = 'button';
    b.dataset.key = 'back';
    b.addEventListener('click', () => {
      if (cameFromGrid) history.back();
      else location.replace('#seriale');
    });
    return b;
  }

  function renderDetail() {
    const box = $('series-detail');
    const d = detail && detail.id === detailId ? detail : null;
    if (!d) {
      const title = h('h1', 'title-screen', 'Serial');
      title.id = 't-series';
      title.tabIndex = -1;
      fill(box, backButton(), title, card(detailError || 'Se încarcă serialul.', !!detailError));
      return renderBar();
    }
    const title = h('h1', 'title-screen', d.title);
    title.id = 't-series';
    title.tabIndex = -1;
    const facts = [];
    if (d.year) facts.push(String(d.year));
    facts.push(seasonText(d.seasons.filter((s) => s.number !== 0).length || d.seasons.length));
    if (d.unwatched != null) facts.push(d.unwatched ? `${d.unwatched} ${d.unwatched === 1 ? 'nevăzut' : 'nevăzute'}` : 'totul văzut');
    let tvLine;
    const pending = pendingOf(d);
    if (pending == null) tvLine = h('span', 'meta muted', 'Plex nu răspunde, nu știu ce episoade ai văzut.');
    else if (pending > 0) tvLine = h('span', 'meta accent', `${epText(pending)} de convertit pentru TV, de la ${d.next}.`);
    else tvLine = h('span', 'meta muted', 'Toate episoadele nevăzute merg direct pe TV.');
    const art = posterImg(d.backdrop);
    const head = h('div', 'np series-head',
      art ? h('div', 'np-backdrop', art) : null,
      poster(d.poster, d.title, 'hero'),
      h('div', 'np-body', h('span', 'meta', facts.join(' · ')), tvLine));
    const cur = d.seasons.find((s) => s.number === season) || d.seasons[0];
    let list;
    if (!cur) list = card('Niciun episod descărcat încă.');
    else {
      list = h('div', 'group', ...cur.episodes.map(episodeRow));
      list.id = 'season-panel';
      list.setAttribute('role', 'tabpanel');
      list.setAttribute('aria-labelledby', 'season-' + cur.number);
    }
    const notes = [];
    if (!d.bazarr) notes.push(h('p', 'meta muted', 'Bazarr nu răspunde, așa că nu știu ce subtitrări există.'));
    if (d.seasons.some((s) => s.episodes.some((e) => e.needs_conversion))) {
      notes.push(h('p', 'meta muted', 'Televizorul nu poate reda direct H.264 pe 10 biți. Îl convertesc în HEVC pe placa '
        + 'video, de la primul episod nevăzut, în ordine. După ce îl vezi, pun la loc fișierul din torrent.'));
    }
    // The chip row keeps its horizontal scroll across polls; the first time, the chosen season shows.
    const oldRow = box.querySelector('.season-chips');
    const keepLeft = oldRow ? oldRow.scrollLeft : null;
    keepFocus(box, () => fill(box, backButton(), title, head,
      d.seasons.length > 1 ? seasonChips(d.seasons) : null, seasonTranslate(d, cur), list, ...notes));
    const row = box.querySelector('.season-chips');
    if (row && keepLeft != null) row.scrollLeft = keepLeft;
    else if (row) {
      const chip = $('season-' + season);
      if (chip) row.scrollLeft = Math.max(0, chip.offsetLeft - row.offsetLeft - row.clientWidth / 2 + chip.offsetWidth / 2);
    }
    renderBar();
  }
  // Episodes still to convert: the screen's count minus what this batch did since.
  function pendingOf(d) {
    if (d.pending == null) return null;
    let n = d.pending;
    for (const s of d.seasons) for (const e of s.episodes) if (e.can_convert && epState(e).converted) n -= 1;
    return Math.max(0, n);
  }

  // ---------- Seriale: the sticky action bar ----------
  function stepButton(text, label, delta, off) {
    const b = netButton('step', text, () => { stepCount += delta; renderBar(); }, 'step' + delta, off);
    b.setAttribute('aria-label', label);
    return b;
  }
  function renderBar() {
    const barEl = $('series-bar');
    const d = detail && detail.id === detailId ? detail : null;
    const show = currentTab() === 'seriale' && detailId != null && d && !$('app').hidden;
    let content = null;
    if (show && running()) {
      const [head, sub] = jobLines(job);
      const other = !jobOf(d.id);
      content = [h('div', 'bar-status',
        h('span', 'tile-label', other ? `${job.title}: ${head.charAt(0).toLowerCase()}${head.slice(1)}` : head),
        sub && !other ? h('span', 'meta muted', sub) : null,
        jobBar(job)),
      job.stopping ? null : netButton('btn btn-danger btn-fit', 'Oprește', stopConvert, 'bar-stop')];
    } else if (show && pendingOf(d) > 0) {
      const max = Math.max(1, Math.min(convertInfo.max || 20, pendingOf(d)));
      if (stepCount == null) stepCount = convertInfo.default || 2;
      stepCount = Math.max(1, Math.min(stepCount, max));
      const value = h('span', 'data stepper-value', String(stepCount));
      value.setAttribute('aria-live', 'polite');
      value.setAttribute('aria-label', epText(stepCount));
      const go = netButton('btn btn-primary', 'Convertește următoarele',
        () => convertAction('/api/convert/start', { seriesId: d.id, count: stepCount }, 'Conversia a pornit.'), 'bar-go');
      go.setAttribute('aria-label', `Convertește următoarele ${epText(stepCount)}, de la ${d.next}`);
      const stepper = h('div', 'stepper', stepButton('−', 'Mai puține', -1, stepCount <= 1), value,
        stepButton('+', 'Mai multe', 1, stepCount >= max));
      stepper.setAttribute('role', 'group');
      stepper.setAttribute('aria-label', 'Câte episoade');
      content = [go, stepper];
    }
    barEl.hidden = !content;
    document.body.classList.toggle('has-bar', !!content);
    if (!content) return fill($('series-bar-inner'));
    keepFocus(barEl, () => fill($('series-bar-inner'), ...content));
    document.documentElement.style.setProperty('--bar-h', barEl.offsetHeight + 'px');
  }

  // ---------- details sheet: an episode or a movie ----------
  let infoRender = null; // re-renders the open sheet from the latest data (or null)

  function fact(label, value) {
    return h('div', 'fact', h('dt', 'meta muted', label), h('dd', 'data', value));
  }
  function roText(st) {
    if (st.state === 'queued') return st.pos ? `în coadă pentru traducere (#${st.pos})` : 'în coadă pentru traducere';
    return { ok: 'există', missing: 'lipsește', translating: 'se traduce acum' }[st.state] || 'necunoscută';
  }
  const seenText = (w) => (w === true ? 'da' : w === false ? 'nu' : 'necunoscut');
  function fillInfo(title, sub, ...nodes) {
    $('info-title').textContent = title;
    $('info-sub').textContent = sub || '';
    $('info-sub').hidden = !sub;
    keepFocus($('info-body'), () => fill($('info-body'), ...nodes));
  }
  function refreshInfo() {
    if (infoRender && !$('info-sheet').hidden) infoRender();
  }
  function openInfo(render) {
    infoRender = render;
    render();
    showSheet($('info-sheet'), $('info-title'));
  }

  function findEpisode(id) {
    for (const s of (detail && detail.seasons) || []) for (const e of s.episodes) if (e.id === id) return e;
    return null;
  }
  function openEpisode(id) {
    openInfo(() => {
      const e = findEpisode(id);
      if (!e) return closeSheet();
      const st = epState(e);
      const vp = videoPill(e, st);
      const ro = epRo(e);
      const facts = h('dl', 'facts',
        fact('Fișier', e.file || ''),
        fact('Mărime', gb(e.size)),
        fact('Video', [e.codec || 'necunoscut', vp ? vp[0].toLocaleLowerCase('ro-RO') : 'merge direct'].join(', ')),
        fact('Subtitrare RO', roText(ro)),
        fact('Văzut', seenText(e.watched)));
      const tr = trControls(ro, { seriesId: detail.id, episodeIds: [e.id] }, e.label);
      const trNote = ro.state === 'missing' && e.ro_note ? h('p', 'meta muted', e.ro_note) : null;
      const actions = [];
      let note = null;
      if (st.converting != null) {
        actions.push(jobBar(job));
        if (!job.stopping) actions.push(netButton('btn btn-danger', 'Oprește conversia', stopConvert, 'info-stop'));
      } else if (st.canConvert) {
        const busyNow = running();
        actions.push(netButton('btn btn-primary', 'Convertește doar acest episod',
          () => { closeSheet(); convertAction('/api/convert/start', { seriesId: detail.id, episodeIds: [e.id] }, 'Conversia a pornit.'); },
          'info-go', busyNow));
        if (busyNow) note = 'O conversie rulează deja. Așteaptă să se termine sau oprește-o.';
      } else if (st.needs && e.watched === true) {
        note = 'L-ai văzut deja, așa că nu-l mai convertesc.';
      } else if (st.converted) {
        note = 'Convertit pentru TV. După ce îl vezi, pun la loc fișierul din torrent.';
      }
      fillInfo(`${e.label} · ${e.title || ''}`.replace(/ · $/, ''), detail.title, facts,
        note ? h('p', 'meta muted', note) : null, ...actions, trNote, ...tr);
    });
  }

  // Filme: the movie sheet (GET /api/movies/<radarr id>, loaded when it opens).
  let movieInfo = null;
  async function fetchMovie(id) {
    try {
      const d = await api('/api/movies/' + id, undefined, DETAIL_TIMEOUT_MS);
      if (!movieInfo || movieInfo.id !== id) return;
      movieInfo.data = d;
      movieInfo.error = null;
      if (d.job !== undefined && !convertLoading) job = d.job;
      if (d.subs) setSubs(d.subs);
    } catch (e) {
      if (e.message === 'auth' || !movieInfo || movieInfo.id !== id) return;
      if (!movieInfo.data) movieInfo.error = e.offline ? OFFLINE_TEXT : e.message;
    }
    refreshInfo();
  }
  function openMovie(m) {
    movieInfo = { id: m.radarr_id, data: null, error: null };
    openInfo(() => renderMovie(m));
    fetchMovie(m.radarr_id);
  }
  function renderMovie(m) {
    const mi = movieInfo && movieInfo.id === m.radarr_id ? movieInfo : null;
    const sub = m.year ? String(m.year) : '';
    if (!mi || !mi.data) return fillInfo(m.title, sub, card(mi && mi.error ? mi.error : 'Se încarcă detaliile.', !!(mi && mi.error)));
    const d = mi.data;
    const live = job && job.movie_id === d.id;
    const converting = live && job.running && job.current ? job.current.percent : null;
    const converted = d.converted || (live && (job.items || []).some((i) => i.result === 'ok'));
    let video = d.codec || 'necunoscut';
    if (converting != null) video += `, se convertește ${pct(converting)}`;
    else if (converted) video += ', convertit';
    else if (d.needs_conversion) video += ', nu merge direct pe TV';
    const ro = roState('movie:' + d.id, d.ro, d.ro_pos);
    const facts = h('dl', 'facts',
      d.file ? fact('Fișier', d.file) : null,
      fact('Mărime', gb(d.size)),
      fact('Video', video),
      fact('Subtitrare RO', roText(ro)),
      fact('Văzut', seenText(d.watched)));
    const tr = d.file ? trControls(ro, { movieId: d.id }, d.title || m.title) : [];
    const trNote = ro.state === 'missing' && d.ro_note ? h('p', 'meta muted', d.ro_note) : null;
    const actions = [];
    let note = null;
    if (converting != null) {
      actions.push(jobBar(job));
      if (!job.stopping) actions.push(netButton('btn btn-danger', 'Oprește conversia', stopConvert, 'info-stop'));
    } else if (d.can_convert && !converted) {
      const busyNow = running();
      note = 'Televizorul nu poate reda direct H.264 pe 10 biți. Îl convertesc în HEVC pe placa video; după ce îl vezi, '
        + 'pun la loc fișierul din torrent.';
      if (busyNow) note = 'O conversie rulează deja. Așteaptă să se termine sau oprește-o.';
      actions.push(netButton('btn btn-primary', 'Convertește pentru TV',
        () => { closeSheet(); convertAction('/api/convert/start', { movieId: d.id }, 'Conversia a pornit.'); }, 'info-go', busyNow));
    } else if (converted) {
      note = 'Convertit pentru TV. După ce îl vezi, pun la loc fișierul din torrent.';
    } else if (d.needs_conversion && d.watched === true) {
      note = 'L-ai văzut deja, așa că nu-l mai convertesc.';
    } else if (d.needs_conversion && !d.plex) {
      note = 'Plex nu răspunde, nu știu dacă ai văzut filmul.';
    }
    fillInfo(d.title || m.title, d.year ? String(d.year) : sub, facts, note ? h('p', 'meta muted', note) : null, ...actions,
      trNote, ...tr);
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

  // ---------- ActionSheet (power) and the details sheet: one open at a time, over one scrim ----------
  let lastFocus = null;
  let sheetKind = null;
  let sheetTimer = null;
  let openEl = null;
  function showSheet(el, focusEl) {
    if (openEl && openEl !== el) closeSheet(true);
    clearTimeout(sheetTimer);
    if (!openEl) lastFocus = document.activeElement;
    openEl = el;
    $('app').inert = true;
    $('scrim').hidden = false;
    el.hidden = false;
    if (reducedMotion.matches) {
      $('scrim').classList.add('open');
      el.classList.add('open');
    } else {
      requestAnimationFrame(() => requestAnimationFrame(() => {
        if (openEl !== el) return;
        $('scrim').classList.add('open');
        el.classList.add('open');
      }));
    }
    focusEl.focus({ preventScroll: true });
  }
  function openSheet(kind) {
    if (pendingPower || offline) return;
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
      h('span', 'meta', (w.remaining || '').replace(/ (?=h)|(?<=ratio) /g, NBSP)))));
    showSheet($('sheet'), $('sheet-title'));
  }
  function closeSheet(instant) {
    const el = openEl;
    if (!el) return;
    openEl = null;
    if (el === $('info-sheet')) { infoRender = null; movieInfo = null; }
    $('app').inert = false;
    $('scrim').classList.remove('open');
    el.classList.remove('open');
    const done = () => { if (!openEl) $('scrim').hidden = true; el.hidden = true; };
    clearTimeout(sheetTimer);
    if (instant || reducedMotion.matches) done();
    else sheetTimer = setTimeout(done, 240);
    if (lastFocus && lastFocus.focus && !lastFocus.disabled && document.contains(lastFocus)) lastFocus.focus({ preventScroll: true });
    lastFocus = null;
  }
  $('tile-shutdown').addEventListener('click', () => openSheet('shutdown'));
  $('btn-shutdown').addEventListener('click', () => openSheet('shutdown'));
  $('btn-restart').addEventListener('click', () => openSheet('restart'));
  $('sheet-cancel').addEventListener('click', () => closeSheet());
  $('info-close').addEventListener('click', () => closeSheet());
  $('scrim').addEventListener('click', () => closeSheet());
  $('sheet-confirm').addEventListener('click', () => power(sheetKind));
  $('sheet-after').addEventListener('click', () => armAfterMovie());
  document.addEventListener('keydown', (e) => {
    if (!openEl) return;
    if (e.key === 'Escape') { e.preventDefault(); closeSheet(); return; }
    if (e.key === 'Tab') { // keep focus inside the sheet
      const items = [...openEl.querySelectorAll('button, a[href]')]
        .filter((b) => !b.disabled && !b.closest('[hidden]'));
      if (!items.length) return;
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
  $('logout-all').addEventListener('click', async () => {
    busy.add('logout-all');
    applyDisabled();
    try {
      await api('/api/logout-all', {});
      forgetLast();
      last = null;
      showLogin();
      toast('Ai ieșit de pe toate dispozitivele.');
    } catch (e) {
      if (e.message !== 'auth') toast(e.message, true);
    } finally {
      busy.delete('logout-all');
      applyDisabled();
    }
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
