---
name: homebrew-tap-distribution
description: macOS ships via AnotherSava/homebrew-tap alongside the DMG; the cask's zap path is untested and can disable sleep
metadata:
  type: project
---

macOS has two install routes as of 2026-08-21: the DMG from Releases, and `brew install --cask AnotherSava/tap/claude-code-dashboard` from the separate `AnotherSava/homebrew-tap` repo. The official homebrew-cask tap is permanently out of reach — see [[homebrew-cask-unsigned-macos-app]] in global learnings for the general rules; the repo-specific consequences are below.

**The cask deliberately bypasses Gatekeeper.** Because the build is ad-hoc signed (`bundle.macOS.signingIdentity = "-"`), a `postflight_steps` block runs `xattr -dr com.apple.quarantine` on the installed bundle. Without it a `brew install` reproduces the same "damaged and can't be opened" wall as the DMG and buys the user nothing. Delete that block — and the workaround paragraph in `docs/pages/install.md` — if the app is ever notarized.

**The cask launches the widget itself** (a second `postflight_steps` `run` calling `/usr/bin/open`, after the quarantine strip, `must_succeed: false`). This is deliberate: with no Dock icon, an install that ended without starting anything left nothing on screen, and the alternative — explaining how to launch it — had to sit under a heading Homebrew hardcodes as "Caveats". Verified working on 2026-08-22; `open` is permitted inside the cask sandbox.

**Plain `brew uninstall --cask` was exercised on 2026-08-22 and is clean** — it quits the app, removes the bundle (backing it up to the Caskroom first), and leaves `SleepDisabled` alone. One side effect worth knowing: the `uninstall launchctl:` stanza *deletes* `~/Library/LaunchAgents/Claude Code Dashboard.plist` rather than just unloading it, and `lib.rs` only calls `autolaunch().enable()` when `is_first_run`. Reinstall and upgrade run the uninstall phase first, so *every* `brew upgrade --cask` deletes it too — not just an uninstall.

**This was fixed on 2026-08-23 in the app, not the cask** (the cask is right to remove the LaunchAgent; one pointing at a deleted binary would fail at every login). `config.autostart: Option<bool>` now records the intent and `lib.rs` re-creates a missing OS entry at startup when it is `true`. The repair is one-directional by design — see [[tauri-macos-native]] in global learnings for why removing an entry would fight System Settings → Login Items. `tray.rs::select_autostart_mode` must keep writing both the OS entry and the config field, or an "Off" chosen there is undone on the next launch.

**The zap path has never been executed.** It could not be tested on a working machine: `uninstall quit:`/`signal: KILL` target the bundle id of the live tray widget, and the zap runs `pmset -a disablesleep 0` against a machine that may have the lid-awake veto armed. The zap clears that flag in `script:` *before* `delete:` removes `/etc/sudoers.d/claude-code-dashboard-lidawake` and the sleepreset LaunchDaemon, because those two files are what every disablesleep recovery path depends on and the flag survives reboot. Get the ordering wrong, or run a partial teardown, and the Mac can never sleep again. Test on a VM or spare machine before recommending `brew uninstall --cask --zap` anywhere in the docs. `docs/pages/settings.md` still documents the manual teardown as the only route, deliberately.

**Never run `brew audit --new` against the tap.** That flag implies both the notability check (this repo is far under the bar) and the Gatekeeper signing check (ad-hoc, fails by design). The tap's own CI calls `brew style --cask` and `brew audit --cask --strict --online` explicitly instead of `brew test-bot`, which would append `--new` itself.

**Bump chain:** publishing a release fires `.github/workflows/notify-tap.yml` → `repository_dispatch` → the tap's `bump-cask.yml`, which resolves the DMG digest from the release API and commits via the Contents API. It needs the `TAP_DISPATCH_TOKEN` secret (fine-grained PAT, Contents write on the tap) because `GITHUB_TOKEN` cannot reach another repo. Both workflows accept a manual `workflow_dispatch` for backfills, and re-dispatching an existing version is a safe no-op. The whole chain was exercised end-to-end on 2026-08-21 against v1.8.0 — token authenticated, tap resolved the digest, diffed, and correctly declined to commit — so it does not need re-verifying; the first real release is what proves the `release: published` trigger specifically.

Related: [[macos_signing_strategy]], [[project_config_wiped_on_deploy]]
