# plex-couch

A tiny self-hosted web app that turns your phone into a couch remote for the Windows PC
that runs your Plex server. From the sofa you can turn the PC monitors off/on, shut down or
restart the PC, and see what is playing, downloading, waiting and seeding. In the background
it keeps the Plex Watchlist, Radarr, Sonarr and qBittorrent in step: it syncs the Watchlist,
starts the next season of a show you are watching, strips Dolby Vision from files your TV
cannot play and deletes torrents once they have seeded long enough and you have watched them.

It is a single Rust executable (`plex-couch.exe`) with the web UI built in.

Not affiliated with or endorsed by Plex Inc.

> The UI text is in **Romanian**. The display name defaults to "Plex Couch" and can be changed
> with `app_name` in the config file (page title, header and PWA manifest all follow it).

## Features

Three tabs: **Acasă** (what is on the TV and the four controls), **Filme** (library and what
comes next, as a poster grid) and **PC** (seeding, disk, services, notifications, shut down).

- **Now playing on the TV** (Plex sessions): poster, title, device, progress, Direct Play /
  Direct Stream / Transcode.
- **Monitors**: turn the PC displays off or on with one tap.
- **Auto monitors off**: when a new Plex session starts playing on another device (e.g. the
  TV), the PC monitors are turned off once. Moving the mouse turns them back on and they stay
  on for the rest of that session.
- **Downloads**: Radarr queue with progress and time left.
- **FileList seeding**: every FileList torrent in qBittorrent, shown by movie title (matched
  through Radarr's grab history and queue, else parsed from the release name), with a progress
  bar towards the hit & run rule (ratio 1.0 or 48 h of seeding) and "gata" once safe, or
  "gata, aștept să-l vezi" while the seed keeper (below) waits for you to watch it. The seeded
  time comes from the seed keeper's own ledger once it knows the torrent.
- **Waiting list** ("Urmează"): monitored Radarr movies without a file, with a human status
  (e.g. waiting for the digital release on a given date) or the download progress.
- **Library**: Radarr movies as posters with size and whether a Romanian subtitle exists (Bazarr).
- **Posters and backdrop**: proxied from Radarr's local media covers and Plex's photo transcoder
  through `/img/movie/<id>`, `/img/plex/<id>` and, for the blurred backdrop behind the
  now-playing card, `/img/plex-art/<id>` (falling back to Radarr's fanart, `/img/movie-art/<id>`).
  Login required, digits-only ids, the API key and token stay on the PC. A drawn placeholder with
  the title is shown when there is no poster.
- **Disk space** and uptime.
- **Power**: shut down or restart after a 60 s countdown (then `shutdown /s /hybrid /t 0` or
  `/r /t 0`: the same as Start > Shut down, with Fast Startup and no forced close of unsaved
  apps), confirmed from an action sheet that warns about torrents still owing seed time, then a
  countdown banner with a cancel button.
- **Shut down after the movie**: from the same sheet, while something plays on the TV. When that
  Plex session ends at 90% or later (the credits), the normal 60 s countdown starts and a phone
  notification says so; stop the movie earlier and the PC stays on (it stays armed for the next
  session, for up to 3 h with nothing playing). A banner on every tab cancels it. This is kept in
  memory only: restarting Plex Couch disarms it.
- **Phone notifications** through [ntfy](https://ntfy.sh) (optional): "Film gata" when a new
  movie is downloaded (as soon as Bazarr has the Romanian subtitle, or after 20 minutes without
  it), "Spațiu puțin" when the Radarr drive drops below 60 GB free, the after-movie shutdown,
  "Sezon nou" when the next season of a show starts downloading, and a "Watchlist" warning
  when the Sonarr cleanup stops itself because too many shows vanished at once (see below).
- **Service status** for Plex, Radarr, Prowlarr, Bazarr and qBittorrent. Every service is
  optional; a service that is down only blanks its own section.
- "Cinema" dark UI built from a small design system (`docs/design-system/`): one accent (Plex
  amber), real posters, self-hosted Figtree font, no external CDNs.
- **Installable PWA over HTTPS** (Android Chrome): a local, name-constrained certificate
  authority, a service worker that caches the app shell, and a clear "PC-ul nu răspunde"
  state when the PC is off. Without certificates the app only serves an error page, unless
  you start it with `--allow-http`.
- **Tray icon** on the PC: a click opens the app in the browser, the right-click menu stops it
  (see [Tray icon](#tray-icon)).

### Background jobs

They start with the app (never in `--demo`), each on its own timer, and log what they do to
`plex-couch.log`. A failure is logged once until it changes, and never stops the loop. With
`--dry-run` they change nothing in Radarr, Sonarr, Plex, qBittorrent or your media files: every
delete, import, remux, rescan and notification is only logged (their own ledger and state are
still kept).

- **Watchlist sync** (every 3 minutes, first run 30 s after start):
  - Forces an `ImportListSync` for every enabled import list in Radarr and in Sonarr, so a
    movie or show added to the Plex Watchlist is picked up within minutes instead of on the
    *arr's own schedule.
  - **Auto import**: a finished download that Radarr/Sonarr grabbed for the right item but
    refuses to import because the release name does not match (typically a year mismatch:
    queue status "matched to movie by ID" / "matched to series by ID", import blocked) is
    imported automatically with the files that have no other rejection.
  - **Sonarr cleanup**: a series that came from the Plex Watchlist and is no longer on it is
    deleted from Sonarr together with its files. It is guarded heavily, and any doubt deletes
    nothing:
    - only series that carry the Sonarr tag `plex-watchlist` (set it on the Plex Watchlist
      import list; no such tag, no cleanup) *and* whose tvdb id was seen on the Watchlist at
      least once;
    - a series still counts as on the list when its tvdb, tmdb or imdb id or its title (without
      a trailing `(year)`) matches a Watchlist show;
    - nothing is deleted when the Watchlist fetch failed, was partial or shifted while paging,
      is empty, has a show without a tvdb id, or when Sonarr or the state file cannot be read;
    - a series is deleted only after it was missing from at least 2 successful fetches at
      least 5 minutes apart;
    - if more than 2 series would go in one run, none is deleted and one ntfy warning is sent.
  - **Next season**: for a show from the Watchlist that is still on it, once every episode but
    the last 2 of the highest monitored season is watched in Plex and the next season has aired
    episodes, that season is monitored, searched (`SeasonSearch`, if an aired episode has no
    file yet) and you get "Sezonul N din <serial> se descarcă", once per season.
- **Dolby Vision strip** (every 5 minutes, first run 60 s after start): every `.mkv` in the
  movie and series libraries that has a Dolby Vision layer on top of an HDR10 base is remuxed
  with ffmpeg (`-c copy -bsf:v dovi_rpu=strip=1`), so TVs without DV play it as plain HDR10
  instead of transcoding. It keeps the first video stream, every audio stream, text subtitles
  in English/Romanian/undetermined and the chapters. DV without an HDR10 base is left alone.
  - The remux goes to a temp folder (it needs 1.1x the file size free); the result is checked
    (no DV left, same duration within 2 s) and then replaces the original with a single rename,
    and only if the original has not changed since it was probed. While Plex holds the file it
    retries every minute for up to an hour. The library file is a hardlink to the torrent, so
    the seeding copy is not touched.
  - Afterwards Radarr (`RescanMovie`) or Sonarr (`RescanSeries`) rescans the item and Plex
    refreshes that folder.
  - Files already checked are remembered in a "seen" file (path, size, modification time);
    files whose remux failed are skipped for 24 h; leftover temp files older than 24 h are
    deleted. ffmpeg runs without a window at below-normal priority, one file at a time, on its
    own thread, and it has its own log file. It holds the `Global\PlexStripDV` mutex while it
    works, so it never overlaps an older `strip-dv.ps1` run that may still be scheduled.
- **Seed keeper** (every 5 minutes, first run 90 s after start): the only thing that deletes
  torrents; keep qBittorrent's own share limits off.
  - It keeps a ledger (`seed-ledger.json` next to the config) of the time each completed
    FileList torrent has really seeded: it starts from qBittorrent's `seeding_time` and then
    counts only while qBittorrent answers, the torrent is seeding and a FileList tracker reports
    it as working, on a clock that stops while the PC sleeps. An unreadable ledger skips the
    run and is never overwritten.
  - A torrent is deleted (with its download copy; the library hardlink stays) only when **both**
    hold: the FileList hit & run rule is met (at least 49 h seeded by the ledger, 48 h plus an
    hour of margin, or ratio 1.0) **and** you are done with it: the movie (Radarr) or every
    episode of that download (Sonarr) is watched in Plex, or the movie/series is no longer in
    Radarr/Sonarr.
  - Only torrents in the qBittorrent categories `radarr` or `sonarr` whose files are under the
    downloads folder are considered. The Radarr/Sonarr item is found through its download
    history and remembered with its tmdb/tvdb id, which must still match before "watched" or
    "gone" is trusted (an item re-added under a new id is kept).
  - A torrent with no Radarr/Sonarr history at all (and not in their queue) is deleted after the
    hit & run rule only if every one of its files has a hard-link count of 1, i.e. no library
    copy shares it.
  - Any API error, unknown mapping, missing file or failed check keeps the torrent.

- **Conversie pentru TV** (PC tab, only when you press it): the Samsung TV's Plex app cannot
  direct-play 10-bit H.264 (Hi10P), so Plex transcodes and the TV buffers. The PC tab lists the
  series with unwatched Hi10P episodes (from Sonarr's media info, no scan of the library), how
  many and the next one. Pick a series and a count (1-20, default 2) and press "Convertește":
  the next unwatched episodes, in episode order, are re-encoded one at a time to HEVC Main10 on
  the AMD GPU (`hevc_amf`, BT.709 colour tags, audio/subtitles/fonts copied), at below-normal
  priority without a window, into `jobs.convert.tmp_dir` (default `D:\Media\Downloads\.reencode`).
  An episode playing in Plex is passed over. The result is checked (HEVC, same length within
  1 s, same number of audio and subtitle streams) and then renamed over the library file; that
  breaks the library hardlink, so the torrent copy stays byte-identical for seeding. Sonarr
  rescans the series and Plex refreshes the folder. The tab shows the episode, the percent, the
  queue and the results; "Oprește conversia" kills ffmpeg at once and deletes its temp file
  (the library file stays as it was). One batch at a time; ntfy says when it is done.
  - Safety: nothing runs unless Sonarr and qBittorrent answer and the temp folder is a full
    path that does not overlap a Sonarr/Radarr root folder or a torrent's files. An episode
    whose library path is itself a file of a torrent (or when that cannot be checked) is skipped.
    ffmpeg is tied to the app (a Windows Job Object), so it never outlives plex-couch. Only
    the files the job itself creates are ever cleaned up after a crash (recorded in
    `state.local.json`, plus `<file>.partial` / `<file>.revert.tmp` next to converted files).
  - **Back to the original after watching**: `convert-ledger.json` (next to the config) records
    each converted file and the torrent file it was hardlinked to (found through Sonarr's import
    history, else qBittorrent's file list, and checked to be the same NTFS file). Every 5
    minutes, once an episode is watched in Plex and not playing, and the torrent file is still
    there with its size, still the very same NTFS file (volume serial + file index recorded at
    conversion) and still part of a torrent in qBittorrent, the library file becomes a hard
    link to it again (Plex and Sonarr are refreshed). If the torrent is gone or replaced by then
    (the seed keeper deleted it, or it was downloaded again), the converted file simply stays.
  - The seed keeper is not affected: for mapped torrents it decides by "watched", and for an
    unmapped torrent a converted episode just means its file has no library link (link count
    1), which is true: the library has its own copy.

- **Paced Romanian subtitles** (every 3 minutes, first run 75 s after start): when a whole
  season arrives at once, Bazarr searches every episode together and the Romanian providers
  rate-limit it, so most episodes wait for Bazarr's next wanted search (every 6 h). This job asks
  Bazarr for one `ro` search per run, for the episode you will watch next: the next unwatched
  episode of each series (by Plex's watched state), the most recently watched series first.
  Watched episodes are left to Bazarr; movies get a turn when no episode is due. After each
  search the same episode waits 30 min, then 2 h, then 6 h (kept in `state.local.json` under
  `subs_search`, dropped once the subtitle is found).
  - **Translation fallback** (anime: the Romanian providers have next to nothing): an episode or
    movie still missing `ro` after at least one of these searches is translated from English by
    Bazarr's own translator, only when Bazarr's translator is set to Gemini with an API key
    (Google Translate is never used). One at a time, at most one started per run and none while
    Bazarr runs any translation; episodes in the search order, movies unwatched and newest
    first, taking turns when both are due; at most `jobs.subs.translate_per_day` (default 40,
    the Gemini free tier allows 500) per local day. The English source: a `<video>.en.srt` /
    `.eng.srt` next to the video as is, else the embedded English text track (ASS/SSA/SRT, not
    Signs/Songs/Forced) extracted with ffmpeg and cleaned into `<video>.en.srt`: only the
    dialogue styles (OP/ED karaoke, signs, preview, titles, songs, credits dropped; if that
    leaves too little, only Default/Main/Dialogue/Italic/Flashback/Thought/Narrator styles),
    no override tags. Bazarr then rescans the folder and translates into `<video>.ro.srt`; when
    it is done its `# Subtitles translated with ... #` line is removed, Plex refreshes the folder
    and ntfy says "Subtitrare RO tradusă: ..." (`jobs.subs.translate_notify: false` to mute).
    A failed or stuck (15 min) translation deletes Bazarr's empty `.ro.srt` and waits 1 h, 6 h,
    then 24 h; no English source waits 24 h (state under `subs_translate`). An existing
    non-empty `.ro.srt` is never overwritten and the video is never touched.
    `jobs.subs.translate: false` turns it off; dry-run only logs.

The seed keeper and the next-season check read "watched" from Plex library section 1 (movies)
and section 2 (TV); these are fixed. The DV strip takes the section of each library from the
config.

## Requirements

- Windows 10/11 (monitor and power control use Win32 APIs and `shutdown.exe`).
- To build: [Rust](https://rustup.rs) stable with the `x86_64-pc-windows-msvc` toolchain and the
  Visual Studio Build Tools ("Desktop development with C++", for the MSVC linker). No other
  runtime is needed: the exe is self-contained and does not need openssl.
- Optional, all on the same PC: Plex Media Server, Radarr (v3 API), Sonarr v4 (v3 API, for the
  watchlist jobs), Bazarr, Prowlarr, qBittorrent (Web UI enabled, with "Bypass authentication for
  clients on localhost" or credentials in the config).
- For the Dolby Vision strip: ffmpeg, e.g. `winget install Gyan.FFmpeg` (found automatically in
  the WinGet packages folder, or set `jobs.strip_dv.ffmpeg_bin`). Without it the job logs once
  and does nothing.
- For the watchlist jobs: a Plex token (read from the registry, see below) and the Plex
  Watchlist import list in Radarr and Sonarr.

API keys are read at runtime from the services' own config files (Radarr and Sonarr
`config.xml`, Bazarr `config.yaml`) and the Plex token from the registry; they are never stored,
logged or returned. Paths, URLs and the tracker regex can be overridden in the config
(see `config.example.json`).

## Build

```
cargo build --release
```

Copy `target\release\plex-couch.exe` to the folder where it will run, for example the repo root
next to `config.local.json`. The exe looks for `config.local.json` and writes `plex-couch.log`
in its own folder, and `make-certs` writes `certs\` there by default, so keep them together.
Stop the running copy before you replace the exe.

## Setup

1. Copy `config.example.json` to `config.local.json` next to `plex-couch.exe` (optional —
   defaults are used for anything missing) and adjust URLs/paths if yours differ.
2. Set the PIN, 4 to 6 digits (stored only as a PBKDF2-SHA256 hash; 6-digit PINs auto-submit
   on the keypad):

   ```
   .\plex-couch.exe --set-pin 123456
   ```

3. Try it without touching the monitors, power or your files:

   ```
   .\plex-couch.exe --dry-run --allow-http
   ```

   Then open `http://<pc-lan-ip>:8765` on your phone. This already works in the browser, but
   only with `--allow-http`: without certificates the app otherwise serves just an error page.
   Set up HTTPS (next section) before you use it for real, then drop `--allow-http`.

   The release exe has no console window. Started from a terminal it prints to it, but the
   shell does not wait for it; stop it with *Ieșire* in its tray icon's menu, or run a debug
   build in the foreground with `cargo run -- --dry-run --allow-http` (Ctrl+C stops it).

4. **Run at logon** with Task Scheduler so it lives in your interactive session (required for
   the monitor controls):
   - Trigger: *At log on* of your user.
   - Action: *Start a program*, program `D:\path\to\plex-couch\plex-couch.exe`, arguments
     `--host 0.0.0.0 --port 8765 --config D:\path\to\plex-couch\config.local.json`.
   - Run only when the user is logged on; do not use "Run whether user is logged on or not".
   - Settings: untick "Stop the task if it runs longer than".

5. **Firewall**: allow inbound TCP 8765 (and 8766 for HTTPS) for the *Private* profile only,
   restricted to your LAN, for example (elevated PowerShell):

   ```
   New-NetFirewallRule -DisplayName "plex-couch" -Direction Inbound -Protocol TCP -LocalPort 8765,8766 `
     -Profile Private -RemoteAddress LocalSubnet -Action Allow
   ```

### Tray icon

While it runs, Plex Couch shows its icon in the notification area of the taskbar, with the
`app_name` as tooltip.

- **Click** (or double-click) opens the app in the default browser at
  `https://<pc-lan-ip>:8766/`: the IP the server certificate was made for (if the PC still has
  it), else the PC's private LAN IPv4, else `127.0.0.1`; with the HTTPS port from the command
  line or config. Without certificates it opens `http://<pc-lan-ip>:8765/` instead. With a
  specific `--host` that address is used.
- **Right-click** shows a menu with *Deschide Plex Couch* (the same as a click; the name follows
  `app_name`) and *Ieșire*,
  which stops the server cleanly, like Ctrl+C, and removes the icon. To start it again, run the
  scheduled task (or log off and on).
- If Explorer restarts, the icon comes back by itself; if the taskbar is not up yet at logon,
  the icon is added as soon as it is.
- `--no-tray` starts without the icon.

The log says which address a click opens (`tray: a click opens https://...`) at startup.

### Configuration

Everything is optional; missing keys use the defaults shown in `config.example.json`. Objects
merge with the defaults key by key, lists replace them. Relative `https` paths are resolved
against the config file's folder; give the `jobs` paths in full. The app writes `pin_hash`, `secret`, `session_epoch` and
`notifications.topic` itself.

| Key | What it does |
| --- | --- |
| `app_name` | Display name (page title, header, PWA manifest). |
| `settings.auto_monitors_off` | Turn the monitors off when a session starts on the TV (also a switch in the UI). |
| `filelist_tracker_regex` | Tracker URLs that count as FileList (case-insensitive). |
| `disks` | Drive letters shown on the PC tab. |
| `plex_poll_seconds` | How often Plex sessions are polled for the monitor and after-movie rules (at least 3). |
| `allowed_hosts` | Extra `Host` names to accept, e.g. `["plexpc.lan"]` (see Security). |
| `notifications` | `server`, `topic`, `disk_min_gb`, `ready_wait_minutes` (see ntfy below). |
| `https` | `port`, `cert`, `key`, `ca_cert`. |
| `services.<name>.url` | Base URL of `plex`, `radarr`, `sonarr`, `bazarr`, `prowlarr`, `qbittorrent`. |
| `services.plex.token` | Plex token; empty = read `token_registry_value` under `HKCU\<token_registry_key>`. |
| `services.radarr` / `sonarr` `.api_key` | API key; empty = read `<ApiKey>` from `config_xml`. |
| `services.bazarr.api_key` | API key; empty = read it from `config_yaml`. |
| `services.qbittorrent` | `username` / `password`, empty when localhost needs no login. |
| `jobs.watchlist.discover_url` | Where the Plex Watchlist is read (default `https://discover.provider.plex.tv`). |
| `jobs.strip_dv.libraries` | `[{"path": ..., "arr": "radarr"/"sonarr", "section": <Plex section>}]` to scan. |
| `jobs.strip_dv.ffmpeg_bin` | Folder with `ffmpeg.exe` and `ffprobe.exe`; empty = newest `Gyan.FFmpeg*\ffmpeg-*\bin` in `ffmpeg_packages`. |
| `jobs.strip_dv.ffmpeg_packages` | The WinGet packages folder to search (set it if winget installs elsewhere for you). |
| `jobs.strip_dv.tmp_dir` | Where remuxes are written before they replace the original. |
| `jobs.strip_dv.log`, `.seen` | The job's own log file and its list of files already checked. |
| `jobs.seed_keeper.downloads_root` | Only torrents under this folder can be deleted. |
| `jobs.subs.translate` | Translate missing RO subtitles from English with Bazarr's Gemini translator (default `true`). |
| `jobs.subs.translate_per_day` | Translations started per local day at most (default `40`). |
| `jobs.subs.translate_notify` | ntfy when a translation is done (default `true`). |
| `jobs.subs.movie_section` | Plex movies section refreshed after a movie translation (default: the DV strip's Radarr library, else `1`). |

The `jobs` defaults are the paths of the original setup (`D:\Media\Movies`, `D:\Media\Seriale`,
`D:\Media\Downloads`, ...); set them to yours.

### Phone notifications (ntfy)

Plex Couch can push notifications to your phone through [ntfy](https://ntfy.sh), a free
publish/subscribe service with an Android and iOS app. Nothing to sign up for: notifications go
to a *topic*, and anyone who subscribes to that topic name gets them.

1. Create a topic (a long random name; re-running keeps it, `--new-topic` replaces it):

   ```
   .\plex-couch.exe --setup-ntfy
   ```

   It prints the topic and its subscribe link. Restart Plex Couch afterwards.
2. Install the ntfy app on the phone and subscribe to that topic (the PC tab has the name and a
   "Copiază numele" button), then tap "Trimite un test" on the PC tab.

What you get: "Film gata" when Radarr finishes a new movie (right away once Bazarr has the
Romanian subtitle, otherwise after `ready_wait_minutes`), "Spațiu puțin" once when the drive of
Radarr's root folder drops below `disk_min_gb` free (again only after it has gone back above
`disk_min_gb + 10`), "Oprire după film", "Sezon nou" from the next-season job and the
"Watchlist" warning from the Sonarr cleanup. Movies already in the library when notifications
are first turned on never notify. What was already sent is remembered in `state.local.json` next
to the config (git-ignored), so a restart does not repeat anything. The `notifications` block in
the config sets `server` (for a self-hosted ntfy), `disk_min_gb` and `ready_wait_minutes`.

The topic name works like a password for reading the notifications (movie titles and free disk
space): keep it private. It is stored only in the config file, shown only to logged-in clients
and never logged. With `--dry-run` notifications are only logged, unless `server` points to this
PC (127.0.0.1/localhost, for testing with a local server).

### HTTPS and installing the app on Android

Chrome only installs web apps and runs service workers on HTTPS. `plex-couch make-certs`
creates your own local certificate authority (CA) and a server certificate signed by it; the
phone trusts the CA once and then the app is a normal installable PWA.

1. Create the certificates. Use the PC's LAN IP; a fixed IP or a DHCP reservation on the router
   is recommended:

   ```
   .\plex-couch.exe make-certs --ip 192.168.1.50 --ip 127.0.0.1 --dns localhost
   ```

   This writes `certs\ca.key`, `certs\ca.pem`, `certs\ca.crt` (DER, for the phone),
   `certs\server.key` and `certs\server.pem` next to the exe (`--out` picks another folder).
   The folder is git-ignored. Private keys are never printed. Only private IPv4 addresses,
   `localhost` and `*.local` names are accepted (the CA could not vouch for anything else).
2. Restart the app. When `certs\server.pem` and `certs\server.key` exist it serves the app over
   HTTPS on port **8766**, and port 8765 only serves an install page, the CA certificate
   (`/ca.crt`) and redirects everything else to HTTPS. When they are missing or cannot be read,
   port 8765 serves only a 503 page saying so (and the log says it too); the app, the API and
   the login are not served at all. Plain HTTP for the whole app needs `--allow-http`.
3. On the phone open `http://<pc-lan-ip>:8765` and follow the steps on the page: download the
   certificate, install it under *Settings → Security & privacy → More security settings →
   Encryption & credentials → Install a certificate → CA certificate* (names vary slightly by
   phone), open `https://<pc-lan-ip>:8766`, then Chrome menu → *Install app*.

The server certificate is valid for 397 days. Re-running `make-certs` renews only the server
certificate and keeps the CA, so the phone does not need anything new; restart the app
afterwards. `--force-ca` replaces the CA (the phone then needs the new `ca.crt`). If the PC's IP
changes, re-run it with the new `--ip`.

Paths and the port can be changed with `--https-port`, `--cert`, `--key`, `--ca-cert` or the
`https` block in the config (relative paths are resolved against the config file's folder).

### Command line

```
plex-couch [--host 0.0.0.0] [--port 8765] [--https-port 8766] [--cert certs/server.pem]
           [--key certs/server.key] [--ca-cert certs/ca.crt] [--config config.local.json]
           [--dry-run] [--allow-http] [--no-tray]
plex-couch --set-pin <digits> [--config config.local.json]
plex-couch --setup-ntfy [--new-topic] [--config config.local.json]
plex-couch --demo --host 127.0.0.1 [--port 8765]
plex-couch make-certs --ip <lan-ip> [--ip ...] [--dns localhost] [--out certs] [--force-ca]
```

`--config` defaults to `config.local.json` next to the exe. `--dry-run` logs monitor and power
actions instead of executing them, and makes the background jobs read-only (see above).
`--allow-http` serves the whole app over plain HTTP when no usable certificate is found (by
default only a 503 page is served then). `--demo` serves the UI with fake data and no login, for
screenshots and UI work; it refuses to start unless the host is `127.0.0.1`, never executes any
action and runs no background job. Open `/#login` to see the login keypad in demo mode.
`--no-tray` starts without the system-tray icon (see [Tray icon](#tray-icon)); the one-shot
commands (`--set-pin`, `--setup-ntfy`, `make-certs`) never show one. Options can be abbreviated to a unique prefix (`--allow` = `--allow-http`), `-h` prints the
help, and a usage error exits with code 2.

Logs go to `plex-couch.log` next to the exe (rotating, 1 MB x 2). The exe has no console when
started by Task Scheduler, so check that file if something does not work. The DV strip job also
writes its own log (`jobs.strip_dv.log`).

## Security notes

- **LAN only. Never expose this app to the internet** (no port forwarding, no reverse proxy).
  It can shut down your PC.
- **HTTPS is required.** With certificates the app itself is only served over HTTPS (TLS 1.2+)
  with `Strict-Transport-Security: max-age=31536000`; the plain-HTTP port only offers the
  install page, the CA certificate and redirects (no HSTS there). The session cookie gets the
  `Secure` flag over HTTPS. If the certificate or key is missing or unreadable, the app does not
  fall back to plain HTTP: port 8765 answers every request with a 503 page that says to run
  `plex-couch make-certs`, and the log says the same. Plain HTTP for the whole app only runs
  with an explicit `--allow-http`.
- **The local CA is name-constrained.** Its certificate carries a critical X.509
  `nameConstraints` extension that permits only private IPv4 ranges (192.168.0.0/16, 10.0.0.0/8,
  172.16.0.0/12), loopback (127.0.0.0/8), `localhost` and `*.local` names. Even if `ca.key` were
  stolen, certificates it signs for public web sites would be rejected by the phone, so
  installing it does not let anyone intercept your normal browsing. The CA also permits no
  e-mail names (`permitted;email:.invalid`) and carries `extendedKeyUsage = serverAuth`, so it is
  good for TLS server certificates only. Still keep `certs\` private and never commit it.
- Requests from non-private, non-loopback addresses are rejected with 403.
- **Host allowlist** (against DNS rebinding): every listener answers only requests whose `Host`
  is an IP address (IPv4 or IPv6), `localhost` or a `*.local` name, or a name you list in the
  config (`"allowed_hosts": ["plexpc.lan"]`). Anything else, including a missing `Host`, gets
  `421 Misdirected Request`.
- At most 64 connections per port at once (more are closed right away), and a connection that
  sends nothing for 15 seconds (the TLS handshake included) is dropped.
- Login with a 4-6 digit PIN; the PIN is stored only as a salted PBKDF2-HMAC-SHA256 hash
  (200k iterations) and verified in constant time.
- Brute-force protection: 5 wrong PINs from one IP lock that IP for 10 minutes. Each attempt
  is counted *before* the PIN is checked, so parallel requests cannot get more than 5 checks per
  lockout; PINs are checked one at a time and at most 4 login requests are handled at once
  (more get a 429). After 20 failed attempts within an hour in total, IPs that have not logged
  in successfully in the last 30 days may try only once a minute (all of them together); IPs
  that have logged in before are not slowed down, so another device on the network cannot lock
  you out. Those IPs are remembered for 30 days in `state.local.json` (git-ignored).
- Sessions are HMAC-signed cookies (`HttpOnly; SameSite=Strict`) valid for 30 days and renewed
  automatically when less than 15 days are left, so a phone in use stays logged in. *Ieși din
  cont* logs out that phone (the cookie is deleted). *Ieși de pe toate dispozitivele* (PC tab)
  invalidates every session at once: each token carries a session epoch kept in the config
  (`session_epoch`), and the button increments it. Changing the PIN also invalidates all
  sessions.
- Every state-changing call is a POST that must carry the `X-Requested-With: plex-couch`
  header (CSRF guard). Strict Content-Security-Policy, no third-party resources.
- The service worker caches the static app shell and, in a separate small cache (150 entries),
  poster images; `/api/*` responses are never cached. The page keeps a copy of the last status in
  the browser's local storage so the cached screen can be shown while the PC is off; it is
  removed on logout.
- Tracker URLs (which contain your passkey), API keys and tokens are never logged or returned
  by the API.
- `config.local.json` contains the PIN hash, the cookie secret and the ntfy topic; it is
  git-ignored, like `state.local.json` and `seed-ledger.json`.
- **Keep the secrets readable only by you.** `make-certs` restricts `ca.key` and `server.key`
  itself (it removes inherited permissions with `icacls` and grants only you and SYSTEM). Do the
  same for the config and the rest of the certs folder, for example in PowerShell from the
  folder of the exe:

  ```
  icacls config.local.json /inheritance:r /grant:r "${env:USERNAME}:F" "SYSTEM:F"
  icacls certs /inheritance:r /grant:r "${env:USERNAME}:(OI)(CI)F" "SYSTEM:(OI)(CI)F"
  ```

  (`cmd.exe`: use `"%USERNAME%:F"`.) Run Plex Couch as that same user.
- Requests to Radarr, Sonarr, Bazarr and Plex that carry an API key or token never follow an
  HTTP redirect, so a key is never sent to another address. The Plex token always goes in the
  `X-Plex-Token` header, never in a URL; besides the local Plex server it is sent only to plex.tv
  (`jobs.watchlist.discover_url`) to read your Watchlist.
- Notifications are sent from the PC to the ntfy server as JSON over HTTPS; they carry only
  movie and show titles, free disk space and the shutdown notice.
- The background jobs delete things (Sonarr series with their files, torrents with their
  download copy) only under the rules above; try them with `--dry-run` first and read the log.

## Resource use

One process with a small async runtime (two worker threads, plus a thread for the DV strip job,
one for the tray icon and short-lived ones for blocking calls). It typically uses about 8-20 MB
of RAM (the tray icon accounts for about 3 MB of that) and next to no CPU between polls; ffmpeg
runs only while a file is remuxed, at below-normal priority.

## Design system

`docs/design-system/` holds the rules (`README.md`), the tokens (`tokens.json`) and one folder per
component with its rules and a reference preview. `static/tokens.css` is generated from the
tokens; after changing `tokens.json` run:

```
cargo run --example build_tokens              # writes static/tokens.css
cargo run --example build_tokens -- --check   # exits 1 if static/tokens.css is out of date
```

The static files are embedded in the exe at build time, so rebuild after changing them.
`static/style.css` uses only those variables for colours, spacing, radii and sizes. The UI font
is [Figtree](https://github.com/erikdkennedy/figtree) (SIL Open Font License 1.1, see
`static/fonts/OFL.txt`), self-hosted as two variable woff2 files (latin and latin-ext, weights
400-700) so the app works offline and Romanian diacritics render in the same face.

The exe has its own icon (Explorer, Task Manager, the tray): `assets/plex-couch.ico`, drawn from
the shapes of `static/icon.svg` at 16, 24, 32, 48 and 256 px, and embedded by `build.rs` at build
time. After changing `icon.svg`, update the shapes in `src/icon.rs` and run:

```
cargo run --example make_icon              # writes assets/plex-couch.ico
cargo run --example make_icon -- --check   # exits 1 if assets/plex-couch.ico is out of date
```

## Development

```
cargo test
cargo clippy --all-targets -- -D warnings
cargo run --example build_tokens -- --check
cargo run --example make_icon -- --check
```

The unit tests live next to the code and cover the pure logic (hit & run rule, tracker
matching, IP checks, session tokens, PIN hashing, lockouts, Romanian status text, auto-monitor
decisions, release-name parsing, torrent-to-movie matching, the ntfy payload, the movie-ready
and low-disk notifications, the after-movie shutdown decisions, the CA name constraints, the
address the tray icon opens, the generated icon) and the
background jobs against fake Radarr/Sonarr/Plex/qBittorrent servers and a fake ffmpeg: the
watchlist guards, the next-season rule, the DV strip replacement and its failure paths, the
seed keeper's ledger and every keep/delete case. The tests in `tests/` run the real listeners:
`security.rs` covers the hardening (parallel login bursts, the session epoch and renewal, the
Host allowlist, the HTTPS-required startup with the exe in a child process, LAN-only TV
sessions, cancelling after the shutdown command ran with `shutdown.exe` replaced by a stub, the
connection cap and timeout, upstream redirects), `web.rs` the routes, poster proxy, static
files, the generated tokens.css and HTTPS with certificates from `make-certs`, and `cli.rs` the
flags and `make-certs` validation. No test needs the network, a real service or admin rights;
the services point at closed local ports.

## License

MIT — see [LICENSE](LICENSE).
