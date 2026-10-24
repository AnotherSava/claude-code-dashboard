---
created: 2026-10-08 03:50:39
---

# Report a relayed grant filed against the wrong directory as a path mismatch, not as unlisted

On the relay route `sync::post_message` computes `let listed = session_launcher::listed_dir(..).is_ok()` (sync.rs, just above the candidate-offer block). `listed_dir` fails two ways — `NotListed` when `auto_start.json` has no entry, and `PathMismatch` when it has one whose directory re-derives a different project id — and `.is_ok()` collapses both into `!listed`.

So a project that IS listed, against a mistyped directory, takes the offer arm and gets a receipt whose detail reads "nothing is running for that project and it is not listed as startable here; its owner can approve one of the offered directories". The first clause about listing is false for that project, which is the class CLAUDE.md spends several paragraphs on: reporting one fact for another is what made a mistyped address read as a live agent having gone away, and what `StartRefusal::NoSuchProject` was split out of `NotListed` to stop. `start_path_mismatch` is consequently unreachable on this route, while remaining reachable on the local route through `check_startable`.

Severity is low and that is why this is parked rather than fixed: the candidates offered alongside the wrong-wording refusal include the correct directory, so approving one re-grants the project properly and repairs the bad entry. Only the sentence misleads, and it misleads about something the very same response fixes.

The change is to make `listed` carry the refusal rather than a bool and let a `PathMismatch` answer with its own slug and detail. The design question to settle first is whether it should still offer candidates when it does — a mismatched entry is a repairable mistake and the offer is what repairs it, so answering `start_path_mismatch` with an empty candidate list would be a regression. Probably: keep the offer, correct the detail.

Found by the adversarial verify pass of the start/refuse mapping workflow on 2026-10-08, alongside the `post_grant` `granted: true` defect (fixed) and the spurious no-launcher offer (fixed).
