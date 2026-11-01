---
created: 2026-11-01 10:54:10
---

# Add cargo clippy to the commit gate after triaging its 59-error baseline

cargo clippy --all-targets -- -D warnings fails with 59 errors (seen 2026-11-01, e.g. redundant_closure), all in code untouched by recent work, and .claude/commit-checks.sh does not run clippy, so the count only grows. Next step: run clippy, triage the baseline per lint (fix, or allow in a documented config with the reason), then add the clippy step to commit-checks.sh and build.yml together, since that file requires the two to stay in lockstep. Same shape as the rustfmt gate memo.
