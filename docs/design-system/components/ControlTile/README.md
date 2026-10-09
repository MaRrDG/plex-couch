A large square-ish button for the four actions on Acasă, laid out in a 2×2 grid.

- The consumer provides an icon, a label (`tile-label`), an optional state line (`meta`) and a variant: `action` (default), `toggle` (on/off) or `danger`.
- Default: `surface` fill, icon in `accent`, label in `ink`. Pressed: `surface-2`.
- Toggle on: the whole tile fills with `accent`, icon and label in `on-accent`, the state line "Pornit" in `on-accent-muted`. Off: default look with "Oprit". Use `aria-pressed`.
- Danger ("Oprește PC-ul"): default fill, icon in `danger`, state line "în 60 de secunde". It never acts directly; it opens the `ActionSheet`.
- Minimum height `size-tile`, radius `radius-lg`, padding `space-4`, gap between tiles `space-3`.
- Do not put more than four tiles on Acasă, and do not use tiles for navigation.
