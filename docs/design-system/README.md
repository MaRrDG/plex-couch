Plex Couch is a phone remote for a Windows PC that runs Plex. It is used in a dark living room, at night, one-handed, while a movie plays on the TV. The look is "Cinema": movies lead, with real posters, a midnight slate ground and one accent, Plex amber. Everything else stays quiet.

## Structure

- Use four tabs, in this order: **Acasă**, **Filme**, **Seriale**, **PC**. Build them with `TabBar`. No other navigation, no hamburger menu; the one sub-screen is a series inside Seriale (`#seriale/<id>`), reached from its poster and left with a "Seriale" back button or the phone's back.
- **Seriale**: the same 3-column `PosterCard` grid ("3 nevăzute" badge bottom-left, an `accent` conversion marker top-right). A series shows its title, a header card (poster, year, seasons, what needs converting), season chips (pill-shaped, `surface` / `surface-2` selected, at least `size-tap` tall), a `ListGroup` of episodes (number, title, ✓ when watched, a video pill and an RO pill: `accent` text means "act on it", an `accent` fill means "converting now"), a sticky action bar above the `TabBar` ("Convertește următoarele" with a Stepper, or the running conversion with "Oprește"), and a details sheet per episode built like the `ActionSheet`. The movie sheet in Filme is the same sheet.
- While a conversion runs, Acasă shows a banner under its title: "Se convertește S01E04 · 45%", a `ProgressBar` and "Oprește conversia".
- **Acasă** answers "what is on the TV and what can I do now": a `NowPlayingCard`, then four `ControlTile`s in a 2×2 grid (Stinge monitoarele, Aprinde monitoarele, Stinge la play, Oprește PC-ul), then at most one compact "Se descarcă" row.
- **Filme**: a `SegmentedControl` (Bibliotecă / Urmează) over a 3-column grid of `PosterCard`s. "Urmează" lists Watchlist movies not yet downloaded, with a plain status ("Apare digital pe 17 nov", "Se caută în 4K pe FileList", "Se descarcă, 62%").
- **PC**: a `ListGroup` for Seed FileList, the D: disk as a `ProgressBar` with a data line, services as `StatusChip`s, a "Notificări" `ListGroup` (the ntfy topic in `data` with a `plain` "Copiază numele", and a "Trimite un test" row; when off, only the footnote "Pornește notificările cu plex-couch --setup-ntfy."), and the destructive `Button` "Oprește PC-ul" last.
- Keep one screen, one job. Details move to another tab before a screen gets a fourth section.

## Content and voice

- Write Romanian with diacritics (ș, ț, ă, î, â), in sentence case. No ALL CAPS anywhere, no emoji, no exclamation marks.
- Buttons say exactly what happens, and the result repeats the verb: "Stinge monitoarele", then the toast "Monitoarele s-au stins."
- Name things the way the user knows them: movie titles, not release file names ("The Godfather", not "The.Godfather.1972.2160p.WEB.H265-PETRiFiED.mkv"). Show "TV" and the device name, not "client" or "player".
- Numbers use Romanian decimal commas and tabular figures, set in `data`: "ratio 0,42, 12 din 48 h", "19,0 GB", "62%, 8 min".
- Empty states give the next step: "Nimic pe TV acum.", "Nicio descărcare. Adaugă un film în Watchlist-ul Plex."
- Errors name the problem and the fix: "Radarr nu răspunde. Repornește PC-ul sau verifică serviciul."
- Warnings about FileList seeding use the exact rule: "Ai voie să închizi, dar în 7 zile trebuie să ajungă la ratio 1 sau 48 h."

## Color

- Ground every screen in `bg`. Put grouped content on `surface`, and the selected or pressed state on `surface-2`. Separate rows with `line` hairlines inside a surface, never with borders around cards.
- `accent` is the only hue in normal use. Spend it on: progress fills, the active control tile (filled `accent` with `on-accent` text), the selected tab, a toggle that is on, tile icons, and the focus ring. Never on large text blocks or backgrounds.
- `ok` and `danger` mean state only, and each always comes with a word: "gata", "pornit", "oprit", "Oprește PC-ul". `danger` is an outline, icon or text until the final confirm row in the `ActionSheet`, the one place it fills.
- Text: `ink` for titles and values, `muted` for everything secondary. Both pass 4.5:1 on `bg`, `surface` and `surface-2`.
- Focus ring: 2px solid `accent`, 2px offset, on every focusable element (at least 3:1 on every surface).

## Type

- One family, Figtree from Google Fonts (`--font-sans`), weights 400/500/600/700, with the system stack as fallback. Load it with `<link rel="stylesheet" href="https://fonts.googleapis.com/css2?family=Figtree:wght@400;500;600;700&display=swap">`. In the PWA, self-host the same weights so it works offline.
- Use the scale as named: `title-screen` once per screen, `title-card` for the playing movie, `body-strong` for section headers and row titles, `tile-label` for tiles, `meta` and `data` in `muted` for everything secondary, `countdown` only for the shutdown seconds.
- Section headers are `body-strong` in `ink`, with an optional right-aligned `meta` in `muted` ("Acum pe TV ... Samsung TV, sufragerie"). No eyebrow labels and no letter-spaced caps.

## Imagery

- Posters come from Plex (or Radarr) at a 2:3 ratio and are always real; use a drawn placeholder only when Plex has none. Corners: `radius-sm` in the grid, `radius-xs` for mini posters, `radius-md` in the now-playing card. No frames, no shadows, no overlays except a `PosterCard` badge.
- The NowPlayingCard is the one place with a backdrop: the movie's art (Plex, else Radarr fanart) fills the card, blurred 14px and scaled 1.15, under a `backdrop-scrim`. Without art, or under reduced transparency, the card stays plain `surface`. The series header in Seriale reuses this card with the series art. No backdrop anywhere else.
- Badges sit bottom-left on the poster: "RO" in `ink`, "fără RO" in `accent`, on a 82% `bg` plate.

## Shape, space, layout

- Side gutter `space-4`; sections apart by `space-5`; tiles and posters apart by `space-3` and `space-2`.
- Radius has hierarchy: `radius-lg` for cards, tiles and groups; `radius-md` for buttons and segmented tracks; `radius-sm`/`radius-xs` for posters; `radius-pill` for chips, bars, toggles and PIN keys; `radius-xl` only on the sheet.
- Every tappable thing is at least `size-tap` tall. Content is capped at `size-content` and centred on wide screens.
- Only the `ActionSheet` casts a shadow (`shadow-sheet`). Cards are flat on `bg`.

## Motion and states

- Motion answers a tap only: the sheet slides up (240 ms ease-out), a toggle slides, a tile darkens to `surface-2` while pressed. No entrance animations, no loading shimmer. Under `prefers-reduced-motion`, switch instantly.
- While the PC is unreachable, show the cached screen with a "PC-ul nu răspunde" banner and disable every control.
- While a shutdown is pending, a `CountdownBanner` sits above the screen title on every tab until it is cancelled. While a shutdown is armed for the end of the movie, its "after the movie" variant sits there instead.

## Iconography

- 24px line icons, 1.8px stroke, round caps and joins, drawn in `currentColor`: home, film, pc (tabs); monitor-off, monitor-on, auto-play, power (tiles); download, disk, seed (rows). These are the system's own simple glyphs, not a third-party icon font.
- No Plex logo or other trademark: "Plex" appears as text only. Plex Couch is not affiliated with Plex Inc.
