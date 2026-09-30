---
created: 2026-09-29 21:28:16
---

# Queue relayed messages for a project with no live session and deliver them when one starts

Proposed 2026-09-29 by the dotfiles session on CHROME (its new client, claude/skills/shared/peer_relay.py, sends /commit's post-push "run /pull" requests through POST /api/message). Today a message to a project with nothing running is refused (no_such_session / start_not_listed / unknown_project) and lost; docs/pages/settings.md says "nothing is queued" for the start-approval timeout.

Narrower than it looks: a project listed in auto_start.json already gets a session started (session_launcher::start_and_wait, relay hop and LocalIdle alike). The loss is for unlisted projects, and for every target on Windows, which has no launcher (start_no_launcher).

Open questions:
- A distinct receipt Outcome (e.g. queued) so a sender never reads a queued message as written.
- Persistence across dashboard restarts and deploys (own file in the app data dir, like auto_start.json, since deploy overwrites config.json).
- Which dashboard holds the queue: the sender's or the recipient's.
- Scope: cross-machine only, or local too (see the local-routing memo).
