# Plex Couch: Rust rewrite plan

Goal: replace the Python app (`app.py`, `plexcouch/`, `tools/make_certs.py`) and the two PowerShell
Task Scheduler jobs in `D:\Media\scripts` (`radarr-watchlist-sync.ps1`, `strip-dv.ps1`) with one
Rust binary, `plex-couch.exe`. Only the single process stays resident; no more `powershell.exe`
spawned every 3 and 5 minutes. Behaviour reference: `plans/rust-rewrite-spec.md` (the spec).

Branch `feat/rust-rewrite` in worktree `D:\projects\plex-couch-rust`, off `dev`, merged back to `dev`.

## Compatibility (must not break the user's phone)
- Same config file `config.local.json` (same keys, same deep-merge defaults) and the same
  `state.local.json` format. The existing PIN hash (`pbkdf2_sha256$...`), `secret`, `session_epoch`
  and session cookie `pc_session` (`<epoch>.<expiry>.<sig>`) stay valid, so the phone remains logged
  in after the switch.
- Same ports and listeners: HTTPS app on `--port + 1` (8766), plain-HTTP install/redirect page on
  `--port` (8765), `--allow-http` opt-out, the no-cert listener. Same certs in `certs/`.
- Same JSON shapes for every `/api/*` route (the frontend in `static/` is reused unchanged except for
  the seeding additions below). Static files are embedded in the binary at build time.
- Same CLI flags (`--host`, `--port`, `--set-pin`, `--setup-ntfy`, `--allow-http`, `--demo`, ...)
  plus a `make-certs` subcommand replacing `tools/make_certs.py` (same CA name constraints,
  key types, validity, file names, ACL lockdown).
- Same log file (`plex-couch.log`, 1 MB x 2 rotations) and the same never-log list.
- Built with `#![windows_subsystem = "windows"]`, run by the existing Task Scheduler entry "Plex Couch"
  at logon as `dragu` (only the action changes from pythonw to the exe).

## Gotchas from spec section 11: decisions
Preserve: 1, 2, 3, 7 (Python round-half-even and `19.0` float formatting where the UI shows it),
8 (implement Python `ipaddress.is_private` semantics exactly), 9, 10, 11, 12, 15, 16, 18, 19, 20, 21, 22.
Fix deliberately:
- 4: only set `primed` after a successful first Plex poll.
- 5: oversized POST bodies get 400 and the connection is closed.
- 6: methods other than GET/HEAD/POST get 405 with the normal security headers, after the IP/Host checks.
- 13: a torrent counts as FileList if any of its trackers is FileList.
- 14: per-upstream timeouts with cancellation (tokio), no piling up.
- 17: a bad `timeleft` value becomes null for that item instead of aborting `/api/status`.
- 23: strip-dv processes `.mkv` only.
- 24: one Plex token source (HKCU, via the header).

## New behaviour (not in the Python app)
1. **Watchlist sync, every 3 min** (replaces `radarr-watchlist-sync.ps1`):
   - Radarr: `ImportListSync` with `definitionId` for every enabled list (spec 12.1).
   - Sonarr (`http://127.0.0.1:8989`, key from `C:\ProgramData\Sonarr\config.xml`): the same for its
     enabled lists. Sonarr's own list refresh minimum is 6 h, hence the forcing.
   - Sonarr has no "remove and delete" list sync. Read the Plex Watchlist
     (`https://discover.provider.plex.tv/library/sections/watchlist/all`, type 2 = shows, by guid/tvdb)
     and delete from Sonarr (`deleteFiles=true`) every series that is tagged/added by the list and no
     longer on the Watchlist. Guard: if the Watchlist fetch fails or returns 0 items while Sonarr has
     list series, delete nothing (an outage must not wipe the library).
   - Auto import of downloads blocked with "matched to movie by ID. Manual Import required" (and the
     Sonarr equivalent "matched to series by ID"): GET manualimport for that downloadId, import the
     files with no rejections (current logic: `D:\Media\scripts\radarr-watchlist-sync.ps1`).
2. **Strip Dolby Vision, every 5 min** (replaces `strip-dv.ps1`, spec 12.2): same ffmpeg command
   (`-c copy -bsf:v dovi_rpu=strip=1`, keep video + all audio + text eng/ron/und subs), same checks,
   same seen-file and log locations, one job at a time, now over both `D:\Media\Movies` and
   `D:\Media\Seriale`. Then Radarr `RescanMovie` / Sonarr `RescanSeries` and a Plex path refresh
   of the right section (1 Filme, 2 Seriale).
3. **Seed keeper, every 5 min**: the only thing that deletes torrents. qBittorrent's own share
   limits are off (it doesn't refresh `seeding_time` for idle stalledUP torrents and loses it on
   restart). Keep a ledger in `state.local.json` (or `seed-ledger.json` next to it): per torrent hash,
   seconds seeded, added each tick only while qBittorrent answers, the torrent is not stopped/errored
   and a FileList tracker has status 2 (working); a single tick adds at most 10 min.
   - Delete the torrent with its download copy (`deleteFiles=true`; the library hardlink or the
     separate DV-stripped file stays) only when BOTH hold:
     a) H&R satisfied: ledger >= 49 h or ratio >= 1.0;
     b) watched or gone: movie (`radarr` category) mapped via Radarr history `downloadId` -> movieId ->
        file path -> Plex item `viewCount > 0`; or the movie no longer exists in Radarr. Series
        (`sonarr` category) via Sonarr history -> episodeIds -> episode files -> Plex episodes, all
        watched; or the series no longer exists in Sonarr.
   - Only categories `radarr`/`sonarr`, only content under `D:\Media\Downloads`. Unknown mapping or an
     API error: keep the torrent.
   - `/api/status` seeding items gain `watched` (bool|null) and `waiting_watch` (H&R done, not watched);
     the PC tab shows "gata, aștept să-l vezi" for those. The H&R progress uses the ledger.
4. **Next season automatically** (part of the watchlist job): for every Sonarr series added by the
   Plex Watchlist list, find the highest monitored season. When the user has watched (Plex
   `viewCount > 0` on the episodes in `D:\Media\Seriale`) all but the last 2 episodes of it, monitor the
   next season (if it exists and has aired episodes) and trigger `SeasonSearch`, so it is downloaded
   before the current one ends. Send an ntfy message "Sezonul N din <serial> se descarcă". Watched
   seasons stay until the series is removed from the Watchlist (user decision 2026-10-26).
5. **Sonarr in the UI**: out of scope for this rewrite (only the jobs above). Later.

## Work split
- Phase 1 (one implementer): Cargo project, config/state, HTTP/HTTPS/install/no-cert listeners,
  auth (PIN, sessions, LoginGuard, CSRF, Host allowlist, IP rule), static embedding, logging, CLI,
  `make-certs`; port `tests/test_security.py` and `tests/test_web.py`. Creates the module skeleton
  and trait seams for phases 2a/2b.
- Phase 2a (implementer): integrations (Plex, Radarr, Bazarr, Prowlarr, qBittorrent, ntfy), `/api/status`,
  image proxy, notifications, power/after-movie/auto-monitor logic, system actions; port
  `tests/test_logic.py` and `tests/test_notify.py`.
- Phase 2b (implementer, parallel): jobs module: watchlist sync + Sonarr cleanup + auto import,
  strip-DV worker, seed keeper; unit tests with fake upstreams.
- Phase 3 (overseer): review, run all tests, run the exe side by side on other ports, then switch the
  "Plex Couch" task to the exe, disable "Radarr Watchlist Sync" and "Plex Strip DV", watch for a day.
  Python files are removed in the final commit on this branch once the switch is confirmed.

Acceptance: every one of the 170 Python test cases has a Rust equivalent that passes;
`cargo clippy -- -D warnings` clean; release exe idles under 15 MB RAM.
