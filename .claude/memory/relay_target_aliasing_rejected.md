---
name: relay_target_aliasing_rejected
description: A display-name fallback for relay targets was built, verified and withdrawn 2026-10-06; two traps for anyone re-attempting target aliasing
metadata:
  type: project
---

On 2026-10-06 a display-name fallback for relay targets was built: the receiver resolved an id no directory derives through `CustomNamesStore`, and the sender retried once on `unknown_project` under its own display name for the target. It was deployed and verified live CHROME → AIR, then reverted without being committed. It was withdrawn because the user chose to rename clone folders to match their GitHub repos instead ([[feedback_clone_folder_matches_repo]]).

Two traps the adversarial review found, for anyone re-attempting target aliasing:

- `sync::send_message_hop` restamps `receipt.target` with the caller's address, so an id the receiver resolved never reaches the sender. The start-approval flow then files its grant under the alias, and `post_grant` refuses it as `start_path_mismatch`.
- Nothing proves an alias names the same repository, so an unrelated folder on the peer carrying the alias receives the message.

The origin-URL alternative (its memo closed 2026-10-07) was never built.
