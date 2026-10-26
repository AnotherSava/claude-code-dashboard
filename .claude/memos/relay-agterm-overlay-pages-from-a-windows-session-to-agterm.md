---
created: 2026-10-14 20:10:44
---

# Relay agterm overlay pages from a Windows session to agterm on the Mac, once dashboard-to-dashboard auth is secure

Blocked on: secure authentication of dashboard apps across machines. Today a sync request proves only a holder of the one fleet-wide token, plus (optionally) a Tailscale node bound in `sync.peer_identity`. That is not enough for this feature, because an agterm page can run any agterm command, typing into any session included. The permission classifier refused to wire it up for that reason ("Create RCE Surface"), and it was right.

## The problem (reported 2026-10-14 by the achievement-overlay session on CHROME)
The user's chain is agterm on the Mac → SSH into the Windows box → tmux → Claude Code on Windows. `/review-manual` runs `review_picker.py` (dotfiles repo, `claude/scripts/review_picker.py`), which shows its findings table with `agtermctl session overlay open --html <page> --js --block --target $AGTERM_SESSION_ID` and reads the answer from stdout. On Windows neither `AGTERM_SESSION_ID` nor `agtermctl` exists, so the picker exits 3 with "not inside agterm", even though the user is looking at agterm.

## Design worked out (code written, then reverted)
- **Windows side:** loopback `POST /api/page` `{from_agent, html, device?, wait_ms?}` (behind `csrf_refusal(headers, true)`). The device is the one named, else the only peer heard from. `from_agent` is normalized with `peer_message::bare_project_id`. The call is held open while the page travels over the sync channel.
- **Mac side:** sync route `POST /api/sync/page`, registered above the `sync::guard` layer (bump the `guard_covers_every_route` count). Needs `sync.accept_pages` (new, default off) and `TailnetResolver::peer_is_bound`, the tier `sync::post_message` uses for a start. The page goes into a temp file and is removed after it closes.
- **Which tab:** remote tabs on the Mac carry agterm titles like `⇄ 🔵 achievement-overlay`. Join each tab title to a synced row with `terminal_title::title_names(title, true, attention::remote_labels(rows))`, the rule `attention::resolve_row` uses for a badged tab, and keep the tabs naming the row `{origin_device}/{agent}`. Exactly one tab shows the page; zero or several refuse (`no_surface` / `ambiguous_surface`). No row synced from that device refuses `no_such_row`.
- **Terminal seam:** two new `TerminalAdapter` methods. `page_targets() -> Option<Vec<PageTarget{key,title}>>` defaults to `Some(vec![])`. `show_page(key, html_path, timeout) -> PageOutcome {Submitted(Value), Dismissed, TimedOut, Failed}`. The Composite unions `page_targets` under `child|key` and routes `show_page`. agterm runs `session overlay open --html F --js --block --target S --window W`, with no `--follow`. Exit 0 means submitted (stdout `{"pageID","outcome":"submitted","value"}`), 2 means dismissed, 1 means error. On timeout it runs `session overlay close --target S --window W`.
- **Waits:** default 15 min, clamped to 10 s–60 min. The hop's reqwest timeout is the wait + 30 s, so the far side's `timed_out` answer arrives before the connection drops.
- **Receipt:** `{outcome: submitted|dismissed|timed_out|refused|failed, answer?, reason?, detail?}`. An answered page (any of the first three) is a 200; dismissing is the person's answer, not an error.

## Safer variant to weigh when this resumes
Relay data rather than HTML: send `{title, rows}` and have the Mac render a fixed page whose only command is submit. A peer then never puts its own script or agterm commands on the Mac's screen. The cost is that the page template moves out of `review_picker.py`.

## Follow-up outside this repo
`review_picker.py` needs a fallback to the new route, and its exit-3 message should say why it cannot reach agterm. The dotfiles (claude) session owns that script.
