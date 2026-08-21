---
name: homebrew-tap-distribution
description: macOS ships via AnotherSava/homebrew-tap alongside the DMG; the cask's zap path is untested and can disable sleep
metadata:
  type: project
---

macOS has two install routes as of 2026-08-21: the DMG from Releases, and `brew install --cask AnotherSava/tap/claude-code-dashboard` from the separate `AnotherSava/homebrew-tap` repo. The official homebrew-cask tap is permanently out of reach — see [[homebrew-cask-unsigned-macos-app]] in global learnings for the general rules; the repo-specific consequences are below.

**The cask deliberately bypasses Gatekeeper.** Because the build is ad-hoc signed (`bundle.macOS.signingIdentity = "-"`), a `postflight_steps` block runs `xattr -dr com.apple.quarantine` on the installed bundle. Without it a `brew install` reproduces the same "damaged and can't be opened" wall as the DMG and buys the user nothing. Delete that block — and the workaround paragraph in `docs/pages/install.md` — if the app is ever notarized.

**The zap path has never been executed.** It could not be tested on a working machine: `uninstall quit:`/`signal: KILL` target the bundle id of the live tray widget, and the zap runs `pmset -a disablesleep 0` against a machine that may have the lid-awake veto armed. The zap clears that flag in `script:` *before* `delete:` removes `/etc/sudoers.d/claude-code-dashboard-lidawake` and the sleepreset LaunchDaemon, because those two files are what every disablesleep recovery path depends on and the flag survives reboot. Get the ordering wrong, or run a partial teardown, and the Mac can never sleep again. Test on a VM or spare machine before recommending `brew uninstall --cask --zap` anywhere in the docs. `docs/pages/settings.md` still documents the manual teardown as the only route, deliberately.

**Never run `brew audit --new` against the tap.** That flag implies both the notability check (this repo is far under the bar) and the Gatekeeper signing check (ad-hoc, fails by design). The tap's own CI calls `brew style --cask` and `brew audit --cask --strict --online` explicitly instead of `brew test-bot`, which would append `--new` itself.

**Bump chain:** publishing a release fires `.github/workflows/notify-tap.yml` → `repository_dispatch` → the tap's `bump-cask.yml`, which resolves the DMG digest from the release API and commits via the Contents API. It needs the `TAP_DISPATCH_TOKEN` secret (fine-grained PAT, Contents write on the tap) because `GITHUB_TOKEN` cannot reach another repo. Both workflows accept a manual `workflow_dispatch` for backfills, and re-dispatching an existing version is a safe no-op.

Related: [[macos_signing_strategy]], [[project_config_wiped_on_deploy]]
