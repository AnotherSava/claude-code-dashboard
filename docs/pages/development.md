---
layout: default
title: Development
nav_order: 2
has_children: true
---

## Setup

### Prerequisites

- **Rust** 1.70+ (`rustup default stable-msvc` on Windows; `rustup default stable` on macOS).
- **Node.js** 20+ and **npm** 10+ (CI uses Node 24).
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

## Architecture

The app pairs a Rust backend (Tauri v2) with a Svelte 5 + Vite frontend rendered in the system webview (WebView2 on Windows, WKWebView on macOS). The Rust side owns all state and external I/O; the frontend is a pure view that subscribes to Tauri events and issues invoke-style commands for window control. External tools integrate via an embedded `axum` HTTP server on `127.0.0.1:9077`, bypassing the frontend entirely.

The source-of-truth `AgentSession` state lives behind a `Mutex` in Rust. Three paths mutate it — the HTTP server, the per-session transcript watcher, and Tauri commands invoked from the Svelte UI — and every mutation funnels through `state::apply_set` or `state::apply_clear` so the sticky-label state machine is enforced in exactly one place.

## Project structure

Under the repo root `claude-code-dashboard/`:

- `src/` — Svelte frontend (Vite)
  - `App.svelte` — top-level layout, subscribes to Tauri events
  - `HistoryApp.svelte` — root component of the history window
  - `AboutApp.svelte` — root component of the About window (Help → About)
  - `main.ts` — mount entry point
  - `lib/`
    - `types.ts` — shared TS types and display helpers
    - `mockSessions.ts` — dev-only fixtures (unused in release)
    - `api.ts` — invoke / listen wrappers
    - `components/`
      - `SessionList.svelte` — list container, empty-state
      - `SessionItem.svelte` — per-row rendering (status badge, timer, tokens, label)
      - `SetupPanel.svelte` — onboarding panel: bundled hook snippet, copy-to-clipboard, hide affordance
      - `LimitBar.svelte` — header 5h / 7d usage bar (segmented fill, percent + timer caps)
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
    - `http_server.rs` — axum routes for POST /api/event
    - `sync.rs` — multi-device session sync: bearer-gated listener + chunked delta push
    - `log_watcher.rs` — per-session transcript tailing + infer_state + assistant text upsert
    - `tray.rs` — TrayIconBuilder, menu handlers, autostart
    - `notifications.rs` — 1s-tick reconciler + Notifier trait
    - `telegram.rs` — reqwest-based Telegram Bot API client
    - `usage_limits.rs` — Anthropic OAuth usage poller + refresh (5h / 7d buckets)
    - `usage_history.rs` — appends each successful usage poll to `usage_history.jsonl`
    - `prompt_history.rs` — per-session dialog persistence to `prompt_history.json`
    - `remote_history.rs` — per-device remote-session dialog persistence under `remote_history/`
    - `chat_id_registry.rs` — persisted `session_id → chat_id` lock in `session_chat_ids.json`
    - `custom_names.rs` — user-assigned display names persisted to `custom_names.json`
    - `terminal_title.rs` — mirrors session status onto terminal tab titles
    - `auto_resize.rs` — Up/Down content-fit window + Win32 resize lock + dark class brush
    - `label_policy.rs` — shared (label, original_prompt) decision used by adapters
    - `adapters.rs` — adapter dispatch for /api/event payloads
    - `adapters/claude.rs` — Claude Code lifecycle classifier + chat-id derivation
    - `logging.rs` — tracing subscriber → widget.jsonl + FrontendLogger for IPC log lines
- `integrations/claude_hook.py` — thin Claude Code hook that forwards the stdin payload to /api/event
- `docs/` — this site
- `.github/workflows/`
  - `build.yml` — CI: check + cargo test + frontend build on push/PR (Windows + macOS matrix)
  - `release.yml` — CI: build NSIS + DMG installers on tag push (Windows + macOS matrix)

### Where state lives at runtime

- **In-memory** — `AppState` (sessions) and `ConfigState` (config) via `tauri::State`.
- **On disk** — `config.json`, `widget.jsonl`, `prompt_history.json`, `session_chat_ids.json`, `custom_names.json`, `usage_history.jsonl`, and the `remote_history/` directory under `app_data_dir()`:
  - Windows: `%APPDATA%\com.anothersava.claude-code-dashboard\`
  - macOS: `~/Library/Application Support/com.anothersava.claude-code-dashboard/`

## Architecture reference

- [Classification](development/classification) — how the Claude adapter turns a raw lifecycle payload into the `(chat_id, status, label)` tuple the widget renders.
- [Sticky labels](development/sticky-labels) — the state machine that keeps a meaningful caption next to a session row across approval cycles, cancellations, and continuation prompts.
- [Data flow](development/data-flow) — end-to-end paths from a Python hook POST or a transcript file change to a rendered pixel.
- [HTTP API](development/http-api) — `POST /api/event` envelope shape and how to write a new adapter for a non-Claude agent.

## Testing

Rust tests live inline in `#[cfg(test)]` modules next to the code they cover:

- `state::tests` — sticky-label machine, working-time accumulator, error transitions.
- `label_policy::tests` — the `(label, original_prompt)` decision extracted from `apply_set`.
- `log_watcher::tests` — the transcript parser (`infer_state`, `split_complete`), the upgrade-only merge policy, and the `flushed_turn_verdict` question corrections (`done → awaiting` and `awaiting → done`).
- `sync::tests` — the receive-side `ingest` (namespacing, dialog seeding, contiguity guard) and the oldest-first chunked `build_push_chunk`.
- `adapters::claude::tests` — `classify`, `derive_chat_id`, `clean_prompt`, `last_assistant_text`, `is_a_question` / `question_reason` / `evidence_snippet`, and the outer `dispatch`.

CI runs Rust tests on every push and PR (`build.yml`) and again before bundling on every tag push (`release.yml`), so a broken state machine can't ship a release.
