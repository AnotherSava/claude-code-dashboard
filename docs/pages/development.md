---
layout: default
title: Development
nav_order: 2
has_children: true
---

## Setup

### Prerequisites

- **Rust** 1.70+ (`rustup default stable-msvc` on Windows; `rustup default stable` on macOS).
- **Node.js** 24, as `.nvmrc` and `engines.node` both say. The range is enforced rather than advised — `.npmrc` sets `engine-strict=true`, so `npm install` on another major exits `EBADENGINE` instead of warning and carrying on.
- **npm** is pinned by `packageManager` in `package.json`. Run `corepack enable npm` and npm shims itself to that exact version; plain `corepack enable` covers yarn and pnpm only, so without the explicit form the pin is silently ignored. Run it again after every Node upgrade — the upgrade replaces the Node install and takes the shim with it, after which `npm -v` quietly answers with the bundled npm while `package.json` still reads correctly. Comparing `npm -v` against the field is the only check that sees it.
- **Platform toolchain**:
  - **Windows**: Microsoft C++ Build Tools (Visual Studio Installer → "Desktop development with C++") and WebView2 (preinstalled on Windows 10 1803+; the installer fetches it if missing on older machines).
  - **macOS**: Xcode Command Line Tools (`xcode-select --install`). WKWebView ships with the OS — nothing to install.

### Install

```bash
git clone git@github.com:AnotherSava/claude-code-dashboard.git
cd claude-code-dashboard
npm install
```

### Run from source

```bash
npm run tauri dev
```

Compiles the Rust backend, starts Vite on `localhost:1420`, and launches the native window. Frontend edits hot-reload; Rust edits trigger a rebuild on save.

## Commands

- `npm run tauri dev` — dev build with HMR.
- `npm run tauri build` — release build; bundles land in `src-tauri/target/release/bundle/` (`nsis/` on Windows, `dmg/` on macOS).
- `npm run check` — TypeScript + Svelte check (no build).
- `npm run tauri icon <path/to/1024.png>` — regenerate the Windows / macOS icon set from a source PNG.
- `cargo test --manifest-path src-tauri/Cargo.toml --lib` — Rust unit tests (state machine, transcript parser, merge policy, Claude adapter, label policy).
- `RUSTFLAGS="-D warnings" cargo check --manifest-path src-tauri/Cargo.toml --all-targets` — compile every target, in both configurations, with warnings as errors. The test run above sees only the test cfg, and `--lib --tests` never performs the bin's plain build, so this is the only command that catches an item in `src/main.rs` that is live under `cfg(test)` and dead without it.
- `bash .claude/commit-checks.sh` — everything CI will run, in CI's order, in under half a minute on a warm cargo cache, plus the checks that read files under `~/.claude/` and so cannot run on a runner at all: the conventions checker, and an assertion that the shared scripts the screenshot capture shells out to still resolve — by path, and on Windows also by the function and parameter names the captures call. It ends with a notice rather than a check: the wire-shape arm exits 0 whatever it finds, and warns when the change set adds a serialized field to `http_server.rs` or `sync.rs`, because `peer_relay.py`, the capture scripts and the other machine's dashboard go on reading the old shape until this machine deploys. Run it before committing.

The dev profile carries line tables rather than full DWARF (`[profile.dev]` in `src-tauri/Cargo.toml`, with dependencies at `debug = 0`), and that profile is what `cargo check`, `cargo test --lib` and rust-analyzer all build. A backtrace still names its files and lines; what goes is variable and type inspection in a debugger, which nothing here depends on. Full DWARF was 82.8% of the bytes of every object file in the graph and had taken `target/debug` to 38 GB.

## Architecture

The app pairs a Rust backend (Tauri v2) with a Svelte 5 + Vite frontend rendered in the system webview (WebView2 on Windows, WKWebView on macOS). The Rust side owns all state and external I/O; the frontend is a pure view that subscribes to Tauri events and issues invoke-style commands for window control. External tools integrate via an embedded `axum` HTTP server on `127.0.0.1:9077`, bypassing the frontend entirely.

The source-of-truth `AgentSession` state lives behind a `Mutex` in Rust. Hook events reach it through `state::apply_set`, which enforces the sticky-label rules, the working-time accumulator and the task boundary in one place whatever the origin. The other writers each own one narrow transition and never choose a label: the transcript watcher's promotion to working and its Esc-cancel revert, the WAIT backstop, the liveness reaper's row removal, a restart's restore, and a subagent's permission prompt. Sessions running on another device arrive pre-enriched over sync and live in a separate map; `commands::resolved_snapshot` is the one place the two sets combine.

## Project structure

Under the repo root `claude-code-dashboard/`:

- `src/` — Svelte frontend (Vite)
  - `App.svelte` — top-level layout, subscribes to Tauri events
  - `HistoryApp.svelte` — root component of the history window
  - `AboutApp.svelte` — root component of the About window (Help → About)
  - `IntensityApp.svelte` — root component of the Work intensity window
  - `main.ts` — mount entry point
  - `lib/`
    - `types.ts` — shared TS types and display helpers
    - `api.ts` — invoke / listen wrappers
    - `components/`
      - `SessionList.svelte` — list container, empty-state
      - `SessionItem.svelte` — per-row rendering (status badge, timer, tokens, label)
      - `SetupPanel.svelte` — onboarding panel: bundled hook snippet, copy-to-clipboard, hide affordance
      - `LimitBar.svelte` — header 5h / 7d usage bar (segmented fill, percent + timer caps)
      - `StartApprovals.svelte` — the prompt asking you to approve starting a session on another machine
- `src-tauri/`
  - `Cargo.toml` — Rust deps: tauri, axum, notify, tracing, serde, reqwest, chrono, open
  - `tauri.conf.json` — NSIS + DMG bundle targets, WebView2 bootstrapper, window config
  - `capabilities/default.json` — capability-based permissions for the main window
  - `src/`
    - `main.rs` — entry; calls lib::run()
    - `lib.rs` — Builder: plugins, state, commands, setup hook
    - `state.rs` — AgentSession struct, apply_set sticky-label machine
    - `config.rs` — Config struct, load/save, ConfigState wrapper
    - `config_watcher.rs` — notify watcher for config.json hot-reload
    - `commands.rs` — Tauri commands + event emitters
    - `setup.rs` — embedded Python hook + settings.json snippet builder for onboarding
    - `http_server.rs` — the loopback axum server: `/api/event`, `/api/agents`, `/api/message`, `/api/window`, `/api/session-clean`, `/api/project/rename`
    - `sync.rs` — multi-device session sync: source- and token-gated listener, metadata push, receiver-driven pulls
    - `tailnet.rs` — asking Tailscale which machine a connection came from, instead of trusting its envelope
    - `peer_message.rs` — relaying one message to an agent on another machine, and the honesty rules around it
    - `start_approval.rs` — asking this machine's user to approve starting a session on another one
    - `session_launcher.rs` — starting a terminal session for a project that has none
    - `auto_start_store.rs` — the `chat_id → absolute path` grants that permit those starts, in `auto_start.json`
    - `session_restore.rs` — giving a live session its row back after a restart
    - `project_rename.rs` — carrying a project's history, anchors, name and start grant over to the id its renamed folder derives, and announcing the rename to peers
    - `log_watcher.rs` — per-session transcript tailing + infer_state + assistant text upsert
    - `liveness.rs` — process-liveness primitives and the per-row owning-pid store
    - `liveness_reaper.rs` — removes a row whose Claude process exited without a `SessionEnd`
    - `waiting_settle.rs` — settles a `waiting` row to `done` once its killed background task can no longer report
    - `subagent_gate.rs` — the subagent permission-prompt overlay, and finding the gated call's result that releases it
    - `prompt_origin.rs` — what a row shows for a task another agent began
    - `nonce_store.rs` — per-session marker nonces for the instruction-adherence canary
    - `tray.rs` — TrayIconBuilder, menu handlers, autostart
    - `tray_badge.rs` — the usage badge drawn onto the tray icon, and the context-usage alert over it
    - `notifications.rs` — 1s-tick reconciler + Notifier trait
    - `telegram.rs` — reqwest-based Telegram Bot API client
    - `idle.rs` — system-wide input-idle milliseconds, for the AFK half of the notification rules
    - `idle_awake.rs` — holds off idle sleep while a local agent is working (macOS)
    - `lid_awake.rs` — holds off sleep with the lid closed, for a bounded window (macOS)
    - `usage_limits.rs` — Anthropic OAuth usage poller + refresh (5h / 7d buckets)
    - `usage_cache.rs` — the last good reading and the endpoint's retry deadline, in `usage_cache.json`
    - `usage_history.rs` — appends each successful usage poll to `usage_history.jsonl`
    - `token_history.rs` — one record per Claude API response in `token_history.jsonl`, and the Work intensity chart built from them
    - `token_scan.rs` — 60s scan of Claude Code's own transcripts, which is what fills that file
    - `remote_tokens.rs` — per-device remote token records under `remote_tokens/`
    - `remote_usage.rs` — per-device remote usage samples under `remote_usage/`
    - `prompt_history.rs` — per-session dialog persistence to `prompt_history.json`
    - `remote_history.rs` — per-device remote-session dialog persistence under `remote_history/`
    - `chat_id_registry.rs` — persisted `session_id → chat_id` lock in `session_chat_ids.json`
    - `custom_names.rs` — user-assigned display names persisted to `custom_names.json`
    - `terminal_title.rs` — mirrors session status onto terminal tab titles
    - `session_registry.rs` — finds a row's terminal in Claude Code's live session list, by cwd
    - `attention.rs` — which finished sessions you have actually looked at, and the stamp that dims them
    - `agterm.rs` — transport for agterm's `agtermctl` control socket (macOS)
    - `terminals/` — one adapter per terminal behind the `TerminalAdapter` seam, so nothing downstream names a terminal
      - `mod.rs` — the trait, the shared vocabulary, and the pure verdicts over it (`person_verdict`, `departure_stamp`, `read_front`)
      - `agterm.rs` / `agterm_facts.rs` — agterm's adapter, and the facts it reads from agterm and the system (macOS)
      - `agterm_wire.rs` — agterm's `session context` protocol: the argv a write takes, the targets its tree reports, and the release that first served the verb (macOS)
      - `agwinterm.rs` / `agwinterm_state.rs` / `agwinterm_wire.rs` — agwinterm's control pipe, its per-window state files, and its protocol (Windows)
      - `windows.rs` / `wt_tabs.rs` — Windows Terminal and the Windows console, and the UI Automation read of what a tab really holds
      - `composite.rs` — presents several terminals on one platform as one adapter
      - `labels.rs` — writes each row's task into a terminal's own per-session context line
      - `stale_check.rs` — catches a tab that has stopped following its session
      - `snapshot_watch.rs` / `window_files.rs` — the directory watch and per-window bookkeeping two adapters share
    - `auto_resize.rs` — Up/Down content-fit window + vertical resize lock (Win32 hit-test subclass / macOS height pin) + dark class brush
    - `label_policy.rs` — shared (label, original_prompt) decision used by adapters
    - `adapters.rs` — adapter dispatch for /api/event payloads
    - `adapters/claude.rs` — Claude Code lifecycle classifier + chat-id derivation
    - `logging.rs` — tracing subscriber → widget.jsonl + FrontendLogger for IPC log lines
- `integrations/claude_hook.py` — thin Claude Code hook that forwards the stdin payload to /api/event
- `docs/` — this site
- `.github/workflows/`
  - `build.yml` — CI: check + `cargo check --all-targets` with warnings denied + cargo test + frontend build on push/PR (Windows + macOS matrix)
  - `release.yml` — CI: build NSIS + DMG installers on tag push (Windows + macOS matrix)
  - `notify-tap.yml` — CI: on a published release, tells the `AnotherSava/homebrew-tap` repo to bump its cask to the new version

### Where state lives at runtime

- **In-memory** — `AppState` (local and remote sessions) and `ConfigState` (config) via `tauri::State`, alongside the other managed stores the frontend and the HTTP routes read.
- **On disk** — `config.json`, `widget.jsonl`, `prompt_history.json`, `session_chat_ids.json`, `custom_names.json`, `auto_start.json`, `project_renames.json`, `usage_history.jsonl`, `usage_cache.json`, `token_history.jsonl`, `token_scan_cursor.json`, and the `remote_history/`, `remote_usage/` and `remote_tokens/` directories under `app_data_dir()`:
  - Windows: `%APPDATA%\com.anothersava.claude-code-dashboard\`
  - macOS: `~/Library/Application Support/com.anothersava.claude-code-dashboard/`

## Architecture reference

- [Classification](development/classification) — how the Claude adapter turns a raw lifecycle payload into the `(chat_id, status, label)` tuple the widget renders.
- [Sticky labels](development/sticky-labels) — the state machine that keeps a meaningful caption next to a session row across approval cycles, cancellations, and continuation prompts.
- [Data flow](development/data-flow) — end-to-end paths from a Python hook POST or a transcript file change to a rendered pixel.
- [HTTP API](development/http-api) — `POST /api/event` envelope shape, how to write a new adapter for a non-Claude agent, and the read-only `GET /api/agents` roster of every tracked session across devices.

## Testing

Rust tests live inline in `#[cfg(test)]` modules next to the code they cover — most modules carry one. The ones worth knowing where to look for:

- `state::tests` — sticky-label machine, working-time accumulator, error transitions, the subagent-prompt overlay.
- `label_policy::tests` — the `(label, original_prompt)` decision extracted from `apply_set`.
- `log_watcher::tests` — the transcript parser (`infer_state`, `split_complete`), the promote-to-`working`-only merge policy, and the `[Request interrupted by user]` cancel marker.
- `sync::tests` — the receive-side `ingest` (namespacing, dialog seeding, the attended verdict), the pulled-dialog merge, and the source/token guard that fronts every sync route.
- `adapters::claude::tests` — `classify` / `classify_stop`, `derive_chat_id`, `clean_prompt`, `is_a_question` / `question_reason` / `evidence_snippet`, and the outer `dispatch`.
- `terminals::*::tests` — the verdicts over the adapter seam's vocabulary, which are pure and so are tested once for every terminal rather than per platform.

CI runs Rust tests on every push and PR (`build.yml`) and again before bundling on every tag push (`release.yml`), so a broken state machine can't ship a release.
