---
name: hooks_research_findings
description: Claude Code hook research (2.1.195) — what we adopted for state derivation and which hook-based ideas were investigated and rejected
metadata:
  type: project
---

Live research (2026-06) on Claude Code hooks for the dashboard's state derivation, validated against the installed binary's Zod schemas (the authoritative source for payload shapes — the public docs don't render the `Stop` JSON example).

**Adopted** (the `Stop` payload superseded transcript workarounds — verified field shapes from the 2.1.195 binary):
- `Stop.last_assistant_message` (string, since 2.1.47) — "Text content of the last assistant message before stopping. Avoids the need to read and parse the transcript file." The adapter's `classify_stop` runs `question_reason` on it directly, so the old too-early-`Stop` correction machinery (`flushed_turn_verdict` + `promote_done_to_blocked`/`demote_scanned_blocked_to_done` + the `status_from_transcript_scan` flag) was deleted.
- `Stop.background_tasks` (optional array of `{id,type,status,description}`, since 2.1.145) — "Empty array when nothing is in flight." Non-empty ⇒ the `Waiting` state, set at `Stop` time; replaced the `pendingBackgroundAgentCount` transcript scrape. `infer_state` now skips sidechain entries for state so background work can't flip a `Waiting` row to `Working`.

**Rejected — do not re-propose** (each investigated and ruled out):
- `Notification(idle_prompt)` is now **ignored entirely** (the adapter's Notification arm returns `None` for it). It's a flaky ~60s fixed timer — not state-aware, fires after every response, sometimes never, **never for AskUserQuestion** — so it can't replace the Windows `idle_probe` screen-scrape, and once `Stop` became authoritative (last_assistant_message) its transcript re-scan was pure redundancy (removed `classify_turn_end` + `last_assistant_text`). Keep `idle_probe`. See [[terminal_promote_to_working_unsafe]], [[idle_probe_screen_criteria_tui_sensitive]].
- `PostToolUse`/`PostToolBatch` as a "turn alive" heartbeat — reintegrates the per-Bash/Read/Grep fork+POST overhead the `PreToolUse` matcher-gate avoids, and can't cover the start-of-turn / text-only windows `WORKING_GRACE_MS` + spinner detection handle.
- `MessageDisplay` as busy corroboration — redundant with `WORKING_GRACE_MS`, display-only with a 10s timeout that blocks TUI rendering, and doesn't fire for tool-first turns.
- **No hook payload carries token/context-usage data** (the `effort` field is a level, not tokens) — context-usage % must keep coming from the transcript + OAuth poller.

**Policy:** target only the latest Claude Code; no fallbacks for older versions (now in CLAUDE.md `## Conventions`). [[feedback_favor_clean_design]]
