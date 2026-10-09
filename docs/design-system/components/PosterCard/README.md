A movie in the Filme grid: poster, title, size, and a Romanian-subtitle badge.

- The consumer provides the poster URL (2:3), title, year and size ("19,0 GB"), plus `roSubs` (true, false, or unknown when Bazarr is down: then no badge).
- Grid: 3 columns, `space-2` column gap, `space-3` row gap. Poster `radius-sm`; title in `tile-label` at 12px is too small, so use `data` weight 600 for the title and `data` in `muted` for the size.
- Badge bottom-left on the poster in `badge`: "RO" in `ink`, "fără RO" in `accent`, on an 82% `bg` plate with `radius-xs`.
- In "Urmează", replace the size with the status ("Apare digital pe 17 nov", "Se descarcă, 62%") and drop the badge.
- Never show the release file name.
