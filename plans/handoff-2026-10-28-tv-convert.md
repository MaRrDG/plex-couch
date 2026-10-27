# Handoff: TV conversion + Romanian subtitles (2026-10-28, ~01:00)

The user stopped for the night. Talk to them in Romanian. Everything below was agreed in chat. Continue from here.

## 1. Where things stand right now

### Demon Slayer S1 (Sonarr series id 1, tvdb 348545)
- **Download location (fixed).** The qBittorrent category `sonarr` had an empty save path, so the 34.4 GB S1 pack went to `C:\Users\dragu\Downloads\sonarr`, where LocalService Sonarr can't import from.
  - Fixed: the category savePath is now `D:/Media/Downloads/sonarr`. qBit moved the torrent there.
  - Imported 26/26 with a manual `DownloadedEpisodesScan`. Plex "Seriale" shows 26 episodes.
  - Torrent `[Kaleido-Flax] Kimetsu no Yaiba (BD 1080p)` is seeding (stalledUP, 100%) from `D:\Media\Downloads\sonarr\...`.
- **Playback problem.** The release is **H.264 Hi10P** (10-bit), which the Samsung TV (Plex for Samsung) can't direct-play.
  - Plex transcodes it to DASH, and the TV buffers every ~25 s. This happens even on cable, and even though the server stays more than a minute ahead and the CPU sits at ~5%.
  - It is not the network: the user has 5 GHz Wi-Fi at 500 Mbps, and the 4K HEVC HDR10 movies direct-play perfectly.
- **Episodes converted by hand.** E01 and E02 were converted with the GPU, and they now direct-play with no buffering (verified in the Plex log).
  - The ffmpeg command is below.
  - The first E01/E02 encode lacked BT.709 tags and the TV showed wrong colours. Both were re-tagged with a remux (no re-encode).
  - E03–E26 are still Hi10P.
- **Seeding is safe.** The torrent originals are untouched. The library files for E01/E02 are now separate files, no longer hardlinks.

### Working ffmpeg command (AMD RX 9060 XT, ~12x realtime, about 2 min per 24-min episode)
```
ffmpeg -hide_banner -loglevel error -y -i <library file> -map 0 -c copy -c:v hevc_amf -profile:v main10 -pix_fmt p010le -quality quality -rc cqp -qp_i 19 -qp_p 21 -qp_b 23 -color_primaries bt709 -color_trc bt709 -colorspace bt709 -color_range tv -bsf:v hevc_metadata=colour_primaries=1:transfer_characteristics=1:matrix_coefficients=1:video_full_range_flag=0 -tag:v hvc1 <temp .mkv>
```
- ffmpeg is `%LOCALAPPDATA%\Microsoft\WinGet\Packages\Gyan.FFmpeg_...\ffmpeg-9.0.2-full_build\bin`.
- Temp dir is `D:\Media\Downloads\.reencode`.
- A throwaway helper for converting by hand is in the session scratchpad (`convert-eps.ps1`). It is not in the repo, and it is not needed once the feature ships.

### Movies (checked, nothing to do)
Back to the Future, Dune and The Lincoln Lawyer are all HEVC Main 10 HDR10, tagged and without DV, so they direct-play.

## 2. Feature A: "Conversie pentru TV" in Plex Couch (approved, in progress)

The user approved the following:
- **Placement:** a new section in the **PC** tab.
- **List:** series whose episodes the TV can't direct-play. The rule is H.264 with bit depth over 8, kept in one function. Each series shows the count of unwatched episodes that need converting and the next one.
- **Count:** the user picks how many episodes (2, 3, 4…) and presses "Convertește". It converts the **next N unwatched** episodes in order. It never starts on its own.
- **Encoding:** GPU, one episode at a time, ffmpeg at BelowNormal priority with no window, using the exact args above, writing to the temp dir. The output is verified (hevc, duration, stream counts) before it replaces the library file. Then it refreshes Plex for that path and runs Sonarr RescanSeries.
- **Skips the episode currently playing** in Plex.
- **UI:** progress (current episode, %, queue) and a **Stop** button.
- **ntfy notification** when the batch ends or fails.
- **No game detection.** The user said "nimic, o să am eu grijă".
- **Revert after watching** (user request: "după ce m-am uitat la un ep, ștergem conversia și lăsăm doar ce e pt FileList"). A ledger records each library file and its hardlinked original torrent file (verified by NTFS file ID).
  - When the episode is watched, not playing, and the original still exists in a torrent: replace the converted file with a hardlink to the original, so it costs no extra space.
  - If the torrent is already deleted: keep the converted file.
- **Hard constraint:** never touch the torrent files. The user said "sper că nu stricăm seed-ul pe FileList".
- **Out of scope:** movies, auto-start, pausing for games.

**Status:** a background agent was implementing this in the worktree `D:/projects/plex-couch/.claude/worktrees/agent-a0f80739363a02a00`, branch `worktree-agent-a0f80739363a02a00` (base dev cf64759).
- When last checked it had **no commits yet** and these uncommitted changes:
  - `src/jobs/convert.rs` (new, ~900 lines);
  - `src/jobs/{common,mod,strip_dv}.rs` (refactored ffmpeg helpers, FileId, same_volume, free_bytes into common);
  - `src/services/{arr_jobs,qbit_jobs}.rs`, `src/app.rs`, `src/http/routes.rs`, `tests/web.rs`.
- Its own report said these were still left:
  - wiring;
  - the UI (index.html, app.js, style.css, sw version bump);
  - unit and route tests;
  - README;
  - gitignore for `convert-ledger.json`;
  - build, test and clippy;
  - the commit.
- If the agent died with the session, resume in that worktree: check `git -C <worktree> status`, then `cargo build`/`cargo test`, then finish the remaining items.

## 3. Feature B: paced Romanian subtitle search (approved: "fixează să nu mai am problema")

- **Problem.** When a season pack imports, Bazarr fires 26 searches at once.
  - regielive answers TooManyRequests and gets disabled for 5 min.
  - subtitrari-noi.ro times out and gets throttled for 1 h.
  - Only E01 got a RO subtitle (regielive, 94%, auto-synced -0.08 s). Bazarr's wanted search runs only every 6 h.
- **Fix.** A new Plex Couch job (`src/jobs/subs.rs`) every 3 min:
  - Pick **one** episode with missing `ro`, next-unwatched first.
  - Call `PATCH /api/episodes/subtitles?seriesid=&episodeid=&language=ro&hi=False&forced=False`.
  - Keep a persisted per-episode backoff: 30 min, then 2 h, then a 6 h cap.
  - Optionally do the same for movies at lower priority.
  - Write unit tests and a README paragraph.
- This was queued to the same agent as a follow-up, in a separate commit `feat(subs): …`, after Feature A.

## 4. The real gap: no RO subtitles exist at the current providers

- Manual provider search (`GET /api/providers/episodes?episodeid=N`) gave:
  - E01: 1 result (regielive);
  - E02: 0 results;
  - E03: 0 results.
- regielive and titrari report status "Good". subtitrarinoi has ConnectTimeout.
- A web search found nothing legitimate for E02. Only spam/malware-style pages came up; avoid them.
- So Feature B alone won't produce subtitles for Demon Slayer. A provider with more anime content is needed.

**Next step (agreed): OpenSubtitles.com in Bazarr.**
- The **user** creates a free account on opensubtitles.com.
- The user enters their username and password **themselves** in Bazarr: http://127.0.0.1:6767 → Settings → Providers → OpenSubtitles.com.
- Never ask for the password in chat, and never store it.
- Then re-run the search for series 1, ideally paced one episode at a time (or via Feature B).
- The free tier has a daily download limit, which pacing helps with.
- While waiting, a fallback was offered and is not yet done: extract E02's embedded English ASS to an external `.en.srt`. ASS forces a transcode on the Samsung, but SRT direct-plays.

## 5. Deploy checklist (when the features are ready)
1. Overseer review:
   - run `cargo test -j 4` and `cargo clippy -j 4 --all-targets` at BelowNormal priority;
   - send the diff to a reviewer;
   - check that torrent files can never be modified or deleted by the convert/revert code paths;
   - check the interaction with `seed_keeper` link-count logic.
2. Commit in the worktree, then merge into `dev`. Never main.
3. Ask the user before building if they might be gaming. Build `cargo build --release -j 4` at BelowNormal priority.
4. `Stop-ScheduledTask "Plex Couch"`, copy `target\release\plex-couch.exe` to `D:\projects\plex-couch\plex-couch.exe`, then `Start-ScheduledTask "Plex Couch"`. Check the log and that RAM is ~15–20 MB.
5. Push to origin/dev is allowed for this repo, and the user has asked for it before. Confirm in the moment.
6. Test with the user: convert 2–3 Demon Slayer episodes from the PC tab, verify Direct Play and colours on the TV, and verify the revert after watching E01/E02.
7. Update memory `plex-media-stack.md` with the Hi10P/TV finding, the sonarr category save path fix, and the new jobs.
