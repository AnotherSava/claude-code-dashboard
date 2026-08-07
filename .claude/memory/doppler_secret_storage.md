---
name: doppler_secret_storage
description: The documented Doppler render-then-wipe deploy flow is currently NOT what scripts/deploy.sh does — verified drift as of 2026-08-07
metadata: 
  node_type: memory
  type: project
  modified: 2026-08-07T07:31:02.599Z
---

**Originally designed flow (2026-06-ish):** the app's three secrets — `TELEGRAM_BOT_TOKEN`, `TELEGRAM_CHAT_ID`, `SYNC_TOKEN` — live in **Doppler project `claude-code-dashboard`, config `dev`**. `config/local.template.json` (gitignored, secret-free) was meant to hold `{{tojson .NAME}}` placeholders plus per-machine prefs; the per-machine `scripts/deploy.sh` would run `doppler secrets substitute config/local.template.json ... > config/local.json`, delegate to the global Tauri deploy (which `cp -f`'s `config/local.json` onto the installed `config.json`), then wipe `config/local.json` via a `trap ... EXIT`.

**Verified current reality (2026-08-07):** none of that render/wipe step exists anymore. `scripts/deploy.sh` in this repo is a bare one-liner (`bash ~/.claude/skills/deploy/scripts/deploy-tauri.sh "$@"`) with no Doppler call and no trap; `~/.claude/skills/deploy/scripts/*.sh` (checked all of them) contain no `doppler`/`template`/`wipe` logic either. `config/local.template.json` does not exist in the tree. `config/local.json` itself holds real, non-placeholder values written directly — including the sync token in plaintext (a real 32-char hex value), not a `{{tojson .SYNC_TOKEN}}` placeholder. Deploy just `cp -f`'s this file onto the installed `config.json` as-is (matches [[project_config_wiped_on_deploy]]) and nothing ever wipes it back out — it's a persistent plaintext file on disk (still gitignored, so not a repo leak, but no longer Doppler-backed).

**Why this matters:** either the Doppler wiring was dropped when `scripts/deploy.sh`/the deploy skill was last regenerated (it's untracked, so a regen silently loses custom logic), or it was never carried over for this project. Don't assert "no plaintext secrets at rest" as current fact — it was true by design, not by current observation.

**How to apply:** if a future session touches deploy/secrets here, flag this drift to the user rather than assuming the documented flow is live — verify by reading `scripts/deploy.sh` and checking for `config/local.template.json` before relying on either. Restoring Doppler-backed secrets (if wanted) means re-adding the `doppler secrets substitute` + wipe step, e.g. via the deploy skill setup. See also the global `feedback_doppler_secrets` / `tools-doppler-credentials` memories for the general Doppler convention this project was meant to follow.
