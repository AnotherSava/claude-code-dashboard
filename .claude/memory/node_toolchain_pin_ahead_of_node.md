---
name: node_toolchain_pin_ahead_of_node
description: "packageManager pins npm@12.0.2 while this Windows box runs Node 24.13.0, below npm 12's floor — bump Node, never lower the pin"
metadata: 
  node_type: memory
  type: project
---

`package.json` pins `"packageManager": "npm@12.0.2"` while `.nvmrc` says `24`, and this Windows
machine runs Node 24.13.0 — below npm 12's supported range, `^22.22.2 || ^24.15.0 || >=26.0.0`.
Corepack is enabled for npm here, so it fetches npm 12 anyway and every npm command in this repo
prints `npm warn cli npm v12.0.2 does not support Node.js v24.13.0`, including both npm steps of
`.claude/commit-checks.sh`. The gate still passes.

Chosen deliberately on 2026-09-18, with the incompatibility stated before the choice, over the
`npm@11.18.0` this box was already running.

**The repair is bumping Node here to the latest 24.x — never lowering the pin.** A future session
reading only the warning will be tempted to edit `packageManager` back down; that undoes a decision
rather than fixing the machine.

CI is unaffected: `build.yml` and `release.yml` both use `setup-node` with `node-version: '24'`,
which installs the newest 24.x and so sits above npm 12's floor.

See [[project_config_wiped_on_deploy]] for why toolchain state belongs in committed files rather
than in `config.json`.
