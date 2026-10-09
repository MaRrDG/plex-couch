A thin bar for playback position, downloads, seeding toward the FileList rule, and disk use.

- The consumer provides a value 0–100 and a tone: `accent` (default) or `ok` (the seed rule is met).
- 4px tall, `radius-pill`, track `surface-2`. Put the numbers in a `data` line below it in `muted`, `space-1` apart, never inside the bar.
- Use `role="progressbar"` with `aria-valuenow`.
- Do not use red for a full disk; say it in words ("Mai ai 40 GB pe D:").
