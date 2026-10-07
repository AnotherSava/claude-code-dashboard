---
created: 2026-10-06 20:49:21
---

# Resolve a relay target by the repo it is, not by the sending clone's folder name

Measured 2026-10-06 by the jsonl-logs-intellij-plugin session on AIR, after a push of three commits.
That repo is cloned under two different folder names — jsonl-logs-intellij-plugin under AIR's projects
root, intellij-jsonl-extension under CHROME's — with one origin between them,
git@github.com:AnotherSava/jsonl-logs-intellij-plugin.git. One repo, two local names.

THE FAILURE: /commit step 9 ran `notify_peer_pull.py <sha> commit` and got

    CHROME/jsonl-logs-intellij-plugin: refused — unknown_project — no existing directory on this
    machine derives that project id, so this is an address that names nothing here rather than a
    session that ended

The receipt is honest — the unknown_project split from no_such_session is working exactly as the
reply-address memo's fix (3) intended, and it is what made the cause findable at all. The problem is
upstream of the receipt: the target project id is derived from the SENDING clone's folder name, so for
any repo whose two clones are named differently the post-push pull request cannot succeed, however
correct the answer is. The peer clone sat two commits behind with nothing telling it.

WHAT WORKED: `peer_relay.py send --project intellij-jsonl-extension` resolved (start_not_listed first,
because nothing was running there; written on retry once the start was approved), and that clone then
pulled to the pushed head. So the address space is fine and only the derivation is wrong.
`notify_peer_pull.py` offers no override — its usage is `notify_peer_pull.py <upstream-sha-before-push |
none> <skill-name>` — so /commit cannot route around it, and the wrapper is in the dotfiles repo
(claude/skills/shared/) rather than here.

TWO FIXES, and the first is why this memo is filed here rather than there:

1. Resolve a target by something both clones share rather than by folder name. The origin URL is the
   obvious candidate: it is identical in both clones and it is what actually identifies the repo. This
   is the dashboard's side of the line, since the dashboard is what maps a project id to a directory in
   its session registry. It fixes the class — every differently-named clone pair, on either machine.
2. Give notify_peer_pull.py a --project override. This only relocates the knowledge into each repo's
   memory, where it has to be written once per repo and goes stale on a rename. Worth having as a
   stopgap, not as the answer.

NOT the same as two memos already open here. Not the unroutable reply address and roster id shape
(that one is about from_agent and what a session calls itself; this is about naming the target). Not
queuing for a project with no live session (that one is about delivery when nothing is running; this
address names nothing even when a session IS running there). Both were read before filing.

WORKAROUND IN USE: jsonl-logs-intellij-plugin now carries a project memory naming the explicit
--project route for its own pushes, and says a rename through /move-project on either side would remove
the need. That memory is the thing fix 1 would make unnecessary.
