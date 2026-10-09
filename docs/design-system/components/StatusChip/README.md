A pill naming a service and whether it runs, used in the Servicii section of the PC tab.

- The consumer provides the service name and its state (running or down).
- Running: a 7px `ok` dot and the name in `ink` on `surface`. Down: a `danger` dot and the name followed by "oprit". The word carries the state, so it reads without color.
- When all services run, the section header's `meta` says "toate pornite"; otherwise "1 oprit".
- Chips wrap with an `space-2` gap. Height at least 32px, padding 6px 10px, `radius-pill`.
