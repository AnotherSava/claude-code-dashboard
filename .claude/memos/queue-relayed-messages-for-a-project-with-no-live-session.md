---
created: 2026-09-29 21:28:16
---

# Queue relayed messages for a project with no live session and deliver them when one starts

Proposed 2026-09-29 by the dotfiles session on CHROME (its new client, claude/skills/shared/peer_relay.py, sends /commit's post-push "run /pull" requests through POST /api/message). Narrower than it looks: a message to a project with nothing running now gets a session started for it (session_launcher::start_and_wait, relay hop and LocalIdle alike), with no grant or listing since 28f3966, and on Windows through the WSL tmux holder (remote_session_script) since e8208dc. The loss is for a start that is refused or fails: start_untrusted_directory, start_ambiguous_project, start_launch_failed, start_not_realized, start_no_launcher (a Windows machine with no remote_session_script set), and unknown_project, where there is nothing to queue for.

Open questions:
- A distinct receipt Outcome (e.g. queued) so a sender never reads a queued message as written.
- Persistence across dashboard restarts and deploys (own file in the app data dir, like custom_names.json, since deploy overwrites config.json).
- Which dashboard holds the queue: the sender's or the recipient's.
- Scope: cross-machine only, or local too (see the local-routing memo).
