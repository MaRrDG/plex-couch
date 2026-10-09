'use strict';

(function () {
  const POLL_MS = 5000;
  const TIMEOUT_MS = 8000;
  const PIN_LEN = 6;
  const CSRF = { 'X-Requested-With': 'plex-couch' };
  const SERVICES = [
    ['plex', 'Plex'], ['radarr', 'Radarr'], ['prowlarr', 'Prowlarr'],
    ['bazarr', 'Bazarr'], ['qbittorrent', 'qBittorrent'],
  ];
  const reducedMotion = window.matchMedia('(prefers-reduced-motion: reduce)');

  const $ = (id) => document.getElementById(id);
  let timer = null;
  let loggedIn = false;
  let last = null;
  let clockOffset = 0;
  let pendingPower = null;
  let countdownTimer = null;

  // ---------- formatting (Romanian: decimal comma) ----------
  const nf = (d) => new Intl.NumberFormat('ro-RO', { minimumFractionDigits: d, maximumFractionDigits: d });
  const n0 = nf(0), n1 = nf(1), n2 = nf(2);
  const lower = (s) => (s || '').toLocaleLowerCase('ro-RO');
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
  const withYear = (t, y) => (y ? `${t} (${y})` : t);

  // ---------- DOM helpers ----------
  function el(tag, cls, text) {
    const e = document.createElement(tag);
    if (cls) e.className = cls;
    if (text !== undefined && text !== null) e.textContent = text;
    return e;
  }
  function kids(parent, ...nodes) {
    for (const n of nodes) if (n) parent.appendChild(n);
    return parent;
  }
  function bar(fraction, cls) {
    const b = el('div', 'bar' + (cls ? ' ' + cls : ''));
    const s = el('span');
    s.style.width = (Math.max(0, Math.min(1, fraction || 0)) * 100).toFixed(1) + '%';
    b.appendChild(s);
    return b;
  }
  const cell = (...nodes) => kids(el('div', 'cell'), ...nodes);
  // Release names have no spaces: allow line breaks only after dots, dashes and underscores.
  function breakable(text, cls) {
    const span = el('span', cls);
    for (const part of String(text || '').split(/(?<=[._-])/)) {
      span.appendChild(document.createTextNode(part));
      span.appendChild(document.createElement('wbr'));
    }
    return span;
  }
  // A title line with an optional right-hand value.
  function row(left, right, leftCls) {
    const r = el('div', 'row');
    r.appendChild(typeof left === 'string' ? el('span', 'grow ' + (leftCls || 'title'), left) : left);
    if (right != null) r.appendChild(typeof right === 'string' ? el('span', 'aside num', right) : right);
    return r;
  }
  const sub = (text, cls) => el('p', 'sub' + (cls ? ' ' + cls : ''), text);
  const foot = (text, cls) => el('p', 'foot-note num' + (cls ? ' ' + cls : ''), text);
  const empty = (text, cls) => cell(el('p', cls || 'empty', text));
  const fill = (id, nodes) => $(id).replaceChildren(...nodes);

  const SVG_NS = 'http://www.w3.org/2000/svg';
  function checkmark() {
    const svg = document.createElementNS(SVG_NS, 'svg');
    svg.setAttribute('viewBox', '0 0 20 20');
    svg.setAttribute('class', 'check');
    svg.setAttribute('role', 'img');
    svg.setAttribute('aria-label', 'gata');
    const p = document.createElementNS(SVG_NS, 'path');
    p.setAttribute('d', 'M4 10.5l4 4 8-9');
    p.setAttribute('fill', 'none');
    p.setAttribute('stroke', 'currentColor');
    p.setAttribute('stroke-width', '2.2');
    p.setAttribute('stroke-linecap', 'round');
    p.setAttribute('stroke-linejoin', 'round');
    svg.appendChild(p);
    return svg;
  }

  let toastTimer = null;
  function toast(text, isErr) {
    const t = $('toast');
    t.textContent = text;
    t.className = 'toast' + (isErr ? ' err' : '');
    t.hidden = false;
    clearTimeout(toastTimer);
    toastTimer = setTimeout(() => { t.hidden = true; }, 3200);
  }

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
      const err = new Error('Nu mă pot conecta la PC. Verifică Wi-Fi-ul.');
      err.offline = true;
      throw err;
    } finally {
      clearTimeout(to);
    }
    setOffline(false);
    let data = null;
    try { data = await r.json(); } catch (e) { data = null; }
    if (r.status === 401 && path !== '/api/login') {
      showLogin();
      throw new Error('auth');
    }
    if (!r.ok) {
      const err = new Error((data && data.error) || `Eroare ${r.status}.`);
      err.data = data;
      throw err;
    }
    return data;
  }
  function setOffline(on) {
    $('offline').hidden = !on;
    document.body.classList.toggle('is-offline', on);
    for (const id of ['mon-off', 'mon-on', 'auto-off']) $(id).disabled = on;
    for (const id of ['pw-shutdown-row', 'pw-restart-row']) $(id).disabled = on || !!pendingPower;
  }

  // ---------- login ----------
  let pin = '';
  let lockUntil = 0;
  let lockTimer = null;
  let busy = false;

  function drawDots() {
    const dots = $('pin-dots').children;
    for (let i = 0; i < dots.length; i++) dots[i].classList.toggle('on', i < pin.length);
    $('kp-del').classList.toggle('invisible', !pin.length);
    $('kp-ok').classList.toggle('invisible', pin.length < 4 || pin.length >= PIN_LEN);
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
    if (busy || pin.length < 4 || Date.now() < lockUntil) return;
    busy = true;
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
        $('login-msg').textContent = e.message || 'Nu mă pot conecta la PC. Verifică Wi-Fi-ul.';
      }
    } finally {
      busy = false;
    }
  }
  function pressKey(k) {
    if (Date.now() < lockUntil || busy) return;
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
    const b = $('keypad').querySelector(`.kp[data-k="${k}"]`);
    if (b) { b.classList.add('pressed'); setTimeout(() => b.classList.remove('pressed'), 120); }
    pressKey(k);
  });

  function showLogin(msg) {
    loggedIn = false;
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
    loggedIn = true;
    $('login').hidden = true;
    $('app').hidden = false;
    refresh();
  }

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
      loggedIn = true;
      $('login').hidden = true;
      $('app').hidden = false;
      render(s);
    } catch (e) {
      if (e.message === 'auth') return;
      if (e.offline) {
        setOffline(true);
        $('login').hidden = true;
        $('app').hidden = false;
        if (!last) renderOfflineSkeleton();
        if (last) $('updated').textContent = 'Ultima actualizare la ' + hhmm(new Date(last._at));
      } else {
        $('updated').textContent = e.message;
      }
    }
    schedule();
  }
  document.addEventListener('visibilitychange', () => {
    if (document.visibilityState === 'visible') { if (!$('app').hidden || loggedIn) refresh(); }
    else stopPolling();
  });
  window.addEventListener('online', () => { if (!$('app').hidden) refresh(); });

  // ---------- render ----------
  function render(s) {
    s._at = Date.now();
    last = s;
    clockOffset = (s.server_time || Date.now() / 1000) - Date.now() / 1000;
    const mode = s.demo ? 'demo' : (s.dry_run ? 'dry-run' : '');
    $('mode').textContent = mode;
    $('mode').hidden = !mode;
    const svc = s.services || {};
    $('auto-off').checked = !!(s.settings && s.settings.auto_monitors_off);
    renderPlaying(s.now_playing, svc);
    renderDownloads(s.downloads);
    renderTorrents(s.torrents);
    renderWaiting(s.waiting);
    renderLibrary(s.library, svc);
    renderDisks(s.pc || {});
    renderServices(svc);
    setPower(s.power);
    $('updated').textContent = 'Actualizat la ' + hhmm(new Date());
  }

  function renderOfflineSkeleton() {
    const dash = () => [empty('Fără date de la PC.')];
    for (const id of ['playing', 'downloads', 'torrents', 'waiting', 'library', 'disks']) fill(id, dash());
    fill('services', SERVICES.map(([, label]) => cell(row(label, el('span', 'status', '—')))));
    $('pc-line').hidden = true;
    $('updated').textContent = '';
  }

  const down = (name) => `${name} nu răspunde. Pornește-l din Services sau repornește PC-ul.`;

  function renderPlaying(list, svc) {
    if (list == null || (!list.length && !svc.plex)) return fill('playing', [empty(down('Plex'), 'warn-text')]);
    if (!list.length) return fill('playing', [empty('Nimic pe TV acum.')]);
    fill('playing', list.map((p) => {
      const where = p.duration ? `${clock(p.position)} din ${clock(p.duration)}` : `${n0.format(p.progress || 0)}%`;
      return cell(
        el('p', 'title', withYear(p.title, p.year)),
        sub(`${p.player}, ${p.state_ro || p.state}`),
        bar((p.progress || 0) / 100, 'thin'),
        foot(`${where}, ${p.decision_ro}`, p.transcode ? 'warn-text' : ''),
      );
    }));
  }

  function renderDownloads(list) {
    if (list == null) return fill('downloads', [empty(down('Radarr'), 'warn-text')]);
    if (!list.length) return fill('downloads', [empty('Nicio descărcare. Adaugă un film în Watchlist-ul Plex.')]);
    fill('downloads', list.map((d) => {
      const parts = [`${n0.format(d.percent)}%`];
      if (d.time_left) parts.push(d.time_left);
      const st = lower(d.status);
      if (st && st !== 'se descarcă') parts.push(st);
      return cell(el('p', 'title', withYear(d.title, d.year)), bar(d.percent / 100, 'thin'), foot(parts.join(', ')));
    }));
  }

  function renderTorrents(list) {
    if (list == null) return fill('torrents', [empty(down('qBittorrent'), 'warn-text')]);
    if (!list.length) return fill('torrents', [empty('Niciun torrent FileList la seed.')]);
    fill('torrents', list.map((t) => {
      if (t.hr === 'downloading') {
        return cell(row(breakable(t.name, 'grow name'), `${n0.format(t.progress)}%`), bar(t.progress / 100, 'thin'),
          foot('se descarcă, regula de seed începe la final'));
      }
      const safe = t.hr === 'safe';
      const hours = n0.format(Math.min(48, Math.floor(t.seed_hours)));
      return cell(
        row(breakable(t.name, 'grow name'), safe ? checkmark() : null),
        bar(t.rule_progress, safe ? 'thin ok' : 'thin'),
        foot(`ratio ${n2.format(t.ratio)}, ${hours} din 48 h`),
      );
    }));
  }

  function renderWaiting(list) {
    if (list == null) return fill('waiting', [empty(down('Radarr'), 'warn-text')]);
    if (!list.length) return fill('waiting', [empty('Nimic în așteptare. Adaugă un film în Watchlist-ul Plex.')]);
    fill('waiting', list.map((m) => cell(el('p', 'title', withYear(m.title, m.year)), foot(m.status))));
  }

  function renderLibrary(list, svc) {
    if (list == null) return fill('library', [empty(down('Radarr'), 'warn-text')]);
    if (!list.length) return fill('library', [empty('Biblioteca e goală. Filmele descărcate apar aici.')]);
    const rows = list.map((m) => {
      let subs = null;
      if (m.ro_subs === true) subs = foot('subtitrare RO');
      else if (m.ro_subs === false) subs = foot('fără subtitrare RO', 'warn-text');
      return cell(row(withYear(m.title, m.year), `${n1.format(m.size_gb)} GB`), subs);
    });
    if (!svc.bazarr) rows.push(empty('Bazarr nu răspunde, așa că nu știu ce subtitrări există.', 'warn-text'));
    fill('library', rows);
  }

  function renderDisks(pc) {
    const disks = pc.disks || [];
    const main = disks.find((d) => d.drive === 'D:') || disks[0];
    const free = (d) => `${n0.format(d.free_gb)} GB liberi din ${n0.format(d.total_gb)} GB`;
    if (!main) fill('disks', [empty('Nu pot citi discurile.')]);
    else {
      const cells = [cell(row(`Disc ${main.drive}`, free(main)), bar(main.used_pct / 100, 'thin'))];
      for (const d of disks) if (d !== main) cells.push(cell(row(`Disc ${d.drive}`, free(d))));
      fill('disks', cells);
    }
    $('pc-line').hidden = !pc.hostname;
    $('pc-line').textContent = pc.hostname ? `${pc.hostname}, pornit de ${uptime(pc.uptime)}` : '';
  }

  function renderServices(svc) {
    fill('services', SERVICES.map(([k, label]) => {
      const up = !!svc[k];
      const st = kids(el('span', 'status'), el('span', 'dot' + (up ? '' : ' dot-off')), el('span', '', up ? 'pornit' : 'oprit'));
      return cell(row(label, st));
    }));
  }

  // ---------- power ----------
  function nowSec() { return Date.now() / 1000 + clockOffset; }
  function setPower(p) {
    pendingPower = p || null;
    const on = !!pendingPower;
    $('power-banner').hidden = !on;
    $('pw-shutdown-row').disabled = on;
    $('pw-restart-row').disabled = on;
    if (!on) {
      clearInterval(countdownTimer);
      countdownTimer = null;
      return;
    }
    const restart = pendingPower.kind === 'restart';
    $('power-cancel').textContent = restart ? 'Anulează repornirea' : 'Anulează oprirea';
    const tick = () => {
      if (!pendingPower) return;
      const left = Math.max(0, pendingPower.at - nowSec());
      const n = Math.ceil(left);
      $('power-text').textContent = restart ? `PC-ul repornește în ${n} s` : `PC-ul se oprește în ${n} s`;
      $('power-bar').style.width = (Math.min(1, left / 60) * 100).toFixed(1) + '%';
    };
    tick();
    if (!countdownTimer) countdownTimer = setInterval(tick, 1000);
  }

  // ---------- action sheet ----------
  let lastFocus = null;
  let sheetKind = null;
  function openSheet(kind) {
    if (pendingPower) return;
    sheetKind = kind;
    const restart = kind === 'restart';
    $('sheet-title').textContent = restart
      ? 'PC-ul repornește la 60 de secunde după ce confirmi. Poți anula până atunci.'
      : 'PC-ul se oprește la 60 de secunde după ce confirmi. Poți anula până atunci.';
    const btn = $('sheet-confirm');
    btn.textContent = restart ? 'Repornește' : 'Oprește PC-ul';
    btn.className = 'sheet-btn' + (restart ? '' : ' danger');
    const warns = (last && last.hr_warnings) || [];
    $('sheet-warn').hidden = !warns.length;
    $('sheet-warn-list').replaceChildren(...warns.map((w) => kids(el('li'), breakable(w.name, 'warn-name'), document.createTextNode(w.remaining))));
    lastFocus = document.activeElement;
    $('app').inert = true;
    $('scrim').hidden = false;
    $('sheet').hidden = false;
    requestAnimationFrame(() => requestAnimationFrame(() => {
      $('scrim').classList.add('open');
      $('sheet').classList.add('open');
    }));
    $('sheet-cancel').focus({ preventScroll: true });
  }
  function closeSheet(instant) {
    if ($('sheet').hidden) return;
    $('app').inert = false;
    $('scrim').classList.remove('open');
    $('sheet').classList.remove('open');
    const done = () => { $('scrim').hidden = true; $('sheet').hidden = true; };
    if (instant || reducedMotion.matches) done();
    else setTimeout(done, 350);
    if (lastFocus && lastFocus.focus) lastFocus.focus({ preventScroll: true });
    lastFocus = null;
  }
  $('pw-shutdown-row').addEventListener('click', () => openSheet('shutdown'));
  $('pw-restart-row').addEventListener('click', () => openSheet('restart'));
  $('sheet-cancel').addEventListener('click', () => closeSheet());
  $('scrim').addEventListener('click', () => closeSheet());
  $('sheet-confirm').addEventListener('click', () => power(sheetKind));
  document.addEventListener('keydown', (e) => {
    if ($('sheet').hidden) return;
    if (e.key === 'Escape') return closeSheet();
    if (e.key === 'Tab') { // keep focus inside the sheet
      const items = [$('sheet-confirm'), $('sheet-cancel')];
      const i = items.indexOf(document.activeElement);
      e.preventDefault();
      items[(i + (e.shiftKey ? items.length - 1 : 1)) % items.length].focus();
    }
  });

  async function power(kind) {
    closeSheet();
    try {
      const r = await api('/api/power/' + kind, {});
      setPower(r.power);
      window.scrollTo({ top: 0, behavior: reducedMotion.matches ? 'auto' : 'smooth' });
      toast(kind === 'restart' ? 'PC-ul repornește în 60 de secunde.' : 'PC-ul se oprește în 60 de secunde.');
    } catch (e) {
      if (e.message !== 'auth') toast(e.message, true);
    }
  }
  $('power-cancel').addEventListener('click', async () => {
    try {
      await api('/api/power/cancel', {});
      setPower(null);
      toast('Oprirea a fost anulată.');
    } catch (e) {
      if (e.message !== 'auth') toast(e.message, true);
    }
  });

  // ---------- monitors & settings ----------
  async function action(btn, path, okText) {
    btn.disabled = true;
    try {
      await api(path, {});
      toast(okText);
    } catch (e) {
      if (e.message !== 'auth') toast(e.message, true);
    } finally {
      setTimeout(() => { btn.disabled = false; }, 500);
    }
  }
  $('mon-off').addEventListener('click', (e) => action(e.currentTarget, '/api/monitors/off', 'Monitoarele s-au stins.'));
  $('mon-on').addEventListener('click', (e) => action(e.currentTarget, '/api/monitors/on', 'Monitoarele s-au aprins.'));
  $('auto-off').addEventListener('change', async (e) => {
    const input = e.currentTarget;
    const v = input.checked;
    try {
      await api('/api/settings', { auto_monitors_off: v });
      toast(v ? 'Monitoarele se vor stinge singure la play pe TV.' : 'Monitoarele nu se mai sting singure.');
    } catch (err) {
      input.checked = !v;
      if (err.message !== 'auth') toast(err.message, true);
    }
  });
  $('logout').addEventListener('click', async () => {
    try { await api('/api/logout', {}); } catch (e) { /* ignore */ }
    showLogin();
  });

  // ---------- service worker (HTTPS / localhost only) ----------
  if ('serviceWorker' in navigator && window.isSecureContext) {
    window.addEventListener('load', () => {
      navigator.serviceWorker.register('/sw.js', { scope: '/' }).catch(() => { /* app works without it */ });
    });
  }

  // ---------- boot ----------
  if (location.hash === '#login') showLogin(); // for screenshots of the login screen
  else refresh();
})();
