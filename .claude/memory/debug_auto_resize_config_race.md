---
name: debug_auto_resize_config_race
description: Auto-resize "too tall" root cause = get_config mount race making frontend auto_resize='none'; fixed by managing ConfigState first in setup() + one authoritative getConfig() re-read at end of mount. Debug via window rect + widget.jsonl, beware multi-window log interleaving.
metadata:
  type: project
---

**The "dashboard too tall with auto-resize on" bug (2026-07-01).** The window opens at its launch/saved size and never shrinks to fit the rows. Intermittent (~1/3 of launches).

**Root cause:** a startup race. The webview's mount-time `getConfig()` can beat `setup()`'s `app.manage(ConfigState)` (lib.rs), so `get_config` returns `Config::default()` with `auto_resize: None`. The frontend then reads `config.auto_resize === 'none'` and `measureAndSend` early-returns on its very first guard — auto-resize is silently disabled for the whole session, so the window is frozen wherever it launched. `show_window` re-emits `config_updated` to fix this, but that event can be missed, AND the **safety-net reveal timer** (lib.rs `spawn`, ~1500ms) revealed the window WITHOUT re-pushing config/setup — the gap.

**Fix — proper sequencing, no poll (a retry poll was tried first and rejected as a band-aid):**
- Backend `lib.rs`: manage `ConfigState` **first** in `setup()` (moved above the other stores / window setup) so `get_config` is far less likely to hit the `unwrap_or_default()` path. This shrinks the race to near-zero.
- Frontend `App.svelte`: **one authoritative `getConfig()` re-read** at the end of `onMount` (after the other mount round-trips have proven the backend is up), placed before the reveal `finally` so a stalled pre-show `requestAnimationFrame` can't skip it. A single sequenced read, not a loop.
- Backend `lib.rs`: the safety-net reveal timer also calls `emit_config_updated` + `emit_setup_state` (mirroring `show_window`) — a cheap backstop.
- Frontend: a `scheduleMeasure()` after `showWindow()` for a reliable post-show measure.
- Verified **9/9 restarts** land `auto_resize:'up'` + a correct apply (was ~1/3 failing).
- Did NOT touch the core `measureAndSend` dedup (a bidirectional-reconcile was tried and reverted — the dedup is loop-prone; on a fresh launch `lastSentHeight===-1` so the existing overflow-dedup already shrinks a too-tall window once auto_resize is enabled).
- **Why no clean "backend-then-frontend" barrier:** the webview is coupled to the app lifecycle and can invoke commands (on worker threads) *during* `setup()` — there's no natural point to "start the frontend after the backend is ready." The correct pattern is an explicit ready-handshake: init state as early as possible, then have the frontend re-read the authoritative value after a point where the backend has demonstrably answered. (A stricter fully-deterministic option: a `tokio::sync::Notify` set at end of `setup()` + a `wait_until_ready` command the frontend awaits — more machinery, not needed here.)

**Debugging gotchas (cost hours):**
- **Multi-window log interleaving:** all four windows (main/about/intensity/history) run `App.svelte` and log to the SAME `widget.jsonl`. A temporary `measure_debug` log looked like the main window failing, but the `config:false`/`mode:none`/`intensityMode:true` lines were the *secondary* windows correctly early-returning. Filter by `!aboutMode && !intensityMode && !historyMode && config` to isolate the main window.
- **Validate via ground truth, not theory:** get the real window height with a Win32 `GetWindowRect` PowerShell probe (Add-Type; run with `-File`, NOT `-ExecutionPolicy Bypass` which the sandbox blocks) and correlate with `auto_resize measure`/`auto_resize::apply` in `widget.jsonl`. "No apply" ≠ "too tall" — a window already matching its content correctly skips the apply.
- **A 0-session test artifact:** rapid restarts with no agents reporting can show the SetupPanel (onboarding) via a separate `has_history`/setup_state race, which sizes the window tall to fit the onboarding copy — not the session-list too-tall the user reported. Normal use has `sessions>0` so `hookEverReceived` is true and SetupPanel stays hidden. Related: [[debug_state_transitions_via_widget_jsonl]], [[debug_auto_resize_dpi_drift]].
