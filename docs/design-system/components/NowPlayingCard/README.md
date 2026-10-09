The card at the top of Acasă showing what plays on the TV right now.

- The consumer provides the poster URL, title, year, device name, position, duration, progress (0–100) and the playback mode ("redare directă", "flux direct", "transcodare"). Paused shows "în pauză" after the year.
- Backdrop: the consumer may provide a backdrop URL (Plex art, else Radarr fanart). It fills the card (cover, centred), blurred 14px with saturate 1.1 and scaled 1.15, clipped by `radius-lg`, under a `backdrop-scrim` layer; poster, title, meta and bar sit on top unchanged. If the image fails or the user prefers reduced transparency, the card is plain `surface`.
- Layout: poster (`size-poster-hero`, `radius-md`) left; title in `title-card`, year and mode in `meta`, an accent `ProgressBar`, then position and duration in `data` at the ends.
- The section header above it reads "Acum pe TV" with the device name as right-aligned `meta`.
- Empty state: the same `surface` card holding one `body` line, "Nimic pe TV acum.", with no poster and no bar.
- Do not add controls (play/pause) to it; the TV remote does that.
