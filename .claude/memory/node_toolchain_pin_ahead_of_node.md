---
name: node_toolchain_pin_ahead_of_node
description: "This repo pins npm@12.0.2, which needs Node >=24.15.0; if npm -v disagrees with the field, a Node upgrade reset corepack and the pin is inert"
metadata: 
  node_type: memory
  type: project
---

`package.json` pins `"packageManager": "npm@12.0.2"` and `.nvmrc` says `24`. npm 12 supports
`^22.22.2 || ^24.15.0 || >=26.0.0`, so the two agree only above 24.15.0 — this box was on 24.13.0
when the pin was chosen on 2026-09-18 and was upgraded to 24.19.0 the same day to close the gap.
**Bump Node rather than lowering the pin** if they ever disagree again.

**The check that matters is `npm -v` inside the repo, not the field.** `package.json` carrying the
pin proves the intent; only corepack's shim makes it take effect, and a Node upgrade replaces the
install directory and takes that shim with it — silently, with `npm -v` reverting to the bundled
version and the `package-manager-pin` rule still passing, because it asserts the field's shape and
the field is untouched. Repair is `corepack enable npm` in an elevated shell. Mechanics and the
measured case are in `learnings/corepack-packagemanager-pin.md`.

The gate's own npm steps are where a mismatch shows up first, as a warning line above `npm run
check` and `npm run build` rather than as a failure — so it is easy to read past.
