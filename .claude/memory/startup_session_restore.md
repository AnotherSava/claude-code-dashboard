---
name: startup_session_restore
description: Startup row restore — rejected 2026-06-21 for want of a liveness signal, built 2026-09-02 once two independent sources could vouch
metadata:
  type: project
---

Restoring the live session list on dashboard startup (so a cold start / deploy isn't blank) was investigated 2026-06-21 and **rejected**, then **built 2026-09-02** (`session_restore.rs`) once the thing it was missing existed. Both halves are kept here: the rejection's reasoning is what the eventual design had to satisfy, and the probes it ran are still the right ones to re-run before trusting any *new* source.

**Why it was rejected:** there was no reliable way to tell a *closed* session from a *still-open-but-idle* one.
- Transcripts (`~/.claude/projects/*/*.jsonl`) carry **no session-end marker** — a session closed 20h+ ago ends with the same record kinds (`turn_duration`, `mode`, `file-history-snapshot`) as a live one, so content can't say "closed".
- mtime only gives "recently active", not "alive" — it includes a just-closed session and misses an idle-open one.
- The one clean cross-platform liveness signal — "is a process holding the transcript open" — **fails**: verified via Win32 RestartManager against two live sessions, neither transcript was held open. Claude opens-appends-closes per write, so between writes (exactly when a session is idle) there's no handle.
- Process→cwd mapping is reliable on macOS (`lsof`) but not Windows → fails the cross-platform bar.

**What changed.** 2026-08-30: Claude Code writes exactly the pid file this anticipated — `<claude-config>/sessions/<pid>.json`, one per live session, with liveness answerable by confirming the pid is still a Claude Code image. `session_registry.rs` read it, and `/api/agents` unioned it into the roster. That closed *existence* but not the rest, and the memory correctly held the line for another two months: a registry entry carries `idle`/`busy` and nothing else, so a row built from it alone would have asserted a status it did not have. "The roster shows it" and "the dashboard is tracking it" stayed different claims.

**What closed the gap** was noticing the status had been persisted all along, in the terminal tab title the dashboard itself writes — six distinct glyphs, and written *after* `apply_read_as_idle`, so `🟢` vs `⚪` even round-trips read-vs-unread, which is why `attended_at` needed no persisting. Two sources, each asked only what it is authoritative for: the registry for *is an agent alive here* (a tab outlives the agent in it), the tab for *what was it doing and had you read it* (`Activity` can express none of it). A tab with no title the dashboard recognizes is skipped — which is also the anti-resurrection guard, since removal blanks the tab.

**How to apply.** The bar for a *third* source is the same one this cleared: name which oracle establishes each claim, and refuse rather than guess when one cannot. Two traps found by adversarial review and worth re-checking on any change here — a restored row is created **outside every mechanism that exists to remove one** (`AgentPids`, `ChatIdRegistry::owners`, `waiting_backstop_armed` are all in-memory and hook-populated), and `state_entered_at` has two consumers that want opposite answers (`/api/agents`' age wants the true instant, the notifier's time-in-state wants not to fire on it) — resolved by backdating and gating the notifier on `state_observed_here`, not by faking the age.

Remote sessions still need none of this — they self-restore via sync peer pushes + 90s TTL regardless of how long the dashboard was down. Windows has no terminal adapter, so it still starts empty; that is the accepted gap, not a fallback to guessing. The empirical "probe before declaring infeasible" check here matched [[feedback_check_remote_before_fixing]] discipline; see also [[terminal_promote_to_working_unsafe]] (a genuinely rejected liveness approach — screen scraping — that stays rejected) and [[agterm_status_coexistence]].
