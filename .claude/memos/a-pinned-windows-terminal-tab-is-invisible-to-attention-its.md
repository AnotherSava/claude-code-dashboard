---
created: 2026-09-03 00:18:00
---

# A pinned Windows Terminal tab is invisible to attention: its caption never changes, so no…

A pinned Windows Terminal tab is invisible to attention: its caption never changes, so no EVENT_OBJECT_NAMECHANGE fires and departures to/from that tab go unobserved. (The second half of this entry described the caption-based pin detector, which no longer writes the stale flag at all; terminals::stale_check owns that now and catches the tab by a different route. The attention blindness above is the part that is still true.)
