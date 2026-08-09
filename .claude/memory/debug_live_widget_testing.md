---
name: debug-live-widget-testing
description: Change widget state for live UI testing via config write-then-touch (not restart-spam, which 429s the usage poll → "??"); DPI-aware PrintWindow capture
metadata:
  type: project
---

Iterating on the widget UI (usage bars, compact view) means repeatedly flipping `compact_mode` / colors and re-viewing. Three operational traps, learned the hard way:

**Config hot-reload reads on the leading edge.** `config_watcher` reads the file on the *first* notify event, then debounces 150ms. A truncate-then-write (`python json.dump`, most editors) races it: the watcher reads the just-truncated/partial file, `Config::load_or_default` returns **all defaults** — silently dropping your change (e.g. `compact_mode`→false, `auto_resize`→"none") — and the real write is swallowed by the debounce. An `os.replace` / atomic-rename isn't caught by the file-watch at all (inode swap). **Reliable external trigger:** write the *complete* file, `sleep 0.6` (> the 150ms debounce), then `touch` it — the second Modify event fires against the now-complete file. Confirm via `widget.jsonl`.

**Restarting instead 429s the usage poll.** Each launch calls `refreshUsageLimits` on mount; enough rapid restarts trip the Anthropic OAuth usage endpoint's aggressive 429 → `status = network_error` → the compact caps render `??` and the bars go stale, which **blocks verifying colored numbers/levels** until a poll succeeds again (backoff can run minutes). Distinct from [[usage_endpoint_zeros_after_5h_cap]] (the post-5h-cap 0/null transient). So prefer the config-touch trigger over restart-spamming when iterating the usage UI; grep `widget.jsonl` for `HttpStatus(429` and `event usage_limits_updated` status.

**Screenshot the frameless HiDPI widget at true resolution.** Make the capturing process DPI-aware first — `SetThreadDpiAwarenessContext(-4)` (PerMonitorV2) — or `GetWindowRect` + `PrintWindow` read ÷scale virtualized (a 454px window captures as 303px; see [[debug_dpi_unaware_probe_virtualization]]). `PrintWindow(hdc, 2)` (`PW_RENDERFULLCONTENT`) grabs the WebView2 content. A 3× nearest-neighbor upscale of the header strip lets you inspect per-pixel — baseline alignment, sub-pixel color, 1px vs 2px borders.
