---
name: debug_tab_on_message_e2e
description: How to test the tab-on-message offer (tab_offer) end to end across the Windows and Mac pair
metadata:
  type: project
---

Testing `sync::offer_tab` needs a WSL `cc-*` holder session with no client from another machine, and in normal use every one has a Mac `⇄` client. List the clients and their recorded origin with `tmux list-clients` plus each client's `REMOTE_SESSION_ORIGIN` (tmux_wire's `LIST_SCRIPT` reads the same thing), then close the Mac's `⇄` tab on a session whose only client is `mac` (client only, not the session).

A successful offer makes the Mac attach again, so the relay case and the local `SendMessage` case each need their own close: the Windows side checks the target before asking any peer and logs `watched_elsewhere`, so the second case never reaches the Mac as `already_attached`.

`sent`/`refused`/`failed`/`no_peer` log at INFO; `not_hosted`, `watched_elsewhere` and `sender_not_watched_elsewhere` are DEBUG lines. First verified on Windows 2026-11-22: a relay reached `outcome=sent` 0.4s after `peer_write`, and a local send 1.4s after `prompt_origin` (two `wsl.exe` lookups). The Mac logs `peer_attach` on its side.

Related: [[debug_sync_fake_peer]], [[verify_peer_message_delivery]].
