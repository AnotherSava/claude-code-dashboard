---
name: windows_terminal_attention_signals
description: Windows attention/restore was investigated and built on 2026-09-02; the Win32 mechanics moved to global learnings, this holds what is true of this repo and this machine
metadata:
  type: project
---

`terminals::windows` (attention plus `sessions()` for [[startup_session_restore]]) was investigated and built on 2026-09-02. Feasibility is settled, so do not re-open it: all three signals work and the design, its gaps and its rejected alternatives are written up in the `terminals::windows` paragraph of CLAUDE.md.

**The Win32 mechanics are global, not project knowledge, and deliberately live outside this repo.** `~/.claude/learnings/windows-terminal-title.md` holds the console title read-back (per session, not per screen) and what Windows Terminal exposes about which tab is on screen; `~/.claude/learnings/windows-winevent-hooks.md` holds the `SetWinEventHook` technique with the measured pid-scoping numbers. Read those before touching the adapter, and do not copy their content back here.

One consequence found in production on 2026-09-03 and now handled by `terminal_title::observe_caption`: a Windows Terminal tab the user renames stops following the title for good, so a row can read `Working` while its tab reads `🟢`. `~/.claude/learnings/windows-terminal-title.md` has the mechanism, the measurements and every dead route to resetting it from outside; the detector's own design is in CLAUDE.md.

What is specific to this setup:

- **This machine's WT `settings.json` pins no titles.** No `suppressApplicationTitle`, no `tabTitle`, no per-profile `title`. Any of them would stop the window caption following the session and would silently kill departure detection, so it is the first thing to check if the adapter goes quiet.
- **All seven live sessions run as tabs of a single Windows Terminal window**, each `claude.exe` under `cmd.exe` under `pwsh.exe`. There is one WT process, so windows are keyed by HWND throughout.
- **Real visits here are short.** Two measured tab visits of 1.7s and 2.2s inside one 12 minute sample, which is why the watch is the primary source and the 30s poll is only a safety net. Same conclusion as [[attention_visit_detection_design]] reached for agterm, arrived at independently.
- **`projects_root` is unset**, so a row id is the bare directory basename and two projects named alike collapse into one row. That is one of the two things `AgentSession::name_shared_by` warns about.

See also [[agterm_status_coexistence]] for the macOS half and [[debug_console_titles_tool_consoles]] for why console side effects cannot be verified from the Bash tool.
