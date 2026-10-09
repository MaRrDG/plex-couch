The confirmation sheet that slides up before the PC shuts down or restarts.

- The consumer provides the action (shutdown or restart), the FileList warnings (movie titles plus what is left, e.g. "mai are nevoie de 45 h sau ratio 1") and the confirm/cancel handlers.
- Over a `scrim`, a `surface` sheet with `radius-xl` top corners and `shadow-sheet`. Title in `title-card` ("Oprești PC-ul?"), one `body` sentence ("Se oprește în 60 de secunde. Poți anula până atunci.").
- With warnings: an `accent` `body` paragraph with the exact rule ("Ai voie să închizi, dar în 7 zile trebuie să ajungă la ratio 1 sau 48 h.") and the affected titles below it in `ink`.
- Then the confirm row filled `danger` with `on-danger` text ("Oprește PC-ul" or "Repornește PC-ul") and a `plain` "Anulează" button.
- Slides up in 240 ms; instant under reduced motion. Tapping the scrim cancels. Focus moves into the sheet and returns on close.
