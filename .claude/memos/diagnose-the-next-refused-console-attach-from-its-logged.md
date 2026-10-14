---
created: 2026-10-13 20:12:52
platform: windows
---

# Diagnose the next refused console attach from its logged error code, and confirm the title retry recovers it

On 2026-10-14 01:03Z the cc-claude remote-session tab (claude.exe pid 17088, started by the WSL holder alongside cc-toolbox) kept the console's default title for over an hour: every title-write candidate refused AttachConsole and terminal_title::sync failed silently until an unrelated event retitled it at 02:17:59Z, so the refusal was transient. The cause was never established and not reproduced (reproducing means restarting the holder's live sessions). Since then terminal_title.rs logs, at debug, 'console attach refused; title not written' per candidate with chat_id, pid and the Win32 error, and retries failed writes on a 1s..60s doubling backoff for up to 10 attempts ('retrying failed terminal title writes'; a warn 'terminal title write kept failing' when it gives up). Next step: grep widget.jsonl for 'console attach refused', look up the error code (e.g. 5 access denied, 6 invalid handle, 31 process has no console) to identify the cause, and check that a retry pass followed and a 'terminal title written' ok=true line for that chat_id landed within the backoff window.
