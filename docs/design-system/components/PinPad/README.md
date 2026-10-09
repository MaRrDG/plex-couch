The login screen: six PIN dots and a round numeric keypad.

- The consumer provides the submit handler and any error ("PIN greșit. Mai ai 3 încercări.", "Prea multe încercări. Încearcă din nou peste 9 min.").
- Centred: the app name in `title-screen`, "Introdu PIN-ul" in `meta`, six 10px dots (empty: 1.5px `muted` ring; filled: `ink`), the error line in `accent`, then a 3×4 grid of `size-key` round keys on `surface` with digits in 26px `ink`. The bottom row is empty, 0, delete.
- Six digits submit on their own. A wrong PIN shakes the dots once (none under reduced motion) and clears them.
- Pin the keypad low on the screen so the thumb reaches it.
