---
created: 2026-10-15 21:24:06
---

# Confirm the first real Windows idle close from widget.jsonl, and that a refused close logs once and stays put

Windows idle auto-close (agwinterm, commit 7b387a6, deployed 2026-10-15 ~21:13 local) has only been exercised on throwaway probe sessions in C:\tmp. Not yet observed: a real CLEAN session closing on schedule after idle_close_ms (1h), or a refusal behaving as designed.

Check after the dashboard has been up over an hour with a CLEAN agwinterm session not on screen:
- grep widget.jsonl (in %APPDATA%\com.anothersava.claude-code-dashboard) for '"decision":"idle_close"'. Expect outcome=closed with idle_ms >= window_ms, then a reap_exited for that chat_id (Claude Code sends no SessionEnd on /exit), the tmux session gone (wsl -d Ubuntu -- tmux list-sessions), and the agwinterm tab gone.
- For a refusal (session on screen in agwinterm's front window, or a draft in its composer): expect ONE close_failed line carrying the reason, no repeat each minute, and the session left alone with nothing typed.
- Watch for the single-read recheck case review round 4 flagged (r1-1, declined as rare): '/exit' left typed in a composer, after which every tick refuses it as a draft. If it shows up, poll the post-typing composer read for a few seconds instead of reading it once.
