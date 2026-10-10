The banner at the top of every tab while a shutdown or restart is pending.

- The consumer provides the action, the seconds left and the cancel handler.
- On `surface`, `radius-lg`: the line "PC-ul se oprește în" in `body-strong` with the seconds in `countdown` (tabular, "42 s" with a non-breaking space), a `danger` `ProgressBar` draining to zero, and a full-width `primary` `Button` "Anulează oprirea".
- It sits above the screen title and stays on every tab until cancelled or the PC goes down. After cancel, the toast says "Oprirea a fost anulată."
- Under reduced motion the number still updates each second; only the bar stops animating smoothly.
- Variant "after the movie": while a shutdown is armed for the end of the playing movie, the same banner shows "PC-ul se oprește după The Godfather." in `body-strong`, with no seconds and no bar, and a `plain` `Button` "Nu mai opri" (on `surface-2`). Toasts: "PC-ul se oprește după film." when armed, "Oprirea după film a fost anulată." after cancel. When the movie ends it gives way to the normal countdown.
