---
name: doppler_secret_storage
description: Windows deploy renders config/local.template.json from Doppler local/dev_claude-code-dashboard inside deploy-tauri.sh; the Mac copies a plaintext local.json
metadata: 
  node_type: memory
  type: project
  modified: 2026-11-13T00:00:00.000Z
---

The app's secrets (`TELEGRAM_BOT_TOKEN`, `TELEGRAM_CHAT_ID`, `SYNC_TOKEN`) live in **Doppler project `local` (the workstation-only shard), config `dev_claude-code-dashboard`**, named in the committed `config/deploy.env` (`DOPPLER_PROJECT`/`DOPPLER_CONFIG`, since convention v17 on 2026-11-05). Moved there on 2026-11-13 from the dashboard's own project `claude-code-dashboard/dev`, to free a project slot under the workplace's 10-project cap; the copy was verified by length and sha256 prefix per key. The old project has since been deleted with the user's approval, so a reference to `claude-code-dashboard/dev` anywhere is stale.

`local/dev` is a shard root and stays empty: every `dev_*` config in `local` inherits its values, so always name `dev_claude-code-dashboard` in full.

**Windows (CHROME):** the global `deploy-tauri.sh` itself renders the gitignored `config/local.template.json` (`{{tojson .NAME}}` placeholders) through Doppler straight into the installed `config.json`. There is no per-repo `scripts/deploy.sh` any more (v17 removed it), and no transient `local.json` is left behind.

**Mac (AIR):** no template; a plaintext gitignored `config/local.json` is copied verbatim, so the Doppler keys in `deploy.env` are inert there (the render only happens where the template exists).

**How to apply:** don't claim "no plaintext secrets at rest" for the Mac. A Mac move onto Doppler means adding a `local.template.json` there, nothing else. See [[project_config_wiped_on_deploy]].
