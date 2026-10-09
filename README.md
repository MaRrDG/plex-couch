# plex-couch

A tiny self-hosted web app that turns your phone into a couch remote for the Windows PC
that runs your Plex server. From the sofa you can turn the PC monitors off/on, shut down or
restart the PC, and see what is playing, downloading, waiting and seeding.

Not affiliated with or endorsed by Plex Inc.

> The UI text is in **Romanian**. The display name defaults to "Plex Couch" and can be changed
> with `app_name` in the config file (page title, header and PWA manifest all follow it).

## Features

- **Now playing on the TV** (Plex sessions): title, player, state, progress, Direct Play /
  Direct Stream / Transcode.
- **Monitors**: turn the PC displays off or on with one tap.
- **Auto monitors off**: when a new Plex session starts playing on another device (e.g. the
  TV), the PC monitors are turned off once. Moving the mouse turns them back on and they stay
  on for the rest of that session.
- **Downloads**: Radarr queue with progress and time left.
- **FileList seeding**: every FileList torrent in qBittorrent with a progress bar towards the
  hit & run rule (ratio 1.0 or 48 h of seeding) and a check mark once safe.
- **Waiting list**: monitored Radarr movies without a file, with a human status
  (e.g. waiting for the digital release on a given date).
- **Library**: Radarr movies with size and whether a Romanian subtitle exists (Bazarr).
- **Disk space** and uptime.
- **Power**: shut down or restart in 60 s, with an in-page confirmation that warns about
  torrents still owing seed time, a live countdown and a big cancel button.
- **Service status chips** for Plex, Radarr, Prowlarr, Bazarr and qBittorrent. Every service is
  optional; a service that is down only blanks its own section.
- Installable as a PWA (manifest + icon), mobile-first dark UI, no external CDNs or fonts.

## Requirements

- Windows 10/11 (monitor and power control use Win32 APIs and `shutdown.exe`).
- Python 3.12+ — **standard library only**, nothing to `pip install`.
- Optional, all on the same PC: Plex Media Server, Radarr (v3 API), Bazarr, Prowlarr,
  qBittorrent (Web UI enabled, with "Bypass authentication for clients on localhost"
  or credentials in the config).

API keys are read at runtime from the services' own config files (Radarr `config.xml`, Bazarr
`config.yaml`) and the Plex token from the registry; they are never stored, logged or
returned. Paths, URLs and the tracker regex can be overridden in the config
(see `config.example.json`).

## Setup

1. Copy `config.example.json` to `config.local.json` next to `app.py` (optional — defaults are
   used for anything missing) and adjust URLs/paths if yours differ.
2. Set the PIN, 4 to 6 digits (stored only as a PBKDF2-SHA256 hash; 6-digit PINs auto-submit on the keypad):

   ```
   python app.py --set-pin 123456
   ```

3. Try it in the foreground first, without touching the monitors or power:

   ```
   python app.py --dry-run
   ```

   Then open `http://<pc-lan-ip>:8765` on your phone and "Add to Home Screen".

4. **Run at logon** with Task Scheduler so it lives in your interactive session (required for the
   monitor controls) without a console window:
   - Trigger: *At log on* of your user.
   - Action: `pythonw.exe` with arguments `"C:\path\to\plex-couch\app.py"` and
     *Start in* set to the project folder.
   - Run only when the user is logged on; do not use "Run whether user is logged on or not".

5. **Firewall**: allow inbound TCP 8765 for the *Private* profile only, restricted to your LAN,
   for example (elevated PowerShell):

   ```
   New-NetFirewallRule -DisplayName "plex-couch" -Direction Inbound -Protocol TCP -LocalPort 8765 `
     -Profile Private -RemoteAddress LocalSubnet -Action Allow
   ```

### Command line

```
app.py [--host 0.0.0.0] [--port 8765] [--config config.local.json] [--dry-run]
app.py --set-pin <digits> [--config config.local.json]
app.py --demo --host 127.0.0.1 [--port 8765]
```

`--dry-run` logs monitor and power actions instead of executing them.
`--demo` serves the UI with fake data and no login, for screenshots and UI work; it refuses to
start unless the host is `127.0.0.1` and never executes any action. Open `/#login` to see the
login keypad in demo mode.

Logs go to `plex-couch.log` next to `app.py` (rotating, 1 MB x 2). `pythonw.exe` has no console,
so check that file if something does not work.

## Security notes

- **LAN only. Never expose this app to the internet** (no port forwarding, no reverse proxy).
  It speaks plain HTTP and can shut down your PC.
- Requests from non-private, non-loopback addresses are rejected with 403.
- Login with a 4-6 digit PIN; the PIN is stored only as a salted PBKDF2-HMAC-SHA256 hash
  (200k iterations) and verified in constant time.
- Brute-force protection: 5 wrong PINs from one IP lock that IP for 10 minutes; 20 failures
  per hour in total lock everyone out for 10 minutes.
- Sessions are HMAC-signed cookies (`HttpOnly; SameSite=Strict`, 180 days). Changing the PIN
  invalidates all existing sessions.
- Every state-changing call is a POST that must carry the `X-Requested-With: plex-couch`
  header (CSRF guard). Strict Content-Security-Policy, no third-party resources.
- Tracker URLs (which contain your passkey), API keys and tokens are never logged or returned
  by the API.
- `config.local.json` contains the PIN hash and the cookie secret; it is git-ignored.

## Tests

```
python -m unittest discover -s tests -v
```

The tests cover the pure logic (hit & run rule, tracker matching, IP checks, session tokens,
PIN hashing, lockouts, Romanian status text, auto-monitor decisions) and need no network or
Windows APIs.

## License

MIT — see [LICENSE](LICENSE).
