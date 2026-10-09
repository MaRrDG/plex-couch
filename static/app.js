'use strict';

(function () {
  const POLL_MS = 5000;
  const PIN_LEN = 6;
  const RING = 351.86; // 2 * PI * 56
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
  function bar(fraction, cls) {
    const b = el('div', 'bar' + (cls ? ' ' + cls : ''));
    const s = el('span');
    s.style.width = (Math.max(0, Math.min(1, fraction || 0)) * 100).toFixed(1) + '%';
    b.appendChild(s);
    return b;
  }
  function line(left, right, leftCls) {
    const r = el('div', 'row-line');
    r.appendChild(el('span', leftCls || 'title', left));
    if (right !== undefined && right !== null) r.appendChild(typeof right === 'string' ? el('span', 'aside num', right) : right);
    return r;
  }
  const note = (text, cls) => el('p', 'note' + (cls ? ' ' + cls : ''), text);
  const fill = (id, nodes) => $(id).replaceChildren(...nodes);

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
  async function api(path, body) {
    const opts = { credentials: 'same-origin', cache: 'no-store', headers: {} };
    if (body !== undefined) {
      opts.method = 'POST';
      opts.headers = Object.assign({ 'Content-Type': 'application/json' }, CSRF);
      opts.body = JSON.stringify(body);
    }
    const r = await fetch(path, opts);
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

  // ---------- login ----------
  let pin = '';
  let lockUntil = 0;
  let lockTimer = null;
  let busy = false;

  function drawDots() {
    const dots = $('pin-dots').children;
    for (let i = 0; i < dots.length; i++) dots[i].classList.toggle('on', i < pin.length);
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
    const b = $('keypad').querySelector(`[data-k="${k}"]`);
    if (b) { b.classList.add('pressed'); setTimeout(() => b.classList.remove('pressed'), 90); }
    pressKey(k);
  });

  function showLogin(msg) {
    loggedIn = false;
    stopPolling();
    closeSheet();
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
  async function refresh() {
    stopPolling();
    try {
      render(await api('/api/status'));
    } catch (e) {
      if (e.message === 'auth') return;
      $('updated').textContent = 'Nu mă pot conecta la PC.';
    }
    if (loggedIn && document.visibilityState === 'visible') timer = setTimeout(refresh, POLL_MS);
  }
  document.addEventListener('visibilitychange', () => {
    if (document.visibilityState === 'visible') { if (loggedIn) refresh(); }
    else stopPolling();
  });

  // ---------- render ----------
  function render(s) {
    last = s;
    clockOffset = (s.server_time || Date.now() / 1000) - Date.now() / 1000;
    const mode = s.demo ? 'demo' : (s.dry_run ? 'dry-run' : '');
    $('mode').textContent = mode;
    $('mode').hidden = !mode;
    renderLeds(s.services || {});
    $('auto-off').checked = !!(s.settings && s.settings.auto_monitors_off);
    renderPlaying(s.now_playing, s.services || {});
    renderDownloads(s.downloads);
    renderTorrents(s.torrents);
    renderWaiting(s.waiting);
    renderLibrary(s.library, s.services || {});
    renderDisks(s.pc || {});
    setPower(s.power);
    $('updated').textContent = 'Actualizat la ' + new Date().toLocaleTimeString('ro-RO', { hour: '2-digit', minute: '2-digit' });
  }

  function renderLeds(svc) {
    const names = [];
    fill('leds', SERVICES.map(([k, label]) => {
      const up = !!svc[k];
      names.push(`${label} ${up ? 'pornit' : 'oprit'}`);
      return el('span', 'led' + (up ? ' up' : ''));
    }));
    const text = names.join(', ') + '.';
    $('leds').setAttribute('aria-label', 'Servicii: ' + text);
    $('led-names').textContent = text;
  }
  $('leds').addEventListener('click', () => {
    const open = $('led-names').hidden;
    $('led-names').hidden = !open;
    $('leds').setAttribute('aria-expanded', String(open));
  });

  const down = (name) => `${name} nu răspunde. Pornește-l din Services sau repornește PC-ul.`;

  function renderPlaying(list, svc) {
    if (list == null || (!list.length && !svc.plex)) return fill('playing', [note(down('Plex'), 'is-warn')]);
    if (!list.length) return fill('playing', [note('Nimic pe TV acum.')]);
    fill('playing', list.map((p) => {
      const box = el('div', 'row');
      box.appendChild(el('p', 'np-title', withYear(p.title, p.year)));
      box.appendChild(el('div', 'dim', `${p.player}, ${p.state_ro || p.state}`));
      box.appendChild(bar((p.progress || 0) / 100, 'thin'));
      const where = p.duration ? `${clock(p.position)} din ${clock(p.duration)}` : `${n0.format(p.progress || 0)}%`;
      box.appendChild(el('div', 'small dim num', `${where}, ${p.decision_ro}`));
      return box;
    }));
  }

  function renderDownloads(list) {
    if (list == null) return fill('downloads', [note(down('Radarr'), 'is-warn')]);
    if (!list.length) return fill('downloads', [note('Nicio descărcare. Adaugă un film în Watchlist-ul Plex.')]);
    fill('downloads', list.map((d) => {
      const box = el('div', 'row');
      box.appendChild(el('div', 'title', withYear(d.title, d.year)));
      box.appendChild(bar(d.percent / 100, 'warn'));
      const parts = [`${n0.format(d.percent)}%`];
      if (d.time_left) parts.push(d.time_left);
      const st = lower(d.status);
      if (st && st !== 'se descarcă') parts.push(st);
      box.appendChild(el('div', 'small dim num', parts.join(', ')));
      return box;
    }));
  }

  function renderTorrents(list) {
    if (list == null) return fill('torrents', [note(down('qBittorrent'), 'is-warn')]);
    if (!list.length) return fill('torrents', [note('Niciun torrent FileList la seed.')]);
    fill('torrents', list.map((t) => {
      const box = el('div', 'row');
      if (t.hr === 'downloading') {
        box.appendChild(line(t.name, `${n0.format(t.progress)}%`));
        box.appendChild(bar(t.progress / 100));
        box.appendChild(el('div', 'small dim', `se descarcă, regula de seed începe la final`));
        return box;
      }
      const safe = t.hr === 'safe';
      box.appendChild(line(t.name, safe ? el('span', 'aside is-ok', 'gata') : null));
      box.appendChild(bar(t.rule_progress, safe ? 'ok' : 'warn'));
      const hours = n0.format(Math.min(48, Math.floor(t.seed_hours)));
      box.appendChild(el('div', 'small dim num', `ratio ${n2.format(t.ratio)}, ${hours} din 48 h`));
      return box;
    }));
  }

  function renderWaiting(list) {
    if (list == null) return fill('waiting', [note(down('Radarr'), 'is-warn')]);
    if (!list.length) return fill('waiting', [note('Nimic în așteptare. Adaugă un film în Watchlist-ul Plex.')]);
    fill('waiting', list.map((m) => {
      const box = el('div', 'row');
      box.appendChild(el('div', 'title', withYear(m.title, m.year)));
      box.appendChild(el('div', 'small dim', m.status));
      return box;
    }));
  }

  function renderLibrary(list, svc) {
    if (list == null) return fill('library', [note(down('Radarr'), 'is-warn')]);
    if (!list.length) return fill('library', [note('Biblioteca e goală. Filmele descărcate apar aici.')]);
    const rows = list.map((m) => {
      const box = el('div', 'row');
      box.appendChild(line(withYear(m.title, m.year), `${n1.format(m.size_gb)} GB`));
      if (m.ro_subs === true) box.appendChild(el('div', 'small dim', 'subtitrare RO'));
      else if (m.ro_subs === false) box.appendChild(el('div', 'small is-warn', 'fără subtitrare RO'));
      return box;
    });
    if (!svc.bazarr) rows.push(note('Bazarr nu răspunde, așa că nu știu ce subtitrări există.', 'is-warn'));
    fill('library', rows);
  }

  function renderDisks(pc) {
    const disks = pc.disks || [];
    const main = disks.find((d) => d.drive === 'D:') || disks[0];
    const nodes = [];
    if (!main) nodes.push(note('Nu pot citi discurile.'));
    else {
      const box = el('div', 'row');
      box.appendChild(line(`Disc ${main.drive}`, null));
      box.appendChild(bar(main.used_pct / 100, main.used_pct > 90 ? 'warn' : ''));
      box.appendChild(el('div', 'small dim num', `${n0.format(main.free_gb)} GB liberi din ${n0.format(main.total_gb)} GB`));
      nodes.push(box);
      for (const d of disks) {
        if (d === main) continue;
        nodes.push(el('p', 'note small num', `Disc ${d.drive} ${n0.format(d.free_gb)} GB liberi din ${n0.format(d.total_gb)} GB`));
      }
    }
    if (pc.hostname) nodes.push(el('p', 'note small', `${pc.hostname}, pornit de ${uptime(pc.uptime)}`));
    fill('disks', nodes);
  }

  // ---------- power ----------
  function nowSec() { return Date.now() / 1000 + clockOffset; }
  function setPower(p) {
    const was = pendingPower;
    pendingPower = p || null;
    const on = !!pendingPower;
    $('mon-keys').hidden = on;
    $('cancel-row').hidden = !on;
    $('ring').toggleAttribute('hidden', !on);
    $('countdown').hidden = !on;
    $('power-key').setAttribute('aria-disabled', String(on));
    if (!on) {
      clearInterval(countdownTimer);
      countdownTimer = null;
      return;
    }
    const restart = pendingPower.kind === 'restart';
    $('power-cancel').textContent = restart ? 'Anulează repornirea' : 'Anulează oprirea';
    if (!was) $('ring-fill').style.strokeDashoffset = '0';
    const tick = () => {
      if (!pendingPower) return;
      const left = Math.max(0, pendingPower.at - nowSec());
      const cd = $('countdown');
      cd.replaceChildren(document.createTextNode(String(Math.ceil(left))),
        el('small', '', restart ? 'secunde până la repornire' : 'secunde până la oprire'));
      if (!reducedMotion.matches) {
        $('ring-fill').style.strokeDashoffset = String(RING * (1 - Math.min(1, left / 60)));
      }
    };
    tick();
    if (!countdownTimer) countdownTimer = setInterval(tick, 1000);
  }

  let lastFocus = null;
  function openSheet() {
    if (pendingPower) return;
    const warns = (last && last.hr_warnings) || [];
    $('sheet-warn').hidden = !warns.length;
    $('sheet-warn-list').replaceChildren(...warns.map((w) => el('li', '', `${w.name}: ${w.remaining}`)));
    lastFocus = document.activeElement;
    $('scrim').hidden = false;
    $('sheet').hidden = false;
    $('sheet-close').focus();
  }
  function closeSheet() {
    $('scrim').hidden = true;
    $('sheet').hidden = true;
    if (lastFocus && lastFocus.focus) lastFocus.focus();
    lastFocus = null;
  }
  $('power-key').addEventListener('click', openSheet);
  $('sheet-close').addEventListener('click', closeSheet);
  $('scrim').addEventListener('click', closeSheet);
  document.addEventListener('keydown', (e) => { if (e.key === 'Escape' && !$('sheet').hidden) closeSheet(); });

  async function power(kind) {
    closeSheet();
    try {
      const r = await api('/api/power/' + kind, {});
      setPower(r.power);
      toast(kind === 'restart' ? 'PC-ul repornește în 60 de secunde.' : 'PC-ul se oprește în 60 de secunde.');
    } catch (e) {
      if (e.message !== 'auth') toast(e.message, true);
    }
  }
  $('pw-shutdown').addEventListener('click', () => power('shutdown'));
  $('pw-restart').addEventListener('click', () => power('restart'));
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

  // ---------- boot ----------
  (async function boot() {
    if (location.hash === '#login') return showLogin(); // for screenshots of the login screen
    try {
      const s = await api('/api/status');
      loggedIn = true;
      $('login').hidden = true;
      $('app').hidden = false;
      render(s);
      timer = setTimeout(refresh, POLL_MS);
    } catch (e) {
      if (e.message !== 'auth') showLogin('Nu mă pot conecta la PC. Verifică Wi-Fi-ul.');
    }
  })();
})();
