---
created: 2026-11-01 10:54:10
---

# Add cargo clippy to the commit gate after triaging its 59-error baseline

cargo clippy --all-targets -- -D warnings fails with 59 errors (seen 2026-11-01, e.g. redundant_closure), all in code untouched by recent work, and .claude/commit-checks.sh does not run clippy, so the count only grows. Next step: run clippy, triage the baseline per lint (fix, or allow in a documented config with the reason), then add the clippy step to commit-checks.sh and build.yml together, since that file requires the two to stay in lockstep. Same shape as the rustfmt gate memo.

**Partly done 2026-11-27 (Windows).** The Windows baseline is cleared: 48 findings fixed in the code, `too_many_arguments` (9 sites) turned off under `[lints.clippy]` in `src-tauri/Cargo.toml` with its reason, and two site `#[allow]`s with comments (`large_enum_variant` on `AdapterOutput`, `enum_variant_names` on `subagent_gate::Unmatched`). `cargo clippy --all-targets -- -D warnings` now runs in `.claude/commit-checks.sh` and in `build.yml` on Windows only (`case "$OSTYPE"` there, `if: runner.os == 'Windows'` in the workflow, `components: clippy` on the toolchain step). `cargo clippy --fix` could not be used: its rewrite of a collapsible `if` produced code that did not compile, so every fix was made by hand.

What is left needs the Mac: run the same clippy command there, fix or document what the macOS-only code reports (the agterm adapter, its `write_title` included, `agterm_facts`, `lid_awake`, `idle_awake`, the `#[cfg(target_os = "macos")]` arms of `auto_resize` and `tray`), then drop both Windows conditions together. No macOS run has been made, so its count is unknown.
