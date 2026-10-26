# Plex Couch: behavioural specification for a 1:1 Rust rewrite

Source: D:\projects\plex-couch, branch `dev` (HEAD d1082ca). Python 3.12+ stdlib only (dev box: 3.14.2).
References are `file:line`. Secrets are named by config key only.
Folded-in scripts: D:\Media\scripts\radarr-watchlist-sync.ps1 and D:\Media\scripts\strip-dv.ps1 (§12).

Glossary: "RO" = Romanian. All user-facing strings are Romanian and must be byte-identical (UTF-8, real diacritics ș ț ă î â, en dash U+2013 where noted).

---------------------------------------------------------------------------------------------------
## 1. Process model

### 1.1 CLI (app.py:871-891)
| flag | default | meaning |
|---|---|---|
| `--host` | `0.0.0.0` | bind address for every listener. IPv4 only: the stdlib server is AF_INET, so `::` would fail. |
| `--port` | `8765` (int) | plain-HTTP port |
| `--https-port` | none -> config `https.port` -> 8766 | HTTPS port |
| `--cert` / `--key` / `--ca-cert` | none -> config `https.cert/key/ca_cert` | CLI paths go through `os.path.abspath`; config paths are resolved relative to the config file's folder (app.py:126-138, config.py:178-185) |
| `--config` | `<app dir>/config.local.json` | config path |
| `--dry-run` | off | log monitor/power actions, never run them; notifications only logged unless the ntfy server is loopback |
| `--allow-http` | off | without a usable cert, serve the full app over HTTP |
| `--set-pin DIGITS` | | store the PIN hash and exit |
| `--setup-ntfy` | | create the ntfy topic if missing (or replace it with `--new-topic`), print it, exit |
| `--new-topic` | | used with `--setup-ntfy` |
| `--demo` | | fake data, no login; refuses unless `--host 127.0.0.1` |

Exit codes: `--set-pin` with a bad format prints `PIN must be 4-6 digits.` and returns 2 (app.py:897-899). Success prints `PIN saved to <abs config path>` and returns 0, after logging `PIN changed via --set-pin`.
`--demo` on a host other than 127.0.0.1 prints `--demo only runs with --host 127.0.0.1` and returns 2 (908-910). A normal run returns 0 after shutdown.

`--setup-ntfy` (app.py:852-868). The notifier is created with dry_run=True. If topic is None or `--new-topic` is given: create a new topic, save it, log `ntfy topic created|replaced via --setup-ntfy`, and print `New ntfy topic saved to <path>`. Otherwise print `Keeping the existing ntfy topic (use --new-topic to replace it).`
Then print, in order:
1. `Topic: <topic>`
2. `Subscribe to it in the ntfy app (server <server>), or open: <server>/<topic>`
3. `Anyone who knows the topic can read the notifications: keep it private.`
4. `Restart Plex Couch to start sending notifications.`

Returns 0.

### 1.2 Startup order (app.py:893-947)
1. `setup_logging()` (§1.5).
2. `Config(path)`: load JSON (UTF-8 with optional BOM). Malformed JSON raises and the process dies.
3. Handle `--set-pin`, then `--setup-ntfy` (these exit). Then the `--demo` host check.
4. `App(config, dry_run, demo)` (app.py:159-178):
   - `config.ensure_secret()` generates and persists `secret` if it is missing. This creates or writes the config file on first run.
   - Builds Services and Actions (`dry_run = dry_run or demo`).
   - A thread pool of 8 workers (`svc` prefix).
   - The Notifier.
   - The StateFile `state.local.json`, kept in the config file's directory.
   - `LoginGuard`, with known IPs loaded from state `known_ips`.
   - `_pin_lock`: one PBKDF2 check at a time.
   - `_login_slots`: a bounded semaphore of 4.
   - `_disk_drive = "D:"`.
   - `_after_movie = None`, held in memory only.
5. Resolve TLS settings and `load_tls` (app.py:148-155):
   - Cert or key is not an existing file: `(None,"missing")`.
   - Load error (SSLError/OSError): `(None,"unreadable (<ExcType>)")`.
   - Otherwise a TLS server context with minimum TLS 1.2 and default ciphers, no ALPN, no SNI handling. Certs are not reloaded at runtime.
6. Listener selection (app.py:917-935):
   - **Context OK**: `servers=[TLSServer(host,https_port,SecureHandler), Server(host,port,InstallHandler)]`.
   - **No context, but `--allow-http` or `--demo`**: `[Server(host,port,Handler)]`. Logs WARNING `HTTPS certificate/key <problem>; serving the app over plain HTTP (--allow-http)` (skipped in demo).
   - **Otherwise**: `[Server(host,port,NoCertHandler)]`. Logs ERROR `HTTPS certificate/key <problem>: serving only an error page on http port <port>. Run tools/make_certs.py (or start with --allow-http for plain HTTP).` and prints a similar line to stdout.
   - Note: in demo mode with certs present, TLS is still used.
7. `stop = Event()`. Unless demo, start daemon threads `auto-monitor` (§6.6) and `notify` (§7.4).
8. Each listener after the first runs `serve_forever` in a daemon thread named `http`. The first listener runs on the main thread.
9. Log INFO `started on <where>[ (demo)| (dry-run)]`. `where` is one of:
   - `https H:P, http H:P (install page)`
   - `http H:P (no certificate, HTTPS off, --allow-http)`
   - `http H:P (certificates missing: error page only)`
10. If `pin_hash` is empty: WARNING `no PIN set; run app.py --set-pin <digits>`.
11. On KeyboardInterrupt (the only shutdown path; there are no signal handlers, and under pythonw the process is simply killed):
    - `stop.set()`
    - `shutdown()` the secondary servers
    - `server_close()` all servers
    - log `stopped`

A bind failure (port in use) raises before the try block, so the process dies with nothing logged.

pythonw has no console: stdout/stderr are redirected to devnull when they are None (app.py:53-56).

### 1.3 Listeners and connection handling (app.py:791-849)
- Threaded server with daemon threads, `allow_reuse_address=False`.
- **Connection cap**: at most `MAX_CONNECTIONS=64` concurrent connections per listener (bounded semaphore). An extra connection is closed immediately without a response (app.py:802-817).
- **Per-socket-operation timeout** `CONNECTION_TIMEOUT=15` s. This includes the TLS handshake and keep-alive idle time (Handler.timeout, app.py:503; TLSServer.finish_request sets it before wrap, app.py:837).
- **TLS handshake** runs in the request thread, not the accept loop. A failed handshake is logged at DEBUG `tls handshake failed from <ip>: <ExcType>` and the connection is dropped.
- **Errors** (`handle_error`, app.py:819-825): SSLError, ConnectionError and socket.timeout are logged at DEBUG only. Anything else logs ERROR with a traceback, `request error from <ip>`, and the connection is dropped with no response. This includes uncaught handler exceptions, e.g. a ctypes failure.
- HTTP/1.1 with keep-alive. The `Server` header is `PlexCouch` (sys_version is empty, so Python emits `PlexCouch ` with a trailing space). A `Date` header is sent.
- Methods: GET, HEAD (= GET without body) and POST. Any other method gets the stdlib `501 Unsupported method` HTML error page with no security headers and no IP/Host checks. The stdlib also sends 400/414/431 pages for malformed requests. Keep or tighten these; it's your call.

### 1.4 Background loops and intervals
| loop | thread | interval | started |
|---|---|---|---|
| auto monitors off + after-movie polling | `auto-monitor` | first poll immediately, then `max(3, int(plex_poll_seconds or 10))` s (default 10) | not in demo |
| notifications (movie ready, disk low) | `notify` | first tick immediately, then every `NOTIFY_POLL_SECONDS=60` s; skipped while notifier disabled | not in demo |
| power timer | `threading.Timer` | `POWER_DELAY=60` s after a power request | on demand |
| async notification send | thread `notify` | one-shot | after-movie fire |
| own IPv4 cache | lazy | refreshed if older than 300 s | on use |

### 1.5 Logging (app.py:95-99)
- Logger `plexcouch`, level INFO, so DEBUG lines are dropped.
- RotatingFileHandler on `<app dir>/plex-couch.log`, maxBytes 1,000,000, backupCount 2 (`.log.1`, `.log.2`), UTF-8.
- Format `%(asctime)s %(levelname)s %(message)s`. Python asctime looks like `2026-10-26 18:01:02,123`.
- The per-request access log is disabled (log_message is a no-op).
- Never log: the ntfy topic, the ntfy server URL in failures, API keys or tokens, tracker URLs, the Host header value, config contents or the PIN.

---------------------------------------------------------------------------------------------------
## 2. Configuration

### 2.1 Defaults and keys (config.py:10-68; config.example.json)
The file is deep-merged over the defaults (`_merge`, config.py:74-81). Dicts merge recursively; any other value, lists included, replaces the default.

| key | default | notes |
|---|---|---|
| `app_name` | `"Plex Couch"` | `str(app_name or default)` (config.py:175) |
| `pin_hash` | `""` | written by `--set-pin` |
| `secret` | `""` | cookie-signing secret; on first run `secrets.token_hex(32)` (64 hex chars) is written |
| `settings.auto_monitors_off` | `true` | the only user setting (`SETTINGS_KEYS={"auto_monitors_off": bool}`) |
| `filelist_tracker_regex` | `thefl\.org\|filelist\.(io\|ro)\|flro\.org` | case-insensitive, unanchored search |
| `disks` | `["C:","D:"]` | drives shown in `pc.disks` |
| `plex_poll_seconds` | `10` | auto-monitor interval (min 3) |
| `allowed_hosts` | `[]` | extra Host names; non-strings ignored (config.py:157-159) |
| `session_epoch` | `1` | read as `max(1,int(x or 1))`, invalid gives 1 (config.py:138-142) |
| `notifications.server` | `"https://ntfy.sh"` | trailing `/` stripped |
| `notifications.topic` | `null` | secret-ish; whitespace-only counts as unset |
| `notifications.disk_min_gb` | `60` | |
| `notifications.ready_wait_minutes` | `20` | 0 or missing means 0 (immediate) |
| `https.port` | `8766` | |
| `https.cert` / `https.key` / `https.ca_cert` | `certs/server.pem` / `certs/server.key` / `certs/ca.crt` | relative to the config folder; `~` and `%VAR%`/`$VAR` expanded (config.py:178-185) |
| `services.plex.url` | `http://127.0.0.1:32400` | |
| `services.plex.token` | `""` | if empty: registry HKCU `token_registry_key` / `token_registry_value` |
| `services.plex.token_registry_key` | `Software\Plex, Inc.\Plex Media Server` | |
| `services.plex.token_registry_value` | `PlexOnlineToken` | |
| `services.radarr.url` | `http://127.0.0.1:7878` | |
| `services.radarr.api_key` | `""` | if empty: parsed from `config_xml` |
| `services.radarr.config_xml` | `C:\ProgramData\Radarr\config.xml` | |
| `services.bazarr.url` | `http://127.0.0.1:6767` | |
| `services.bazarr.api_key` | `""` | if empty: parsed from `config_yaml` |
| `services.bazarr.config_yaml` | `C:\ProgramData\Bazarr\config\config.yaml` | |
| `services.prowlarr.url` | `http://127.0.0.1:9696` | |
| `services.qbittorrent.url` | `http://127.0.0.1:8080` | |
| `services.qbittorrent.username` / `password` | `""` | empty gives the login fallback `admin` / `x` |

`config.example.json` also has `_comment` (ignored). The current config.local.json holds only `pin_hash`, `secret`, `settings`, `notifications`.

### 2.2 Validation
There is essentially no validation. Bad types would raise where used, e.g. `int(https.port)`. Only `allowed_hosts`, `session_epoch` and `app_name` are coerced.

### 2.3 Writes (config.py:100-168)
The file is written only from the raw user dict (`_raw`, not the merged defaults) as `json.dump(indent=2, ensure_ascii=False)` plus `"\n"`, atomically: write `<path>.tmp`, then `os.replace`. The directory is created if missing. After each write `data` is re-merged.

| writer | trigger | change |
|---|---|---|
| `ensure_secret` | every App start | adds `secret` only if missing |
| `set_pin_hash` | `--set-pin` | sets `pin_hash`; adds `secret` if missing; `settings.auto_monitors_off` set to true if absent |
| `set_notification_topic` | `--setup-ntfy` | `notifications.topic`; creates the `notifications` dict if it isn't a dict |
| `bump_session_epoch` | POST /api/logout-all | `session_epoch = max(1,int(raw or 1)) + 1` |
| `update_settings` | POST /api/settings | `settings[k] = bool(v)` for known keys |

The config is loaded once at startup. Hand edits, including a topic set by a separate `--setup-ntfy` run, need a restart.

### 2.4 Secrets sources (services.py:104-163)
- Keys are cached per service for `KEY_TTL=300` s (monotonic clock). **The empty string is cached too** when reading fails; WARNING `cannot read <name> key: <ExcType>`.
- **Plex**: `services.plex.token`, else `HKEY_CURRENT_USER\<token_registry_key>` value `<token_registry_value>`, stringified.
- **Radarr**: `api_key`, else the regex `<ApiKey>\s*([^<\s]+)\s*</ApiKey>` over `config_xml` (utf-8-sig). No match gives `""`.
- **Bazarr**: `api_key`, else a line parser over `config_yaml` (services.py:110-124):
  1. Skip blank lines and lines whose stripped text starts with `#`.
  2. A non-indented line sets `in_auth = (line minus any #comment, stripped) == "auth:"`.
  3. In auth, an indented line starting (after strip) with `apikey:` returns the value: the text after the first `:`, stripped, then `'` and `"` stripped.
  4. Otherwise `""`.
- **qBittorrent**: username/password from config only.
- Prowlarr: no key.

### 2.5 State file `state.local.json` (notify.py:121-162)
- Lives next to the config file (`os.path.dirname(config.path)`, app.py:170).
- A missing file, invalid JSON or a non-dict reads as `{}`.
- Writes: `json.dump(indent=2, ensure_ascii=False, sort_keys=True)` plus `"\n"`, atomic via `.tmp` + replace. On failure, WARNING `cannot save the state file: <ExcType>`.
- `update(changes)` merges top-level keys under one lock (load, merge, save), so writers never clobber each other's keys.

Keys:
- `known_ips`: `{ "<ip>": <int unix seconds of last successful login> }`. Written on every successful login (app.py:241). Loaded at start, dropping entries ≥ 30 days old and non-str/non-number entries (logic.py:416-422).
- `movie_ready`: `{ "primed": bool, "done": ["<radarr id>", ...] (sorted numerically, non-digits after), "pending": { "<id>": <float unix first seen with file> } }`.
- `disk_low`: `{ "<drive>": bool }`, e.g. `{"D:": false}`.

`movie_ready` and `disk_low` are written in each notify tick before sending (app.py:442).

---------------------------------------------------------------------------------------------------
## 3. HTTP surface

### 3.1 Common response rules (app.py:85-92, 516-536)
Every response from `_send` carries:
- `X-Content-Type-Options: nosniff`
- `X-Frame-Options: DENY`
- `Referrer-Policy: no-referrer`
- `Content-Security-Policy: default-src 'self'; img-src 'self' data:; style-src 'self'; script-src 'self'; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'`
- `Strict-Transport-Security: max-age=31536000`, **only on the HTTPS listener** (SecureHandler), on every response, including 403 and 421.
- `Content-Type`, `Content-Length`.
- `Cache-Control`: if the caller passed none, `no-store` for `application/json*`, else `no-cache`.
- Any extra headers.
- If a session renewal is pending and the extra headers contain no `Set-Cookie`: `Set-Cookie: <renewed cookie>`.

HEAD sends the same headers without a body.
JSON is `json.dumps(obj, ensure_ascii=False)` encoded UTF-8 (raw diacritics), with keys in insertion order. Content-Type is `application/json; charset=utf-8`.

### 3.2 Preflight on every listener, before anything else (app.py:560-580)
1. **Client IP**: `is_private_client(peer_ip)` (§4.6). On failure: WARNING `rejected non-private client <ip>`, then **403** JSON `{"error":"Acces permis doar din rețeaua locală."}` with `Connection: close`.
2. **Host allowlist** (§4.7). On failure: WARNING `rejected Host header from <ip>` (the value is never logged), then **421** body `Gazdă nepermisă.` with `text/plain; charset=utf-8` and `Connection: close`. A missing or empty Host also gets 421.
3. The renewal slot is reset.

The path is `self.path.split("?",1)[0]`, with no percent-decoding and no normalization.

### 3.3 Main app handler (`Handler`/`SecureHandler`: HTTPS listener, or HTTP with --allow-http)

**GET/HEAD** (app.py:594-627):
- `path` starts with `/api/`:
  - Auth with renewal. If invalid: **401** `{"error":"Autentificare necesară."}`.
  - `/api/status`: **200** status JSON (§3.5).
  - Any other `/api/...`: **404** `{"error":"Necunoscut."}`, after auth (unauthenticated gets 401).
- `path` starts with `/img/`: image proxy (§3.6).
- Static whitelist (app.py:61-76). Exact match only; files are read from `static/` on every request:

| URL | file | Content-Type |
|---|---|---|
| `/`, `/index.html` | index.html | `text/html; charset=utf-8` |
| `/app.js` | app.js | `text/javascript; charset=utf-8` |
| `/style.css` | style.css | `text/css; charset=utf-8` |
| `/tokens.css` | tokens.css | `text/css; charset=utf-8` |
| `/fonts/figtree-latin.woff2`, `/fonts/figtree-latin-ext.woff2` | fonts/... | `font/woff2` |
| `/sw.js` | sw.js | `text/javascript; charset=utf-8`, plus header `Service-Worker-Allowed: /` |
| `/manifest.webmanifest` | manifest.webmanifest | `application/manifest+json; charset=utf-8` |
| `/icon.svg` | icon.svg | `image/svg+xml` |
| `/icon-192.png`, `/icon-512.png`, `/icon-maskable-512.png`, `/apple-touch-icon.png` | same | `image/png` |

  - Templated files: `index.html` and `manifest.webmanifest` have `{{APP_NAME}}` replaced. For `.html` the value is HTML-escaped (`&`, `<`, `>`, `"`; app.py:102-104). For the manifest it is JSON-string-escaped without the quotes (`json.dumps(name)[1:-1]`, ASCII-escaped since json.dumps defaults to ensure_ascii=True).
  - Static responses get `Cache-Control: no-cache` and need no auth.
  - Anything else: **404** body `Not found`, `text/plain; charset=utf-8`.
  - `install.html` and `fonts/OFL.txt` are never served by this handler.

**POST** (app.py:630-699). Order:
1. Preflight.
2. CSRF: header `X-Requested-With` must equal exactly `plex-couch`. Otherwise WARNING `csrf header missing on <path> from <ip>` and **403** `{"error":"Cerere respinsă (CSRF)."}`. This applies to `/api/login` too.
3. Body (app.py:548-558): `Content-Length` (non-int is treated as 0).
   - ≤0 gives `{}`.
   - Over `MAX_BODY=4096` gives **400** `{"error":"Cerere invalidă."}`. The body is not read (see gotchas).
   - Otherwise UTF-8 JSON (an empty string is treated as `{}`). A JSON/Unicode error gives the same 400. A non-object gives `{}`.
   - Chunked bodies are not supported.
4. `/api/login` is handled here, without auth (§4.3).
5. Auth with renewal. If invalid: **401** `{"error":"Autentificare necesară."}`.
6. Routes. All log INFO `action <name> from <ip>` except settings, which logs after the change.

| path | behaviour | response |
|---|---|---|
| `/api/logout` | clear the cookie | 200 `{"ok":true}` + `Set-Cookie: pc_session=; Max-Age=0; HttpOnly; SameSite=Strict; Path=/[; Secure]` |
| `/api/logout-all` | `logout_all()`: bump the epoch, log `all sessions revoked (session epoch N)`; no-op in demo | 200 `{"ok":true}` + the clearing cookie |
| `/api/monitors/off` | `actions.monitors_off()` | 200 `{"ok":<bool>,"dry_run":<bool>}`, always 200 |
| `/api/monitors/on` | `actions.monitors_on()` | 200 `{"ok":<bool>,"dry_run":<bool>}` |
| `/api/power/shutdown`, `/api/power/restart` | `actions.power(kind)` | 200 `{"ok":true,"power":{"kind":k,"at":<int unix>},"dry_run":b}`; if it returns None (already pending): **500** `{"error":"Comanda de oprire a eșuat (poate există deja una programată)."}` |
| `/api/power/cancel` | `actions.cancel_power()` | outcome `too_late`: **409** `{"error":"Prea târziu: comanda pornise deja și nu a mai putut fi anulată.","outcome":"too_late"}`; else 200 `{"ok":true,"power":null,"outcome":"cancelled"\|"aborted"\|"nothing"}` |
| `/api/power/after-movie` | `arm_after_movie()` (§6.4) | 200 `{"ok":true,"power":<power_status>}` / 409 / 503 |
| `/api/power/after-movie/cancel` | disarm (log `after-movie shutdown disarmed` if it was armed) | 200 `{"ok":true,"power":<power_status or null>}` |
| `/api/notify/test` | §7.3 | 200 `{"ok":true}` / 409 / 502 |
| `/api/settings` | body `auto_monitors_off` must be a JSON bool; else **400** `{"error":"Setare invalidă."}`. Demo updates memory only. Otherwise persist and log `settings changed {'auto_monitors_off': X} from <ip>` (Python repr) | 200 `{"ok":true,"settings":{...all settings keys...}}` |
| anything else | | **404** `{"error":"Necunoscut."}` |

On logout and logout-all, the explicit `Set-Cookie` overrides any renewal cookie.

### 3.4 Install listener (`InstallHandler`, plain HTTP while HTTPS is on; app.py:713-760)
- GET/HEAD `/`: install page (`static/install.html`).
  - `{{APP_NAME}}` is replaced with the HTML-escaped name and `{{HTTPS_URL}}` with the HTML-escaped `https_url(Host, https_port, "/")`.
  - Headers: the security headers **except** the normal CSP, replaced by the install CSP: `default-src 'none'; style-src 'sha256-<b64 sha256 of the bytes between the first <style> and </style>>'; img-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'`. If there is no `<style>`, `style-src 'none'`.
  - `Content-Type: text/html; charset=utf-8`, `Cache-Control: no-cache`. No HSTS. No session renewal.
- GET `/ca.crt`: reads the `ca_cert` file and converts PEM to DER if the content (after lstrip) starts with `-----BEGIN` (first cert, base64 between header and footer). On OSError/ValueError: **404** `Not found`. Otherwise 200 with `Content-Type: application/x-x509-ca-cert` and `Content-Disposition: attachment; filename="plex-couch-ca.crt"` (Cache-Control `no-cache`).
- Any other GET/HEAD path and **every POST** (body unread, no CSRF check): **301** with `Location: https_url(Host, https_port, self.path)`, where self.path includes the query. Empty body, `text/plain; charset=utf-8`.
- `https_url` (logic.py:1043-1054):
  - The host comes from `split_host_header(Host)`. If that is None, the fallback is the local socket address (`getsockname()[0]`), bracketed if it contains `:`.
  - The path becomes `/` if empty, if it doesn't start with `/`, or if it contains any char with ord < 33 or == 127. A leading `//...` collapses to `/...`.
  - Port 443 is omitted from the URL.

### 3.5 GET /api/status response (app.py:257-313)
The 7 upstream jobs run concurrently on the pool. There is one shared deadline of 6 s; each `future.result(timeout=max(0.1, remaining))`. Timed-out futures are not cancelled. Failures are recorded by exception type at DEBUG `status: <name> failed: <Type>`.
Jobs: `plex` (plex_sessions), `radarr_queue`, `radarr_movies`, `bazarr`, `prowlarr`, `qbittorrent` (filelist_torrents), `radarr_history`.

```jsonc
{
  "app_name": "Plex Couch",
  "server_time": 1761500000.123,          // float unix seconds
  "dry_run": false,
  "pc": {"hostname": "PC", "uptime": 12345 /*int s or null*/, "disks": [
          {"drive":"C:","free_gb":181.4,"total_gb":930.5,"used_pct":80.5}], "platform": "win32"},
  "services": {"plex":bool, "radarr":bool /*movies OR queue ok*/, "prowlarr":bool, "bazarr":bool, "qbittorrent":bool},
  "now_playing": [ /* [] when Plex is down (NOT null) */
     {"title","year","player","product","state","state_ro","progress","position","duration",
      "decision","decision_ro","transcode","rating_key","poster","backdrop","on_tv"} ],
  "downloads": null | [ {"title","year","percent","time_left","status","movie_id","radarr_id","poster"} ],
  "waiting":   null | [ {"title","year","status","downloading","radarr_id","poster"} ],
  "library":   null | [ {"title","year","size_gb","ro_subs" /*true|false|null*/,"radarr_id","poster"} ],
  "torrents":  null | [ {"title","name","state","state_ro","progress","ratio","seed_hours","hr","safe",
                        "rule_progress","remaining","dlspeed","upspeed","eta","size"} ],
  "hr_warnings": [ {"title","name","remaining"} ],   // always a list
  "power": null | {"kind":"shutdown"|"restart","at":int, "after_movie": null|{"armed":true,"title":str,"since":int}}
           //   or only {"after_movie": {...}} when armed with no countdown
  "settings": {"auto_monitors_off": bool},
  "notifications": {"enabled": bool, "topic": str|null, "server": str|null /*null when disabled*/}
}
```

How each field is built:
- `downloads`/`waiting`/`library` are null when the corresponding Radarr call failed: queue for downloads, movies for waiting and library.
- `torrents` is null when qBittorrent failed.
- Torrent titles: `torrent_title_map(history, queue, movies)`, then `summarize_torrent(t, torrent_title(t, map))` for each FileList torrent. Sort key: (`pending`=0, `downloading`=1, `safe`=2), then title lowercased.
- `ro_map = bazarr_ro_map(bazarr)` if Bazarr is OK, else None.
- `queue_ids` = set of queue `movieId`s.
- `now_playing[i] = with_backdrop_fallback(public_session(s), movies)` plus `on_tv = is_tv_player(s.address, own_ips)`.
- `power` and `notifications`: §6, §7.
- Demo: §8.

### 3.6 Image proxy `/img/<kind>/<id>` (app.py:617-627, logic.py:217-230, services.py:87-101, 178-235)
- Auth required, **without** renewal; invalid gives 401 JSON.
- Path must fully match `^/img/(movie|plex|plex-art|movie-art)/([0-9]{1,10})$`. ASCII digits only; no query (the query was already stripped), no trailing slash. A non-match gives **404** `Not found` text/plain, and no upstream call is made.
- Kinds:
  - `movie`: Radarr `/api/v3/mediacover/<id>/poster-500.jpg`, then `poster.jpg`.
  - `movie-art`: Radarr `fanart-360.jpg`, then `fanart.jpg`.
  - `plex`: Plex photo transcoder, 240×360, `thumb`.
  - `plex-art`: Plex photo transcoder, 720×405, `art`.
- Upstream fetch:
  - Headers `Accept: image/*` plus the key header. Timeout 6 s.
  - Read at most 4 MiB + 1 byte. An empty body or one over 4 MiB counts as no image.
  - Content-Type is the upstream type (before `;`, lowercased) if it starts with `image/`, else `image/jpeg`.
  - URLError/OSError/ValueError (HTTP errors and refused redirects included) count as no image; DEBUG `image fetch failed: <Type>`.
- Found: **200** with the upstream bytes and type, plus `Cache-Control: private, max-age=86400`. Nothing found: **404** `Not found`.

### 3.7 No-cert listener (`NoCertHandler`, app.py:763-788)
After preflight, GET, HEAD and POST all return **503** with `text/html; charset=utf-8`, `Retry-After: 300` and `Connection: close`. The body is never read. The body (`{name}` = HTML-escaped app_name):
```html
<!doctype html>
<html lang="ro">
<head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>{name}</title></head>
<body>
<h1>Certificatele HTTPS lipsesc</h1>
<p>{name} nu pornește fără HTTPS: certificatul sau cheia lipsesc ori nu pot fi citite.</p>
<p>Pe PC, rulează <code>python tools/make_certs.py --ip &lt;IP-ul PC-ului&gt;</code> și repornește {name}.</p>
</body>
</html>
```

### 3.8 What the frontend calls (static/app.js, sw.js)
- `GET /api/status`, polled every 5 s while visible. Client timeout 8 s.
- POSTs, all with `{}` body except login and settings, headers `Content-Type: application/json` and `X-Requested-With: plex-couch`, `credentials: same-origin`, `cache: no-store`:
  - `/api/login {pin}`
  - `/api/logout`
  - `/api/logout-all`
  - `/api/monitors/off`
  - `/api/monitors/on`
  - `/api/power/shutdown`
  - `/api/power/restart`
  - `/api/power/cancel`
  - `/api/power/after-movie`
  - `/api/power/after-movie/cancel`
  - `/api/notify/test`
  - `/api/settings {auto_monitors_off: bool}`
- `<img>` for the `poster`/`backdrop` URLs. The client only accepts `^/img/(movie|plex|movie-art|plex-art)/\d+$`.
- What the client relies on:
  - 401 on anything except login shows the login screen.
  - 503 with a non-JSON body means offline.
  - `error` text is shown in toasts.
  - Login failures: `lockout_seconds` and `attempts_left`.
  - `power.kind`/`power.at` drive the countdown; `power.after_movie.armed`/`title` drive the after-movie banner.
  - `server_time` sets the clock offset.
  - `notifications.enabled`/`topic`.
  - `pc.disks` (the `D:` disk is preferred), `pc.hostname`, `pc.uptime`.
  - `demo`, `dry_run`.
  - `now_playing[].on_tv`.
  - The `downloads` `radarr_id`/`movie_id` join with `waiting`.
- Service worker:
  - Cache `plex-couch-v4` holds the shell `/`, `/app.js`, `/style.css`, `/tokens.css`, both fonts, the manifest and the icons, served stale-while-revalidate. Navigations map to `/`.
  - `/api/*` is never cached.
  - `/img/*` is cache-first in `plex-couch-img-v1` (max 150 entries; only 200 responses with an `image/*` type are stored).
  - The offline fallback is a 503 `PC-ul nu răspunde.` text/plain.
  - The SW is registered only in a secure context, with scope `/`.

---------------------------------------------------------------------------------------------------
## 4. Auth and security

### 4.1 PIN hashing (logic.py:302-327)
- Format: `pbkdf2_sha256$<iterations>$<salt hex>$<dk hex>`. PBKDF2-HMAC-SHA256 over the PIN's UTF-8 bytes with a 16-byte random salt (32 hex chars). Default iterations are 200 000; dk length is 32 bytes (the SHA-256 default).
- Verify:
  1. Split on `$` into exactly 4 parts.
  2. Algorithm must equal `pbkdf2_sha256`.
  3. Iterations must parse as an int; salt and hash as hex. Any failure means false.
  4. Recompute with the stored iterations and compare in constant time.
  - A None or empty PIN is hashed as "".
- PIN format for `--set-pin`: 4-6 ASCII digits only (Unicode digits are rejected).
- The login endpoint does not check the format: any string of ≤ 64 chars is verified. A non-string or a longer string counts as a wrong PIN and still uses up an attempt.

### 4.2 Sessions (logic.py:330-377; app.py:181-208, 582-588)
- Key: `HMAC-SHA256(key = secret UTF-8, msg = pin_hash UTF-8 (or ""))`, raw 32 bytes. Changing the PIN or the secret invalidates every token.
- Token: `"<epoch>.<expiry>.<sig>"`.
  - `epoch` is the decimal config session_epoch.
  - `expiry = int(now + 30*86400)`.
  - `sig = hex(HMAC-SHA256(key, "<epoch>.<expiry>" ASCII))`, lowercase, 64 chars.
- Validity:
  1. The token is a string with exactly 2 dots.
  2. The epoch part is ASCII digits, ≤ 9 chars; the expiry part is ASCII digits, ≤ 12 chars.
  3. The signature compares equal in constant time (non-ASCII is replaced, so it fails).
  4. `int(epoch) == current epoch`.
  5. `expiry > now`.
  - Old 2-part tokens are rejected.
- If `pin_hash` is empty, every session is invalid. In demo every session is valid.
- **Sliding renewal**: when a valid token has less than 15 days left (`expiry - now < 15*86400`), any authenticated `/api/*` GET or POST response carries `Set-Cookie` with a fresh 30-day token. `/img/*` never renews.
- Cookie (logic.py:1057-1060): `pc_session=<v>; Max-Age=<s>; HttpOnly; SameSite=Strict; Path=/` plus `; Secure` on the HTTPS listener only. Login and renewal use Max-Age 2592000; logout uses value `""` with Max-Age 0.
- Cookie read: standard Cookie header parse. A parse error means no token.
- Logout-everywhere: `session_epoch += 1`, persisted, so every older token fails the epoch check. New tokens start with the new epoch, e.g. `2.`.
- Successful login: 200 `{"ok":true}` plus Set-Cookie. In demo: 200 `{"ok":true}` and **no** cookie.

### 4.3 Login (app.py:210-248) and LoginGuard (logic.py:384-553)
Login steps:
1. Demo: 200 `{"ok":true}`.
2. No `pin_hash`: **503** `{"error":"PIN-ul nu este setat. Rulează app.py --set-pin."}`.
3. `ticket, wait = guard.reserve(ip)`. If `wait > 0`: **429** `{"error":"Prea multe încercări. Așteaptă <wait> secunde.","lockout_seconds":<wait>}`.
4. Non-blocking acquire of one of the 4 login slots. If none is free: refund the ticket, DEBUG log, **429** with lockout body `wait=5` (busy_retry).
5. If the PIN is a str of ≤ 64 chars: verify under the global `_pin_lock` (one PBKDF2 at a time). Release the slot in all cases. An exception refunds the ticket and re-raises.
6. Correct: `guard.succeed`, write `known_ips` to the state file, INFO `login ok from <ip>`, 200 plus cookie.
7. Wrong: `locked = guard.fail(ticket)`, WARNING `login failed from <ip>`. If locked > 0: **429** lockout body. Else **401** `{"error":"PIN greșit.","attempts_left":<n>}`.

Guard parameters: per_ip_max 5, global_max 20, lock_seconds 600, window_seconds 3600, global_delay 60, known_days 30, busy_retry 5. All waits are integer seconds, `ceil(...)`.

Per-IP state is `{fails, inflight, locked_until}`. The entry is deleted when all three are 0/expired.

`_global` is a list of timestamps of **failed or in-flight** attempts. Success and refund remove their timestamp; fail keeps it.

`known` maps ip to the time of its last success, pruned at ≥ 30 days.

`reserve(ip, now)`, all under one mutex:
1. `left = max(ip_lock_left, global_left)`. If > 0, return `(None, left)`.
   - `ip_lock_left = ceil(locked_until - now)` if positive.
   - `global_left`: prune `_global` to timestamps `> now-3600`. If the IP is known, or `len < 20`, it is 0. Otherwise `ceil(max(_global) + 60 - now)` if positive.
2. If `fails + inflight >= 5`, return `(None, 5)`. The in-flight checks decide the lockout.
3. Otherwise `inflight += 1`, append `now` to `_global`, return the ticket `(ip, now)`.

`succeed`: inflight -= 1 (min 0), remove the ticket time from `_global`, `fails = 0`, `locked_until = 0`, `known[ip] = now`, prune known, clean up the entry.

`fail`: inflight -= 1, `fails += 1`. If `fails >= 5`: `locked_until = now + 600` and `fails = 0`. Returns `ceil(locked_until - now)` if positive, else 0. **Fails never decay with time**; they reset only on success or lockout.

`refund`: inflight -= 1, remove the timestamp from `_global`.

`attempts_left = max(0, 5 - fails - inflight)`.

Effect: parallel bursts can never get more than 5 PIN checks per lockout window.

### 4.4 CSRF
Every POST must carry the exact header `X-Requested-With: plex-couch`. There is no Origin/Referer check; SameSite=Strict, a strict CSP and the Host allowlist complete the picture.

### 4.5 Redirect policy
The main app never redirects. The install listener answers everything except `/` and `/ca.crt` with a 301 to HTTPS.

### 4.6 Client IP rule `is_private_client` (logic.py:237-246)
1. Strip a `%zone` suffix and parse the IP. Garbage or empty means **reject**.
2. Unwrap IPv4-mapped IPv6 (`::ffff:a.b.c.d`).
3. Allow if Python's `is_private or is_loopback`.

Python 3.12+ `is_private` (replicate exactly; Rust std is narrower):
- IPv4 private: 0.0.0.0/8, 10/8, 127/8, 169.254/16, 172.16/12, 192.0.0.0/29, 192.0.0.170/31, 192.0.2.0/24, 192.168/16, 198.18/15, 198.51.100/24, 203.0.113/24, 240/4, 255.255.255.255/32. Exceptions that are *not* private: 192.0.0.9/32, 192.0.0.10/32.
- IPv4 not private: 100.64/10 and 224/4.
- IPv6 private: ::1/128, ::/128, ::ffff:0:0/96, 64:ff9b:1::/48, 100::/64, 2001::/23 (exceptions not private: 2001:1::1/128, 2001:1::2/128, 2001:3::/32, 2001:4:112::/48, 2001:20::/28, 2001:30::/28), 2001:db8::/32, 2002::/16, 3fff::/20, fc00::/7, fe80::/10.

Tests require: allowed 127.0.0.1, ::1, 10.0.0.5, 172.16.3.4, ::ffff:10.1.2.3, fd00::1, 169.254.1.1; rejected 8.8.8.8, 1.1.1.1, 2001:4860:4860::8888, ::ffff:8.8.8.8, "", garbage, None.

### 4.7 Host allowlist (logic.py:277-295, 1012-1040)
`split_host_header(h)`:
1. Strip. Empty or more than 255 chars gives None.
2. Bracketed form: find `]` (missing gives None). The rest must be empty or `:\d{1,5}`. The inner part, with any `%zone` stripped, must be a valid IPv6 address. Return `[inner]`, zone kept.
3. Otherwise, if there is exactly one `:`, split host and port; the port must be 1-5 digits. With 0 or ≥ 2 colons, the whole string is the host.
4. An empty host gives None.
5. Host must match `^[A-Za-z0-9](?:[A-Za-z0-9-]{0,62})(?:\.[A-Za-z0-9-]{1,63})*\.?$`. Return it lowercased, or None.

`host_allowed`:
1. None gives false; a bracketed host gives true.
2. Strip trailing dots.
3. A strict dotted-quad IPv4 (Python rejects leading zeros and the "2130706433" form) gives true.
4. `localhost`, or a name ending `.local` and longer than `.local`, gives true.
5. Otherwise true only if the name is in `allowed_hosts` (each entry stripped, lowercased, trailing dot removed; non-strings and empty entries dropped).

Test vectors in §10.

### 4.8 Upstream "never follow redirects when a key is attached" (services.py:47-75)
- All Plex/Radarr/Bazarr requests and qBittorrent requests use a custom opener.
- If any request header name, case-insensitive, is `x-api-key` or `x-plex-token`, any redirect response raises `RedirectRefused` (a URLError subclass) with WARNING `upstream redirect refused (request carries a key)`.
- Requests without such headers follow redirects normally. This covers qBittorrent, the Prowlarr ping (plain urlopen), and ntfy (plain urlopen; the topic is in the body).

---------------------------------------------------------------------------------------------------
## 5. Integrations
Default timeout is `TIMEOUT=2.5` s. JSON GETs add `Accept: application/json`; an empty body gives None (services.py:78-84).

### 5.1 Plex (local server)
- Sessions: `GET {plex.url}/status/sessions`, with `X-Plex-Token: <token>` only if the token is non-empty. Not cached.
- Photo: `GET {plex.url}/photo/:/transcode?width=W&height=H&minSize=1&upscale=1&url=%2Flibrary%2Fmetadata%2F<rk>%2F<thumb|art>`. The query is urlencoded in that order. The rating key must be ASCII digits. Uses the image rules in §3.6.
- plex.tv: **not called** by the app. Only the token from the registry is used.
- Session parsing (logic.py:746-785). For each `MediaContainer.Metadata[]`:
  - `title`: for `type=="episode"` with `grandparentTitle`: `"<show>[ S%02dE%02d] – <title>"`, using an en dash surrounded by spaces. The SxxEyy part is added only if both `parentIndex` and `index` are present (int-cast).
  - `key`: `sessionKey`, or `Session.id`, or `""`, as a string. Internal only.
  - `year`.
  - `player`: `Player.title`, or `device`, or `product`, or `""`.
  - `product`.
  - `state`: `Player.state`.
  - `state_ro`: `{"playing":"rulează","paused":"în pauză","buffering":"se încarcă"}`, else the raw state.
  - `progress`: `round(viewOffset/duration*100,1)`, or 0.0 if duration is 0 or missing.
  - `position` / `duration`: `int(ms/1000)`.
  - `decision`: no `TranscodeSession` gives `direct_play`. If video or audio decision is `transcode`, `transcode`. Else if either is `copy`, `direct_stream`. Else `direct_play` (a subtitle-only transcode session counts as direct play).
  - `decision_ro`: `redare directă` / `flux direct` / `transcodare`.
  - `transcode`: `decision=="transcode"`.
  - `address`: `Player.address` or "". Internal only.
  - `rating_key`: for episodes with `grandparentRatingKey`, that; else `ratingKey`, else "" (as a string). The field is null unless it is a valid poster id.
  - `poster`: `/img/plex/<rk>` or null.
  - `backdrop`: `/img/plex-art/<rk>` if there is art (episodes: `grandparentArt`; otherwise `art`), else null.
- `public_session` drops `address` and `key`.
- `with_backdrop_fallback`: if `backdrop` is falsy and the Radarr movie list is non-empty, take the first movie whose title equals the session title (casefold) and whose `year` is equal, and set `backdrop = /img/movie-art/<id>`.

### 5.2 Radarr (v3; header `X-Api-Key: <key>`, sent even when empty)
| call | URL | cache | use |
|---|---|---|---|
| queue | `/api/v3/queue?includeMovie=true&pageSize=200` → `.records` or [] | none | downloads, queue ids, title map |
| movies | `/api/v3/movie` (or [] if empty) | 30 s | library, waiting, title map, backdrop fallback, notifications |
| history | `/api/v3/history?eventType=1&pageSize=200` → `.records` (grabbed) | 60 s | title map |
| root folders | `/api/v3/rootfolder` | 300 s | disk-low drive |
| covers | `/api/v3/mediacover/<id>/<file>` | none | images |

The TTL cache stores only successful results (exceptions are not cached). There is no single-flight.

Mappings:
- **Queue record** (logic.py:649-667):
  - `percent = clamp(0..100, round((size-sizeleft)/size*100,1))`, or 0.0 if size ≤ 0.
  - `state = trackedDownloadState or status or ""`. If state == `downloading` and `status` ∈ {paused, queued, warning, failed}, then state = status.
  - `status` uses this map: downloading→`Se descarcă`, paused→`Pauză`, queued→`În coadă`, completed→`Terminat`, delay→`Amânat`, downloadClientUnavailable→`Client indisponibil`, warning→`Avertisment`, failed→`Eșuat`, importPending→`Așteaptă importul`, importing→`Se importă`, imported→`Importat`, failedPending→`Eșuat`. Anything else stays raw.
  - `title = movie.title or rec.title or ""`, `year = movie.year`.
  - `time_left = format_duration_ro(parse_timeleft(timeleft))`.
  - `movie_id = radarr_id = movieId`, `poster = /img/movie/<movieId>` or null.
- `parse_timeleft`: `"d.hh:mm:ss"` or `"hh:mm:ss"` to seconds. The seconds part is float-cast. A bad h/m/s gives None. A bad day part raises (uncaught).
- `format_duration_ro`: None→`""`; under 60 s→`sub 1 min`; days→`"%dz %dh"`; hours→`"%dh %dm"`; else `"%d min"`.
- **Library** (logic.py:683-700): movies with `hasFile`.
  - `size = movieFile.size or sizeOnDisk or 0`, `size_gb = round(size/1024³,1)`.
  - `ro_subs`: null if Bazarr is down, else `ro_map.get(id)`. A movie missing from Bazarr also gives null.
  - Sorted by lowercased title.
- **Waiting** (logic.py:579-598, 703-717): `monitored && !hasFile`.
  - Sort key: (timestamp of `digitalRelease`, or `physicalRelease` if digital is missing, or +inf; lowercased title).
  - `downloading = id ∈ queue movieIds`.
  - `status`:
    1. downloading: `Se descarcă acum`.
    2. Else if `!isAvailable`:
       - digital date in the future: `Așteaptă lansarea digitală (<d> <mon> <yyyy>)`
       - else physical date in the future: `Așteaptă lansarea pe disc (<d> <mon> <yyyy>)`
       - else neither date exists and `status` ∈ {announced, tba}: `Anunțat, fără dată de lansare`
    3. Otherwise: `Se caută în 4K pe FileList`.
  - Dates are ISO strings (`Z` becomes +00:00; naive is treated as UTC), compared with UTC now. Day without padding; months `ian feb mar apr mai iun iul aug sep oct nov dec`.
- **Torrent hash → title** (logic.py:188-209):
  - `by_id` maps movie id to movie.
  - For history records, then queue records: `dl = upper(strip(str(downloadId)))`, `movie = by_id[movieId] or rec.movie or {}`. If dl and `movie.title` exist and dl is not already present, map dl to the title. History wins.
  - Per torrent: `map[upper(hash)]` or `release_title(name)`.
- **release_title** (logic.py:134-185), exact algorithm:
  1. `s = name.strip()`. Empty gives `""`.
  2. Remove the extension regex `\.(mkv|mp4|avi|m4v|mov|wmv|ts|m2ts|iso)$` (case-insensitive).
  3. Remove leading bracket groups `^(\s*\[[^\]]*\]\s*[-_.]?\s*)+`.
  4. If there is no space in `s.strip()`, or `s.count('.') >= 3`, or `'_'` is in `s`: replace `[._]+` with a space.
  5. Replace `[` and `]` with spaces. Split on whitespace. No tokens gives the original name stripped.
  6. Year candidates: indices i > 0 where the token fully matches `^[(\[]?((?:19|20)\d\d)[)\]]?$`.
     - "Strong" candidates are those at the end of the list or followed by a release tag.
     - `year_idx` = last strong candidate, else last candidate, else none.
  7. `end` = year_idx, else the first i > 0 that is a release tag, else len.
  8. `title = " ".join(tokens[:end]).strip(" -–_.(")`. Empty gives the original name stripped.
  9. Return `"<title> (<yyyy>)"` or `title`.
  - Release tag: `tok.lower().strip("[]()")` is in the set `{2160p 1080p 1080i 720p 576p 480p 4k uhd fhd web webdl web-dl webrip web-rip hdtv bluray blu-ray bdrip brrip bdremux remux dvdrip hdrip hdr hdr10 hdr10+ hdr10plus dovi x264 x265 h264 h265 hevc avc av1 10bit 8bit repack proper rerip extended remastered unrated uncut imax multi subbed dubbed aac ac3 dts dts-hd truehd atmos ddp eac3 flac amzn dsnp hmax atvp}`, or matches (anchored at the start, case-insensitive) `^(ddp\d|dd\d|aac\d|ac3\d|dts-|x26[45]|h26[45]|\d{3,4}[pi]$)`.

### 5.3 Bazarr
- `GET {bazarr.url}/api/movies`, header `X-API-KEY: <key>`, timeout **4 s**. Result is `.data` or []. Cached 60 s.
- `bazarr_ro_map`: for each entry with a `radarrId`, the value is (some `subtitles[].code2=="ro"`) AND NOT (some `missing_subtitles[].code2=="ro"`).

### 5.4 Prowlarr
`GET {prowlarr.url}/ping`, no headers beyond the defaults, timeout 2.5 s. Up if there is no exception (HTTP ≥ 400 counts as down; redirects are followed).

### 5.5 qBittorrent (services.py:253-291)
- A cookie-jar opener (with the no-secret-redirect handler).
- `GET {url}/api/v2/torrents/info`. On HTTP **403** only: log in, then retry once.
- Login: `POST {url}/api/v2/auth/login`, form `username=<cfg or "admin">&password=<cfg or "x">`, headers `Referer: {url}` and `Origin: {url}` (url with trailing slash stripped). Serialized by a lock. The response is not checked.
- FileList filter per torrent:
  - If `tracker` (the current tracker URL) is non-empty: regex search on it. No caching.
  - Else, if the hash is in the cache: the cached bool.
  - Else `GET /api/v2/torrents/trackers?hash=<urlquoted>` and test whether any `url` matches. The result is cached per hash. An exception gives False (not cached).
- Tracker URLs are never stored, logged or returned.
- Torrent summary (logic.py:95-119). With `p = float(progress)`, `r = float(ratio)`, `s = int(seeding_time)`:
  - `hr`: not complete (p < 1) → `downloading`; `r >= 1.0 or s >= 48*3600` → `safe`; else `pending`.
  - `rule_progress = round(min(1, max(r/1.0, (s/3600)/48)), 3)`, with negatives clamped to 0.
  - `remaining` (only when pending): `mai are nevoie de <max(1, ceil(48 - s/3600))> h sau ratio 1`. The safe text `regula îndeplinită` exists but is never reached through the summary.
  - `progress = round(p*100,1)`, `ratio = round(r,2)`, `seed_hours = round(s/3600,1)`, `safe = hr=="safe"`.
  - `state_ro` map: downloading/forcedDL/metaDL/forcedMetaDL→`Se descarcă`; stalledDL→`Blocat (descărcare)`; queuedDL→`În coadă`; pausedDL→`Pauză`; stoppedDL→`Oprit`; checkingDL/checkingUP/checkingResumeData→`Se verifică`; uploading/forcedUP→`Seed activ`; stalledUP→`Seed (fără cereri)`; queuedUP→`Seed în coadă`; pausedUP/stoppedUP→`Seed oprit`; moving→`Se mută`; missingFiles→`Fișiere lipsă`; error→`Eroare`; anything else → the raw state, or `necunoscut` if empty.
  - `dlspeed`, `upspeed`, `eta`, `size` are ints.
- `hr_warnings` = the pending torrents as `{title, name, remaining}`.

### 5.6 ntfy: see §7.

---------------------------------------------------------------------------------------------------
## 6. System actions (plexcouch/system.py)

### 6.1 Monitors
- **Off** (system.py:72-79): `PostMessageW(HWND_BROADCAST=0xFFFF, WM_SYSCOMMAND=0x0112, SC_MONITORPOWER=0xF170, 2)`. PostMessage, never SendMessage (which can hang). Returns its BOOL. Log `monitors off (<reason>)`; the reason is `manual` or `auto: film pornit pe TV`.
- **On** (81-88): `SendInput` with 2 `INPUT_MOUSE` records, relative move `dx=+1` then `dx=-1`, `MOUSEEVENTF_MOVE=0x0001`. Then `SetThreadExecutionState(ES_DISPLAY_REQUIRED=0x2)`. ok = (SendInput returned 2). Log `monitors on`.
- In dry-run, both only log `dry-run: monitors off (<reason>)` / `dry-run: monitors on` and return true.
- They must run in the interactive user session (started from Task Scheduler at logon).

### 6.2 Shutdown/restart with our own 60 s countdown (system.py:27-176)
- Executable: `%SystemRoot%\System32\shutdown.exe` if it exists, else `shutdown`.
- Arguments: shutdown is `[exe, "/s", "/hybrid", "/t", "0"]` and restart is `[exe, "/r", "/t", "0"]`. Never `/f`; `/t` above 0 would imply it. An unknown kind raises.
- Abort: `[exe, "/a"]`.
- `power(kind)`:
  1. Under the lock: if `pending_power(state)` is not None, return None (one at a time).
  2. Otherwise `state = {"kind", "at": now+60}`, clear `fired`, start a daemon Timer(60 s, `_fire`).
  3. Log `[dry-run: ]power <kind> scheduled in 60s`.
  4. Return `{"kind", "at": int(at)}`.
- `pending_power(state, grace=10)`: None if there is no state or `at + 10 < now`. Otherwise `{"kind", "at": int(at)}`. `pending()` clears the state once expired. The countdown therefore stays "pending", and blocks new power requests, until 10 s after `at`.
- `_fire`:
  1. Under the lock: abort if the state was replaced or cancelled.
  2. Set `timer = None`, `fired = state`, and take the exec lock before releasing the main lock.
  3. Dry-run: log `dry-run: power <kind> (would run shutdown /s /hybrid /t 0)` and stop.
  4. Otherwise log `power <kind> now` and run the command with `CREATE_NO_WINDOW (0x08000000)`, capturing output, timeout 15 s.
  5. On a non-zero exit or an exception: WARNING, and clear `state`/`fired` if they are unchanged.
  6. Release the exec lock.
- `cancel_power()` outcomes `cancelled` / `aborted` / `too_late` / `nothing`:
  1. Under the lock: remember `fired` and whether a state existed, cancel the timer, clear the state and `fired`.
  2. If nothing had fired: return `cancelled` if a state existed, else `nothing`. Log `[dry-run: ]power cancelled`.
  3. If it had fired: wait on the exec lock (shutdown.exe may still be running), then run `shutdown /a` (dry-run: just log and return `aborted`). Exit 0 gives `aborted`; anything else gives `too_late` (WARNING `power <kind> could not be aborted (shutdown /a code <n>)`).
- `power_status()` (app.py:320-330): if there is no pending power and no after-movie state, null. Otherwise `dict(pending or {})` plus `after_movie: public_after_movie(state)`.

### 6.3 PC info (system.py:215-268)
- `uptime = int(GetTickCount64()/1000)`, or null on error.
- `disks` (for each configured letter):
  - Root is the letter plus `\` (unless it already ends with one).
  - Total and free come from GetDiskFreeSpaceExW (TotalNumberOfBytes, TotalNumberOfFreeBytes). An error skips the disk.
  - `{"drive": letter without a trailing \, "free_gb": round(free/1024³,1), "total_gb": round(total/1024³,1), "used_pct": round((total-free)/total*100,1) or 0}`.
- `hostname = gethostname()`, `platform = "win32"`.
- `free_gb(drive)` (for the notifications) is the same calculation but unrounded, None on error.

### 6.4 Shut down after the movie (logic.py:831-900; app.py:338-384)
- "TV sessions" = sessions with a key whose address passes `is_tv_player`:
  - `is_local_player(addr)`: lowercased and stripped, in {127.0.0.1, ::1, localhost, ::ffff:127.0.0.1}, or in own_ips, or a loopback IP.
  - `is_tv_player(addr)`: non-empty, not local, `is_private_client(addr)`, and not local after stripping `%zone`.
  - Remote, public viewers never count.
- `own_ips` (system.py:247-263): every IPv4 from `getaddrinfo(gethostname(), AF_INET)` plus the local address of a UDP "connect" to 10.255.255.255:1. No packet is sent. Cached 300 s.
- **Arm** (POST /api/power/after-movie):
  1. If a power countdown is pending: **409** `{"error":"Există deja o oprire programată."}`.
  2. Poll Plex sessions (demo uses fake ones). On an exception: DEBUG log, then **503** `{"error":"Plex nu răspunde. Repornește PC-ul sau verifică serviciul."}`.
  3. `_pick_session(tv_sessions)`: the first `playing` session, else the first one. None gives **409** `{"error":"Nu rulează nimic pe TV acum."}`.
  4. State is `{key, title, since: now, progress, seen: now}`, kept in memory only and replaced if already armed. Log `after-movie shutdown armed`.
  5. Response: 200 `{"ok":true,"power":<power_status>}`.
- **Step**, called with every *successful* Plex poll of the auto-monitor loop:
  - If there is no state: no-op.
  - If the state has a key and that key is still among the TV sessions: update progress, title and `seen=now`. No action.
  - If the key is gone and the last progress was ≥ **90.0**: clear the state and **fire**.
  - If the key is gone below 90 (stopped early): set `key=None`, `seen=now`, and stay armed.
  - Without a key: pick the next TV session, preferring the same title if it appears (then the first playing, else the first). If one is found, follow it (key, title, progress, seen=now).
  - If nothing is found and `now - seen (or since) >= 3*3600`: clear and **expire** (log `after-movie shutdown expired (nothing played for 3 h)`).
- **Fire**: log `after-movie: the movie ended, shutting down`, then `actions.power("shutdown")`. Only if that returns non-None, send the notification asynchronously (§7.2 #3).
- **Cancel**: clear the state. `power/cancel` does *not* disarm after-movie.
- `public_after_movie` = `{"armed":true,"title":str,"since":int(since)}`.

### 6.5 Auto monitors off (logic.py:806-828; app.py:472-496)
- `handled` is the set of session keys already decided. A session is decided the first time it is seen in state `playing`. It triggers only if `enabled` and the player is a TV player. Keys of sessions that ended are removed from handled. Sessions without a key are ignored.
- Loop:
  1. `primed=false`. Poll immediately, then every interval.
  2. On a poll exception: DEBUG log. If not yet primed, set primed (gotcha §11), then continue.
  3. On success: `enabled = primed and settings.auto_monitors_off`, read live each tick.
  4. `(handled, trigger) = decision(handled, sessions, own_ips, enabled)`, then `primed = true`.
  5. `after_movie_poll(sessions)`; an exception logs a traceback.
  6. If trigger: `monitors_off(reason="auto: film pornit pe TV")`.
- So the first successful poll only records what is already playing. Toggling the setting on later never fires for a session that was already seen. Each session can turn the monitors off at most once.

---------------------------------------------------------------------------------------------------
## 7. Notifications (ntfy; plexcouch/notify.py, app.py:386-456)

### 7.1 Transport
- Enabled iff `notifications.topic` is a non-blank string. `server = (notifications.server or "https://ntfy.sh")` with trailing `/` removed.
- `send()`:
  - No topic: return false.
  - Dry-run and the server host is not loopback (`localhost` or a loopback IP): log `dry-run: notify <title>` and return true.
  - Otherwise `POST <server>/` with `Content-Type: application/json; charset=utf-8` and a raw UTF-8 JSON body.
- Body fields, in order: `topic`, `title`, `message`, then optional `tags` (list), `priority` and `click`. `priority` accepts the strings min=1, low=2, default=3, high=4, max=5, urgent=5, or an int 1-5; anything else is dropped.
- Timeout 10 s by default.
- Result: true on 2xx. An HTTP error logs WARNING `notify '<title>' failed: HTTP <code>` (Python repr quoting) and returns false. Any other exception logs WARNING `notify '<title>' failed: <ExcType>` and returns false. Never raises.
- Success logs `notify sent: <title>`.
- Redirects are followed (plain urlopen).
- Topic generation: `plexcouch-` plus `token_urlsafe(18)`, i.e. 24 chars of `[A-Za-z0-9_-]`. Subscribe URL: `<server>/<topic>`.

### 7.2 Types (verbatim)
| # | title | message | tags | priority | trigger |
|---|---|---|---|---|---|
| 1 | `Film gata` | `<label> e gata de vizionat, cu subtitrare RO.` / `<label> e gata de vizionat. Subtitrarea RO încă lipsește.` / `<label> e gata de vizionat.` | `["movie_camera"]` | none | movie_ready_step |
| 2 | `Spațiu puțin` | `Mai ai <int(free)> GB liberi pe <drive>. Un film 4K are 20–30 GB.` (en dash; int truncation) | `["warning"]` | `high` (4) | disk_low_step |
| 3 | `Oprire după film` | `PC-ul se oprește în 60 de secunde. Anulează din Plex Couch.` (literal "Plex Couch") | none | `high` | after-movie fire, async |
| 4 | `<app_name>` | `Test Plex Couch: notificările merg.` | none | none | POST /api/notify/test, timeout 6 s |

`label` = `"<title> (<year>)"` if year is truthy, else the title.

### 7.3 Test endpoint (app.py:387-397)
- Not enabled: **409** `{"error":"Notificările nu sunt pornite. Rulează app.py --setup-ntfy."}`.
- Demo (enabled): 200 `{"ok":true}` without sending.
- Send failed: **502** `{"error":"Notificarea nu a plecat. Verifică internetul PC-ului și reîncearcă."}`.
- Otherwise 200 `{"ok":true}`.

### 7.4 Notify tick (every 60 s while enabled; app.py:410-456)
1. `movies` = Radarr movies, or None on an exception. `ro_map` = `bazarr_ro_map(bazarr)`, or None on an exception. `state = state_file.load()`.
2. `wait_s = float(ready_wait_minutes or 0)*60`.
3. **movie_ready_step(state.movie_ready, movies, ro_map, now, wait_s)** (logic.py:925-962):
   - `done` = set of str ids; `pending` = {str id: float}.
   - If movies is None: return the state unchanged (primed kept, done sorted) and no messages.
   - `with_file` = str ids of movies with `hasFile`.
   - If not primed: return `{"primed":true,"done":sorted(with_file),"pending":{}}` and no messages (baseline).
   - `new = with_file - done - pending keys`. If `len(new) > 10` (MOVIE_READY_FLOOD), add them all to done silently.
   - For each id in `with_file - done`, sorted numerically:
     - `first = pending.setdefault(id, now)`.
     - `ro` = None if Bazarr is down, else `bool(ro_map.get(int id))` (a movie missing from Bazarr counts as False).
     - If ro is true: message with the RO-subtitle text, now.
     - Else if `now - first >= wait_s`: message with `ro` (False gives the "lipsește" text, None the generic text).
     - Else skip it for now.
     - Each notified id goes to done and leaves pending.
   - Finally, prune pending to ids that still have a file and are not done. `done` is never pruned, so a re-download or upgrade never repeats.
4. **Disk**: `drive = _radarr_drive()`. That is the first Radarr root folder whose `drive_of(path)` is non-null; otherwise the last known one, initially `D:`. Errors are logged at DEBUG.
   - `drive_of`: `D:\x` gives `D:` (uppercased); a UNC path gives `\\server\share`; otherwise None.
   - `free = free_gb(drive)`. `lows = state.disk_low if it is a dict else {}`.
   - `disk_low_step(low, free, min_gb = float(disk_min_gb or 0))`:
     - free is None: `(low, false)`.
     - not low and free < min: `(true, true)`, i.e. warn.
     - low and free > min + 10 (DISK_REARM_GB): `(false, false)`.
     - else `(low, false)`.
5. `state_file.update({"movie_ready", "disk_low"})` **before** sending, so a crash never causes a resend.
6. Send each message synchronously.

Exceptions in the tick log a traceback `notify check failed`.

---------------------------------------------------------------------------------------------------
## 8. Demo mode (plexcouch/demo.py; app.py)
- Requires `--host 127.0.0.1`. Forces dry-run and allows HTTP (TLS is still used if certs exist). No background threads.
- Auth: every session is valid. Login returns 200 `{"ok":true}` with no cookie. logout-all is a no-op. The settings POST updates memory only.
- `/api/status` = `demo_status(app_name, power_status(), settings)` plus `notifications` (demo.py:101-152). The data is fixed:
  - `demo: true`, `dry_run: true`.
  - pc: hostname `DEMO-PC`, uptime 3 d 4 h, disks C: 181.4/930.5/80.5 and D: 807.2/931.5/13.3.
  - All 5 services up.
  - now_playing: "Dune: Part Two" (2024) on "Samsung TV, sufragerie" / "Plex for Samsung", playing. `position = 2843 + seconds since module load`, duration 9960, direct play, rating_key "101", poster `/img/plex/101`, backdrop `/img/plex-art/101`, `on_tv` true.
  - downloads: Anatomy of a Fall at 62 %, "8 min", "Se descarcă".
  - waiting: Anatomy of a Fall ("Se descarcă acum"), The Odyssey ("Așteaptă lansarea digitală (17 nov 2026)"), Perfect Days ("Se caută în 4K pe FileList").
  - library: Dune 61.8 GB RO, Past Lives 19.1 GB without RO, The Godfather 19.0 GB RO.
  - Three torrents built through the real summarize, title-map and sort functions.
- After-movie arm uses the fake session `{"key":"demo","title":"Dune: Part Two","year":2024,"state":"playing","progress":…,"address":"10.0.0.20"}`. It never fires because there is no poll thread.
- Images: `/img/plex*/<id>` maps id 101 to movie 2; `/img/movie*/<id>` is the movie id (1-6). Posters are SVGs (`image/svg+xml`) generated from per-movie colours:
  - Poster: 100×150 viewBox at 240×360, a vertical gradient, a circle (opacity .14) and up to 4 uppercased title lines wrapped at 10 chars. Font size `max(8, min(17, (88/longest - 1)/0.72))`; `y0 = 110 - (n-1)*fs*0.55`; line spacing `fs*1.1`. Georgia (serif) or Arial Black.
  - Backdrop: 160×90 viewBox at 720×405, a diagonal gradient, a circle (.22) and an ellipse (.18).
  - The exact templates are at demo.py:44-72. MOVIES table: demo.py:13-20.

---------------------------------------------------------------------------------------------------
## 9. Tooling

### 9.1 tools/make_certs.py
- Usage: `--ip` (repeatable), `--dns` (repeatable), `--out` (default `<repo>/certs`), `--force-ca`, `--openssl`.
- **Validation** (97-118). Errors are printed as `error: <msg>` to stderr, exit 2:
  - IPs must be IPv4 inside 192.168/16, 10/8, 172.16/12 or 127/8.
  - DNS names: lowercase, strip the trailing dot, match `^(?=.{1,253}$)[A-Za-z0-9](?:[A-Za-z0-9-]{0,62})(?:\.[A-Za-z0-9-]{1,63})*$`, and be `localhost` or end in `.local`.
  - At least one name is required.
- **openssl lookup**, in order: `--openssl`, `$OPENSSL`, PATH, then `Git\usr\bin\openssl.exe` and `Git\mingw64\bin\openssl.exe` under ProgramFiles, ProgramW6432, ProgramFiles(x86) and LOCALAPPDATA\Programs. Not found: exit 2.
- **CA** (only if `ca.key` or `ca.pem` is missing, or with `--force-ca`):
  - Key: RSA 3072 (`genpkey`) to `ca.key`, ACL-restricted immediately.
  - Cert: self-signed `req -x509 -sha256 -days 3650 -set_serial 0x<32 random hex>`.
  - Subject `CN = Plex Couch Local CA`.
  - Extensions:
    - `basicConstraints = critical,CA:TRUE,pathlen:0`
    - `keyUsage = critical,keyCertSign,cRLSign`
    - `extendedKeyUsage = serverAuth`
    - `subjectKeyIdentifier = hash`
    - `nameConstraints = critical,permitted;IP:192.168.0.0/255.255.0.0,permitted;IP:10.0.0.0/255.0.0.0,permitted;IP:172.16.0.0/255.240.0.0,permitted;IP:127.0.0.0/255.0.0.0,permitted;DNS:.local,permitted;DNS:localhost,permitted;email:.invalid` (no `excluded` entries).
  - Otherwise prints `Keeping the existing CA in <out> (use --force-ca to replace it).`; when created, `Created CA "Plex Couch Local CA" (3650 days).`
- **ca.crt**: DER export of `ca.pem`, regenerated on every run.
- **Server**:
  - Key: RSA 2048 to `server.key`, restricted.
  - CSR in a temp dir (`plexcouch-certs-*`), signed by the CA: `-sha256 -days 397 -set_serial 0x<random>`.
  - Subject `CN = Plex Couch`, deliberately not a host name.
  - Extensions (`v3_srv`):
    - `basicConstraints = critical,CA:FALSE`
    - `keyUsage = critical,digitalSignature,keyEncipherment`
    - `extendedKeyUsage = serverAuth`
    - `subjectKeyIdentifier = hash`
    - `authorityKeyIdentifier = keyid,issuer`
    - `subjectAltName = IP:..,...,DNS:..` (IPs first, then DNS, as given)
- **Files** in out: `ca.key`, `ca.pem`, `ca.crt`, `server.key`, `server.pem`. The config files are written as ASCII with LF line endings.
- **ACL lockdown** (153-184):
  - Windows: `icacls <path> /inheritance:r /grant:r <USERNAME>:F SYSTEM:F`, where icacls is `%SystemRoot%\System32\icacls.exe` or `icacls`. USERNAME falls back to getpass. On failure, stderr gets `warning: could not restrict access to <path>; do it by hand with icacls.`
  - Non-Windows: chmod 600.
- An openssl error gives `error: openssl <subcmd> failed:\n<stderr>` and exit 1.
- Success output:
  - `Created server certificate for <names> (397 days).`
  - `Files in <out>: ca.crt (install on the phone), ca.pem, server.pem, server.key, ca.key.`
  - `Keep ca.key and server.key private; never commit the certs folder.`
  - Exit 0.

### 9.2 tools/build_tokens.py (dev tool; could stay Python or become a build.rs step)
- Reads `docs/design-system/tokens.json` (top-level keys: name, version, color, type, spacing, radius, shadow, size) and writes `static/tokens.css` with LF line endings.
- Output:
  1. Header comment.
  2. `:root {` containing, in order:
     - `--<color.name>: <value.dark or value>;`
     - `--font-<family>: <stack>;`
     - the spacing/radius/shadow/size tokens as `--<name>: <value>;`
  3. `}`.
  4. For each type group style: an empty line, then `.<name> {` with font-family `var(--font-<group.family or sans>)`, font-size, line-height, font-weight, optional letter-spacing, and `font-variant-numeric: tabular-nums` if `usage` contains "tabular"; then `}`.
- `--check` compares instead of writing. It prints `static/tokens.css is out of date; run tools/build_tokens.py` and exits 1, or prints `... is up to date` and exits 0.

---------------------------------------------------------------------------------------------------
## 10. Test inventory (170 cases, which become the Rust acceptance tests)

### 10.1 H&R / torrents / titles (tests/test_logic.py)
- TestHitAndRun
  - `test_safe_by_ratio`: (1.0,0) and (2.5,10) are safe.
  - `test_safe_by_seed_time`: (0,48h) safe; (0.99,48h-1) not.
  - `test_status`: (0.5,..)=downloading, (1,0.2,13h)=pending, (1,1.0,0)=safe.
  - `test_rule_progress`: (0.5,0)=.5, (0.1,24h)=.5, (0.75,12h)=.75, (3,0)=1, (0,100h)=1, (None,None)=0.
  - `test_remaining_text`: 13h and 13h+1s give "mai are nevoie de 35 h sau ratio 1"; 48h-10s gives "1 h".
  - `test_summarize_and_warnings`: 3 torrents give pending/safe/downloading; pending remaining is "48 h"; warnings equal `[{"title":"A","name":"A","remaining":...}]`; no `tracker`/`hash` keys.
  - `test_title_given_or_parsed`: the parsed title is "The Godfather (1972)"; a given title overrides it and flows into the warnings.
- TestReleaseTitle
  - `test_shapes`, 12 cases:
    - Godfather mkv → "The Godfather (1972)"
    - Godfather WEB-DL.AAC5.1 → "The Godfather (1972)"
    - Anatomy REPACK → "Anatomy of a Fall (2023)"
    - "Dune Part Two 2024 2160p UHD BluRay x265-GROUP" → "Dune Part Two (2024)"
    - "[YTS.MX] Past Lives (2023) [2160p] [WEB-DL]" → "Past Lives (2023)"
    - "Perfect Days (2023) [1080p] [BluRay] [5.1] [YTS.MX]" → "Perfect Days (2023)"
    - Blade Runner 2049 2017 → "Blade Runner 2049 (2017)"
    - 2001 A Space Odyssey 1968 → "2001 A Space Odyssey (1968)"
    - A.Complete.Unknown.2024 mp4 → "A Complete Unknown (2024)"
    - Mad_Max_Fury_Road_2015 → "Mad Max Fury Road (2015)"
    - "Some.Movie.2160p.WEB-DL.DDP5.1-GROUP" → "Some Movie"
    - "The Odyssey" → "The Odyssey"
  - `test_empty`: "" and None give "".
- TestTorrentTitles
  - `test_history_then_queue_case_insensitive`: the map holds upper-case hashes; lookup by a lower-case torrent hash works.
  - `test_history_wins_and_unknown_movie_skipped`.
  - `test_queue_movie_object_and_fallback`: the queue's `movie.title` is used when the movie list is None; otherwise falls back to release_title.

### 10.2 Image routes
- test_logic.TestImgRoutes
  - `test_parse_img_path`: the valid kinds parse; invalid paths include `/img/plex-art/x`, `/img/movie-art/`, `/img/art/1`, `/img/plex-art/1/2`, `abc`, `1a`, `-1`, a trailing slash, `../x`, `%31`, a trailing space, the `other` kind, full-width digits, Arabic digits, 11 digits, `?x`, `.jpg`, `\n`, "" and None.
  - `test_poster_url`: (movie,2) gives /img/movie/2; None, "", "x1", Arabic digits, "1/2" and kind "tv" give None.
- test_web.TestImageRoutes
  - `test_requires_login`: 401 without a cookie and with the cookie `123.bad`, and no upstream call.
  - `test_digits_only_and_cache_headers`: 200 with `image/jpeg`, body passthrough, `Cache-Control: private, max-age=86400`, nosniff, for all 4 kinds. Bad ids give 404 with no upstream call. An unknown id gives 404 after one call.

### 10.3 Trackers / IPs / Host
- TestTrackerMatch
  - `test_filelist_hosts`: reactor.thefl.org, filelist.io, tracker.filelist.ro, flro.org and upper-case variants match.
  - `test_other_hosts`: "", None, opentrackr, `example.org/thefl` (path only) and filelist.com don't match.
  - `test_custom_regex`.
- TestPrivateIp: `test_allowed`, `test_rejected` (§4.6).
- test_security.TestHostAllowed
  - `test_allowed`: 127.0.0.1[:8800], 192.168.0.5:8766, 10.1.2.3, [::1][:8766], [fe80::1%25eth0]:1, localhost[:8765], LOCALHOST, `localhost.`, plexpc.local, PlexPC.local:8766, a.b.local, `plexpc.local.`.
  - `test_rejected`: None, "", " ", rebind.evil[:8766], evil.com, local, .local, plexpc.local.evil.com, localhost.evil.com, 127.0.0.1.nip.io, 2130706433, "a b", 300×"x", [nonsense], plexpc.lan.
  - `test_configured_names`: case and trailing dot are normalized; non-string entries are ignored.
- TestHostOverHttp
  - `test_rebind_host_refused_everywhere`: GET /, GET /api/status, POST /api/login and GET /app.js all give 421 "Gazdă nepermisă." text/plain.
  - `test_missing_host_refused`.
  - `test_allowed_hosts`: an IP with port, localhost and plexpc.local give 200; plexpc.lan gives 421 until it is configured.
  - `test_no_hsts_on_plain_http`.
- TestInstallHandlerHost
  - `test_install_listener_checks_host_and_has_no_hsts`: 421 for a rebind Host; `/` gives 200; `/api/status` gives 301; no HSTS.
- TestHstsOnSecureHandler
  - `test_hsts_on_every_response`: `/`, `/api/status`, `/nope` and the 421 all carry `max-age=31536000`.

### 10.4 PIN / sessions / login guard
- TestSessionToken
  - `test_roundtrip`.
  - `test_expiry`: SESSION_DAYS is 30; valid at 29 d; `session_expiry == now+30d`; invalid at exactly 30 d and at 180 d.
  - `test_tamper`: the epoch is "1"; a modified expiry, the wrong epoch, a zeroed signature and a wrong key all fail; malformed tokens fail ("", None, "abc", "1.2.3", ".x", "x.", "12a.ff", "1.é", "1.2", 2-part, 4-part, a non-digit epoch, a non-ASCII signature).
  - `test_key_bound_to_pin`.
- TestPin
  - `test_hash_verify`: prefix `pbkdf2_sha256$1000$`.
  - `test_default_iterations_and_salt`: `$200000$`, random salt, 32 hex chars.
  - `test_garbage_hash`.
  - `test_format`: 1234 and 741852 are OK; 123, 12a4, "", 1234567 and Arabic digits are not.
- TestLoginGuard (test_logic)
  - `test_per_ip_lockout`: 4 failures give 0; attempts_left is 1; the 5th gives 600; remaining is 600, then 300 after 300 s, 0 for another IP, 0 at +601.
  - `test_success_resets`: attempts_left 4 after a new failure.
  - `test_global_slow_down_for_unknown_ips`: 19 IPs, then the 20th failure; remaining is 59 at +1 s, 0 at +60; then one attempt resets the 60 s window.
  - `test_global_window_expires`: after 3700 s.
- TestGuardReservation (test_security)
  - `test_parallel_reservations_never_exceed_the_per_ip_limit`: 60 threads give exactly 5 tickets; failing them yields [0,0,0,0,>590]; another IP is unaffected.
  - `test_success_refunds_and_resets`: attempts_left 3, then 5; is_known.
  - `test_correct_pin_while_others_in_flight_unlocks`.
  - `test_refund`.
  - `test_per_ip_lockout_window`: `reserve` at t=200 gives `(None,504)`; at t=704, wait 0.
- TestGuardKnownIps
  - `test_known_ip_is_not_slowed_down`.
  - `test_known_ip_keeps_its_per_ip_lockout`.
  - `test_known_ips_expire_after_30_days_and_round_trip`: load drops entries ≥ 31 d old and bad entries; known is false at exactly 30 d; `load_known(None)` and `(["x"])` don't crash.
- TestLoginBurst (real server; verify_pin instrumented)
  - `test_parallel_burst_checks_at_most_five_pins_and_hides_the_right_one`: 40 parallel logins (the correct PIN last). At most 5 checks; at most 1 concurrent; no 200; the correct PIN is never checked; every response is 401 or 429. After the remaining attempts are used, even the correct PIN gets 429 with `lockout_seconds` and no extra check.
  - `test_login_slots_refuse_beyond_the_cap`: with all 4 slots held, 429 with no check and the attempt refunded (attempts_left 5); then a 200.
  - `test_success_remembers_the_ip_in_the_state_file`: known_ips contains 127.0.0.1; it survives another key's update; it reloads on restart.
- TestSessions
  - `test_logout_all_invalidates_every_token`: Max-Age is 2592000 on login; logout-all without CSRF gives 403 and without auth 401; with both, 200 and `Max-Age=0`; both old tokens then get 401; the epoch is 2 in memory and in the file; the new token starts `2.`.
  - `test_logout_keeps_other_devices`.
  - `test_sliding_renewal`: a fresh token gets no Set-Cookie; a token with 14 d left gets an HttpOnly Set-Cookie with expiry > now+29 d; a POST to `/api/monitors/on` renews; `/img` doesn't.
  - `test_expired_after_30_days`.
  - `test_ui_has_the_logout_all_action`.
  - `test_old_two_part_tokens_are_rejected`.
- test_web.TestCookie
  - `test_secure_flag_only_over_https`: the exact string `pc_session=tok; Max-Age=60; HttpOnly; SameSite=Strict; Path=/; Secure`.
  - `test_handlers`.

### 10.5 Radarr / Bazarr / Plex text
- TestWaitingStatus (now = 2026-10-10Z)
  - `test_waiting_digital`: "Așteaptă lansarea digitală (17 nov 2026)".
  - `test_waiting_physical_only`: "Așteaptă lansarea pe disc (2 mar 2027)".
  - `test_searching`.
  - `test_announced_and_downloading`.
  - `test_summaries`: waiting holds only monitored movies without a file; the library entry equals `{"title":"The Godfather","year":1972,"size_gb":19.0,"ro_subs":true,"radarr_id":2,"poster":"/img/movie/2"}`; `ro_subs` is None when Bazarr is down; missing-RO gives False.
  - `test_timeleft`: "01:02:03" is 3723; "1.00:00:10" is 86410; "1h 2m"; "1z 2h".
  - `test_queue_record_poster`: (62.0,"8 min",3,"/img/movie/3").
- TestPlexParse
  - `test_parse`: progress 25.0; a subtitle-only TS gives direct_play; "în pauză"; "redare directă"; the public session has no address; rk/poster/backdrop handling; video transcode gives transcode true.
  - `test_backdrop_fallback_to_radarr`.
  - `test_episode_uses_show_poster`: "Show S01E02 – Pilot", poster /img/plex/40, grandparentArt gives the backdrop.
  - `test_empty`.
- TestBazarrKey `test_parse`: only the `auth:` block's apikey is used, quotes are stripped; a missing key gives "".

### 10.6 Auto monitors / TV sessions / after-movie
- TestAutoMonitor
  - `test_new_remote_playing_triggers_once` (also after pause→play).
  - `test_buffering_then_playing`.
  - `test_local_player_skipped`: 127.0.0.1, ::1 and own IP are handled but don't trigger.
  - `test_disabled`: enabling later doesn't fire.
  - `test_ended_sessions_forgotten`.
- TestTvSessions
  - `test_is_tv_player`: true for 192.168.0.40, 10.0.0.129, 172.16.5.5, fd00::5, ::ffff:192.168.0.40; false for own IP, loopbacks, localhost, "", None, public v4/v6, ::ffff:8.8.8.8, garbage.
  - `test_remote_viewers_are_not_the_tv`.
  - `test_on_tv_flag_in_status`: `on_tv` false for remote, true for TV; no address key.
- TestAfterMovie
  - `test_nothing_playing_on_the_tv`.
  - `test_arm_follows_the_tv_session`: key "5", title, since 100, progress 12.5; the public form is `{"armed":true,"title":..,"since":100}`.
  - `test_ended_after_90_percent_fires_once`.
  - `test_exactly_90_fires`.
  - `test_stopped_early_does_not_fire_and_stays_armed`: key None; a new session key "8" is followed and fires after 97 %.
  - `test_same_title_preferred_when_it_comes_back`.
  - `test_local_sessions_ignored`.
  - `test_expires_after_3_hours_with_nothing_on_the_tv`: none at 3h-1s, expire at 3h.
  - `test_long_movie_does_not_expire_while_it_plays`.
- TestAppNotify (real App with stubs)
  - `test_after_movie_poll_fires_power_once_and_notifies`: power called once with "shutdown"; the ntfy message matches §7.2 #3; after_movie is then None.
  - `test_after_movie_stopped_early_no_power`.
  - `test_arm_errors`: 409 "Nu rulează nimic pe TV acum.", 503 when Plex raises, 409 when power is pending.
  - `test_power_status_shape`: None, or `{"kind":"shutdown","at":5,"after_movie":null}`.
- TestRoutes
  - `test_after_movie_nothing_playing_409`.
  - `test_after_movie_arm_and_cancel`: 200 with power.after_movie.title and no `kind`; cancel gives `{"ok":true,"power":null}`.

### 10.7 Power
- TestPower `test_pending`: at 1060.4 with now 1000 gives at 1060; None at now=1100.
- TestPowerArgs
  - `test_shutdown_is_hybrid_and_immediate`.
  - `test_restart_has_no_hybrid`.
  - `test_never_forced`.
  - `test_unknown_kind_raises`.
- TestPowerTimer: `test_cancel_stops_it` (also checks one at a time), `test_dry_run_never_executes`.
- TestCancelAfterFire
  - `test_cancel_before_fire`: cancelled, then nothing.
  - `test_cancel_after_fire_runs_abort`: calls are exactly `[exe,/s,/hybrid,/t,0]` then `[exe,/a]`; pending is None afterwards.
  - `test_cancel_too_late`: `/a` returns 1116, giving too_late.
  - `test_cancel_while_firing_waits_then_aborts`.
  - `test_dry_run_after_fire_only_logs`: aborted, with no calls.
  - `test_abort_args`.
- TestCancelRoute `test_too_late_is_409`: "Prea târziu…"; aborted gives 200 with outcome "aborted".

### 10.8 Notifications
- TestNotifier
  - `test_json_payload_keeps_diacritics`: the URL is `https://ntfy.example/`, timeout 10, raw UTF-8 bytes, priority 4.
  - `test_payload_optional_fields`: priority 9 is dropped; click is kept.
  - `test_topic_never_logged`.
  - `test_disabled_without_topic`: None, "" and whitespace count as disabled; the default server is https://ntfy.sh.
  - `test_dry_run_never_sends_to_a_real_server`: ntfy.sh and a LAN IP get no post.
  - `test_dry_run_sends_to_a_local_test_server`: 127.0.0.1, localhost and [::1] get a post.
  - `test_real_post_to_fake_server`: path `/`, JSON content type.
  - `test_http_error_returns_false`: 429.
  - `test_new_topic`: regex `^plexcouch-[A-Za-z0-9_-]{24}$`.
- TestSetupNtfy `test_creates_keeps_and_replaces`: prints the `https://ntfy.sh/<topic>` URL; the defaults stay merged (disk_min_gb 60).
- TestMovieReady (wait 20 min)
  - `test_first_run_is_a_baseline`: state `{"primed":true,"done":["1"],"pending":{}}`.
  - `test_radarr_down_changes_nothing`.
  - `test_ro_present_notifies_at_once`.
  - `test_ro_missing_waits_then_notifies`: pending `{"2":1000.0}`.
  - `test_ro_arrives_during_the_wait`.
  - `test_bazarr_down_waits_full_time_generic_text`.
  - `test_no_year`.
  - `test_flood_is_recorded_not_sent`: 20 new movies give no messages.
  - `test_no_duplicates_across_a_reload_of_the_state_file`.
  - `test_broken_state_file_reads_empty`.
- TestDiskLow
  - `test_hysteresis`: frees 70,55,50,65,70,71,59,None,58 with min 60 give warnings F,T,F,F,F,F,T,F,F.
  - `test_text`: 55.9 gives "Mai ai 55 GB liberi pe D:. Un film 4K are 20–30 GB.".
  - `test_drive_of`.
- TestAppNotify
  - `test_state_file_next_to_config`.
  - `test_tick_baseline_then_ready_then_no_repeat_after_restart`: tags `["movie_camera"]`.
  - `test_tick_disk_low_once_on_radarr_drive`: drive "D:" from the root folders; one message `("Spațiu puțin", text, ["warning"], "high")`; ntfy priority 4.
- TestRoutes
  - `test_auth_and_csrf_required`: the 3 new endpoints give 401 without a cookie and 403 without CSRF.
  - `test_status_has_topic_when_logged_in`: the topic is absent from the 401 body.
  - `test_notify_test_reaches_the_server`: exact JSON `{"topic","title":"Plex Couch","message":"Test Plex Couch: notificările merg."}`.
  - `test_notify_test_without_topic`: 409 mentioning `--setup-ntfy`.
- TestStaticUi `test_ui_texts`: the listed RO strings are in index.html and app.js; no topic-like string is hard-coded.

### 10.9 HTTPS / startup / static
- test_web.TestHttpsUrl
  - `test_swaps_port_keeps_host`: also checks lowercasing and that port 443 is omitted.
  - `test_ipv6_host`.
  - `test_bad_host_uses_fallback`: fallback `::1` gives `[::1]`.
  - `test_path_sanitised`: `//evil.com/x` becomes `/evil.com/x`; CRLF becomes `/`; "foo" becomes `/`.
- TestStatic
  - `test_whitelist_has_pwa_files`.
  - `test_whitelisted_files_exist_and_nothing_else_is_served`: nested names only as `fonts/*.woff2`; no install.html or OFL.txt.
  - `test_whitelist_has_design_files`: fonts start with `wOF2`.
  - `test_style_uses_only_token_colors`.
  - `test_manifest_icons`.
  - `test_sw_precaches_whitelisted_files_only`.
  - `test_install_csp_hashes_inline_style`.
- TestHttpsSettings `test_precedence_cli_config_default`: config port 9443, a relative key resolved against the config folder, the default cert/ca paths; CLI values override.
- TestCertDer `test_pem_and_der`.
- TestNoCertHandler `test_only_a_503_page`: GET /, /api/status, POST /api/login, /app.js, /ca.crt and HEAD / all give 503 text/html containing "make_certs.py" and "Certificatele HTTPS lipsesc"; a rebind Host gives 421.
- TestLoadTls `test_missing_and_unreadable`.
- TestStartupWithoutCertificates (child process)
  - `test_missing_certificates_serve_only_503`.
  - `test_allow_http_serves_the_app`: `/` gives 200; /api/status gives 401.
- TestConnectionCap
  - `test_extra_connections_are_closed`: with a cap of 2, a third connection is closed without data; it recovers afterwards.
  - `test_limits`: 64 and 15.
- TestNoRedirectWithKeys
  - `test_redirect_refused_with_a_key`: X-Api-Key, X-Plex-Token and X-API-KEY all give RedirectRefused or an image of None, and the target is never hit.
  - `test_redirect_followed_without_a_key`.
  - `test_qbittorrent_opener_has_the_handler`.
- TestMakeCerts
  - `test_ca_config_is_name_constrained`.
  - `test_server_config`: exact SAN line `IP:192.168.1.50,IP:127.0.0.1,DNS:localhost`; 397/3650.
  - `test_validate_names`: rejects 8.8.8.8, 172.32.0.1, fe80::1, example.com and the empty list.
- TestMakeCertsHardening
  - `test_ca_constraints_and_eku`.
  - `test_restrict_uses_icacls_on_windows`: exact argv tail `[path,"/inheritance:r","/grant:r","alice:F","SYSTEM:F"]`; an icacls exit of 5 returns False.
- TestTokens: `test_tokens_css_matches_tokens_json`, `test_every_token_is_a_variable`.

---------------------------------------------------------------------------------------------------
## 11. Gotchas / surprising behaviour (decide: preserve or fix deliberately)
1. When Plex is down, `now_playing` is `[]`, not null (app.py:304). The UI uses `services.plex` to tell.
2. Per-IP failed PINs never decay with time; they reset only on success or lockout. In-flight attempts count toward the global window; failed ones stay in it.
3. A key or token read failure caches `""` for 300 s. Radarr still sends `X-Api-Key:` empty.
4. auto-monitor: if the *first* Plex poll fails, `primed` is set anyway, so the next successful poll can turn the monitors off for something already playing.
5. A POST body over 4096 gives 400 but the body is left unread on a keep-alive connection, so its bytes are parsed as the next request. Rust should close the connection or drain the body. Chunked bodies are ignored.
6. Methods other than GET/HEAD/POST get the stdlib 501 with no security headers and no IP/Host check.
7. Python `round()` rounds half to even on the binary value; match it for `size_gb`, `free_gb`, `progress` and so on. Python floats such as `19.0` serialize as `19.0`.
8. Python `is_private` is far wider than Rust std's (see §4.6). It also unwraps IPv4-mapped addresses before the LAN check, but `is_local_player` does not, so `::ffff:<own ip>` would count as a TV.
9. The config is read once. A topic added by `--setup-ntfy` needs a restart (it says so). The app writes `secret` into the config on the first start.
10. The after-movie notification text hard-codes "Plex Couch", regardless of `app_name`. The test notification uses `app_name` as its title.
11. A power countdown stays "pending" for 10 s after `at` (grace), blocking new requests and arming. `power/cancel` doesn't disarm after-movie. If after-movie fires while a power countdown is pending, `power()` returns None, so no notification is sent and the after-movie state is still cleared.
12. The disk-low drive defaults to `D:` until Radarr root folders are read; the UI also prefers `D:`.
13. qBittorrent: if a torrent's current `tracker` is non-FileList, the torrent is excluded even when FileList is in its tracker list. Login failures aren't detected (qBittorrent returns 200 "Fails.").
14. The status call waits up to 6 s overall; futures that time out keep running in the pool (8 workers), so a hung upstream can pile up work.
15. The install-listener redirect is a **301** (cached by browsers). The HTTPS URL host comes from the Host header, falling back to the local socket IP.
16. The listener is IPv4 only (AF_INET). TLS certs are not reloaded; restart after `make_certs.py`.
17. `parse_timeleft` with a bad day part raises inside status building, which aborts the whole `/api/status` request (connection dropped).
18. Library `ro_subs` is null for a movie Bazarr doesn't know. Movie-ready treats the same case as "subtitle missing".
19. Login with a non-string or over-64-char PIN still burns an attempt. The login route doesn't enforce the 4-6 digit format.
20. In demo with certs present, TLS is used. Demo login sets no cookie.
21. `/api/logout` itself requires a valid session (401 otherwise); the UI ignores errors.
22. The frontend expects any 503 without a JSON body to mean "PC off" (the service worker fallback).
23. strip-dv writes an MKV even for `.mp4` sources, then moves it onto the original `.mp4` path (§12.2).
24. strip-dv reads the Plex token from HKEY_USERS (written for SYSTEM) and passes it as a URL query; the app reads HKCU and uses a header.

---------------------------------------------------------------------------------------------------
## 12. PowerShell scripts to fold in

### 12.1 radarr-watchlist-sync.ps1 (Task Scheduler "Radarr Watchlist Sync", every 3 min)
No explicit error handling. A failing `Invoke-RestMethod` aborts only that statement; there is no logging. The Radarr base is hard-coded as `http://127.0.0.1:7878`.
1. Read `C:\ProgramData\Radarr\config.xml` as XML; the key is `Config.ApiKey`. Header `X-Api-Key`.
2. `GET /api/v3/importlist`, keep the entries with `enabled` true.
3. For each list: `POST /api/v3/command`, body `{"name":"ImportListSync","definitionId":<list.id>}`, `application/json`. A per-list sync is forced because without `definitionId` Radarr skips lists it checked recently.
4. `GET /api/v3/queue?pageSize=100`, take `.records`. For each record `q`:
   - `msgs` = every `q.statusMessages[].messages[]` joined with a space.
   - Skip unless `q.trackedDownloadState == "importBlocked"` AND `q.movieId` is truthy AND `msgs` matches `matched to movie by ID` (case-insensitive regex).
   - `GET /api/v3/manualimport?downloadId=<q.downloadId>&movieId=<q.movieId>&filterExistingFiles=false`, as an array.
   - `files` = items whose `rejections` is empty or null, mapped to `{path, movieId: q.movieId, quality, languages, releaseGroup, downloadId: q.downloadId, indexerFlags}`. The `quality` and `languages` objects are passed through unchanged.
   - If there are no files, skip. Else `POST /api/v3/command` `{"name":"ManualImport","importMode":"auto","files":[...]}`, JSON depth 10.
   - Purpose: FileList releases sometimes carry a different year (e.g. "The Father" 2020 came as "...2021..."). Radarr blocks those as "matched to movie by ID. Manual Import required"; they are auto-imported when there are no other rejections.

### 12.2 strip-dv.ps1 (Task Scheduler "Plex Strip DV", every 5 min as user `dragu`, with `-All`)
Parameters: `-Path <file>`, `-MovieId N` (default 0), `-All`. `$ErrorActionPreference='Stop'`.

Constants:
- MoviesRoot `D:\Media\Movies`
- LogDir `<script dir>\logs` (created), Log `logs\strip-dv.log`
- TmpDir `D:\Media\Downloads\.strip-dv`
- SeenFile `logs\strip-dv-seen.txt`, one line per file: `<FullName>|<Length>|<LastWriteTimeUtc.Ticks>`

Logging: append UTF-8 lines `yyyy-MM-dd HH:mm:ss  <msg>` (two spaces).

ffmpeg/ffprobe: the newest directory matching `C:\Users\dragu\AppData\Local\Microsoft\WinGet\Packages\Gyan.FFmpeg*\ffmpeg-*\bin` (FullName sorted descending, first). If none: log `EROARE: ffmpeg nu e instalat (winget Gyan.FFmpeg)` and exit 1. This happens before the mutex.

Helpers:
- Probe: `ffprobe -v error -show_format -show_streams -of json <file>`.
- HasDV: the first video stream's `side_data_list` has an entry with `side_data_type == "DOVI configuration record"`.
- Plex token: enumerate `HKEY_USERS\*\Software\Plex, Inc.\Plex Media Server`, take the first non-empty `PlexOnlineToken`.
- Update-Apps(file, id):
  1. Read the Radarr key again.
  2. If there is no id: `GET /api/v3/movie` and take the first movie whose `path` equals the file's parent directory (PowerShell `-eq` is case-insensitive).
  3. If there is an id: `POST /api/v3/command {"name":"RescanMovie","movieId":id}`.
  4. Plex: `GET http://127.0.0.1:32400/library/sections/1/refresh?path=<EscapeDataString(parent dir)>&X-Plex-Token=<tok>`. Section 1 is hard-coded.
  5. Without a token, log `  atentie: fara token Plex, biblioteca se actualizeaza la urmatoarea scanare`.

Invoke-StripDV(file, id) returns true if the file is OK now (no DV, or nothing doable), false on error:
1. Missing file: log `lipsa: <file>`, return false.
2. No DV: log `fara DV: <leaf>`, return true.
3. First video stream `color_transfer != "smpte2084"`: log `DV fara HDR10 (nu pot scoate DV): <file>`, return true.
4. If free space on the drive (`Get-PSDrive <first letter>`) is below `size*1.1`: log `EROARE: spatiu insuficient pentru <file>`, return false.
5. Maps: `-map 0:<first video index>`, plus every audio stream, plus subtitle streams whose `codec_name` is one of subrip, ass, ssa, mov_text, webvtt, text AND whose `tags.language` is one of eng, ron, rum, und, or empty (case-insensitive). PGS/VobSub subtitles and extra video streams (e.g. cover art) are dropped.
6. tmp = `TmpDir\<basename without extension>.nodv.mkv`. This is always MKV.
7. Log `remux start: <leaf> (<round(size/1GB,1)> GB)`.
8. Run `ffmpeg -hide_banner -nostdin -loglevel error -y -i <file> <maps> -map_chapters 0 -c copy -bsf:v dovi_rpu=strip=1 <tmp>`, with ErrorActionPreference set to Continue around it. Each output line is logged as `  ffmpeg: <line>`.
9. Exit code ≠ 0 or no tmp file: log `EROARE: ffmpeg a esuat (<code>), originalul ramane`, delete tmp, return false.
10. Verify by probing tmp. If DV is still present or `|duration_in - duration_out| > 2` s: log `EROARE: verificare esuata (DV inca prezent sau durata <out> vs <in>), originalul ramane`, delete tmp, return false.
11. Delete the original. Retry up to 60 times, 60 s apart, because Plex may be playing it. The library file is a hardlink to the torrent, so seeding is unaffected. If it still exists: log `EROARE: originalul e blocat, noul fisier ramane in <tmp>`, return false.
12. Move tmp onto the original path, keeping the original name and extension.
13. Log `gata in {min:N1} min: <leaf>` (culture-dependent decimal separator).
14. Update-Apps; on an exception log `  atentie: refresh Radarr/Plex esuat: <message>`.
15. Return true.

Main:
- Global mutex `Global\PlexStripDV`, `WaitOne()` with no timeout. AbandonedMutexException is ignored.
- With `-All`: load the seen set. Recurse `MoviesRoot` for `*.mkv` and `*.mp4`, skipping stamps already seen. Run Invoke-StripDV(file, 0); on an exception log `EROARE: <file>: <message>`. If it returned true, append the *current* file's stamp (after replacement) to the seen file. The seen file is never pruned.
- With `-Path`: a single run, errors logged, nothing recorded in the seen file.
- `ReleaseMutex` in `finally`.
