---
created: 2026-09-24 18:45:59
platform: windows
---

# Attribute a tmux-hosted session's Windows Terminal window in attached_surface, or document that it cannot

Under WSL tmux, claude.exe's console is a headless conhost under wslhost.exe. GetConsoleWindow returns an invisible PseudoConsoleWindow with no owner, so WindowsAdapter::attached_surface returns None on every write: every 'terminal title written' line in widget.jsonl since 2026-09-23 logs host="". TerminalTitles::hosts therefore stays empty. observe_caption's eligible closure and TerminalTitles::row_for_title then treat every row as eligible in every window, which loses the protection against a lookalike title in a second Windows Terminal window. That is moot while one window holds every tab.

A lead: Windows Terminal adds WT_SESSION to WSLENV, and each tmux client's /proc/<client_pid>/environ carries a distinct WT_SESSION GUID. That maps cc-<project> to its client and on to a WT tab. Mapping the GUID to an HWND on the Windows side is unsolved. A title join cannot run inside push_title, where attached_surface is called: the new title has not reached Windows Terminal yet (10-47 ms measured).

Either way, the doc comments on attached_surface, push_title (windows arm) and read_title still describe a console owned by Windows Terminal, which is false for tmux-hosted sessions.
