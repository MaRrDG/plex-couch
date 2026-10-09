Full-width buttons for the few actions that are not control tiles.

- Variants: `primary` (filled `accent`, `on-accent` text: "Anulează oprirea", "Intră"), `danger` (1px `danger` outline and text: "Oprește PC-ul" at the end of the PC tab), `plain` (`surface` fill, `ink` text: "Repornește", "Închide").
- Height at least `size-tap`, `radius-md`, label in `body-strong`, centred.
- One primary per screen at most. The danger button never acts directly; it opens the `ActionSheet`.
- Labels are verbs that say what happens; no arrows or icons after the text.
