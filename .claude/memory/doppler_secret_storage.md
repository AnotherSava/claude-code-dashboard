---
name: doppler_secret_storage
description: Windows deploy renders config/local.template.json from Doppler claude-code-dashboard/dev inside deploy-tauri.sh; the Mac copies a plaintext local.json
metadata: 
  node_type: memory
  type: project
  modified: 2026-11-05T15:30:00.000Z
---

The app's secrets (`TELEGRAM_BOT_TOKEN`, `TELEGRAM_CHAT_ID`, `SYNC_TOKEN`) live in **Doppler project `claude-code-dashboard`, config `dev`**, named in the committed `config/deploy.env` (`DOPPLER_PROJECT`/`DOPPLER_CONFIG`, since convention v17 on 2026-11-05).

**Windows (CHROME):** the global `deploy-tauri.sh` itself renders the gitignored `config/local.template.json` (`{{tojson .NAME}}` placeholders) through Doppler straight into the installed `config.json`. Observed on the 2026-11-05 deploy: `Rendered config/local.template.json from Doppler claude-code-dashboard/dev into …/config.json`. There is no per-repo `scripts/deploy.sh` any more (v17 removed it), and no transient `local.json` is left behind.

**Mac (AIR):** no template; a plaintext gitignored `config/local.json` is copied verbatim, so the Doppler keys in `deploy.env` are inert there (the render only happens where the template exists).

**How to apply:** don't claim "no plaintext secrets at rest" for the Mac. A Mac move onto Doppler means adding a `local.template.json` there, nothing else. See [[project_config_wiped_on_deploy]].
