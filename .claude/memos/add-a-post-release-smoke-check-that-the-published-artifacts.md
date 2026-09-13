---
created: 2026-09-03 04:07:00
---

# Add a post-release smoke check that the published artifacts actually install, for every release and both platforms.

Today nothing does: a release is declared done when the CI run is green and the GitHub Release page has its assets, and neither fact touches the installers. That is the general form of the Homebrew tap failure (separate memo, 2026-09-03 03:02) rather than the same bug: that one is a specific broken cask plus a workflow that asserted nothing, and gets checked off when it is fixed; this is the standing check whose absence let it run for two releases. Shape to consider: a job after the release publishes, or a healthchecks.io-pinged scheduled run, that on macOS does `brew update && brew install --cask AnotherSava/tap/claude-code-dashboard` from a clean runner and asserts the app bundle exists with the expected CFBundleShortVersionString, and on Windows downloads the release .exe and asserts it is a valid NSIS installer of the expected version (a silent install plus a version probe if a runner allows it). Assert identity, not liveness: the check must confirm the version it just installed equals the tag, since "something installed" is exactly what a stale cask would also report. See ~/.claude/skills/heartbeat for the dead-man's-switch half if it becomes a scheduled job.
