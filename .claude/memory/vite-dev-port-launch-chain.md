---
name: vite-dev-port-launch-chain
description: package.json's "dev": "vite" must stay bare — the Tauri CLI runs it under scripts/dev.mjs, which has already set PORT
metadata:
  type: project
---

`npm run tauri dev` goes through `scripts/dev.mjs`, which resolves the port (env `PORT`, else the ports
registry's `tauri-dashboard-vite-dev`, else a kernel `listen(0)` probe), writes a per-pid `--config` file
overriding `build.devUrl`, and re-enters itself with `PORT` set. The Tauri CLI then runs
`beforeDevCommand`, which is `npm run dev` — so a bare `vite` is correct there, and `vite.config.ts`
reads `process.env.PORT` out of the environment that chain built.

**Why:** the `ports-from-registry` convention (v15) tells an adopting repo to rewrite its `"dev"` script
to call `scripts/dev.mjs`, its example being a Next project where `next dev` is itself the server. Doing
that here recurses — `dev.mjs` spawns the Tauri CLI, which runs `npm run dev`, which re-enters `dev.mjs`.
The entry point is the `"tauri"` script, not `"dev"`.

**How to apply:** leave `"dev": "vite"` alone in any ports or dev-script refactor, and keep a real port in
`build.devUrl` rather than emptying it. A hand-run `npm run dev` refusing with "PORT is unset" is the
`requireResolvedPort` plugin in `vite.config.ts` working as designed, not something to route around. See
[[debug_dev_build_alongside_installed]].

The Vite half is checkable **without launching the widget**, which matters because the window needs the
user's go-ahead: `npm run dev` with `PORT` unset must fail inside `configureServer` during `_createServer`
and exit on its own, and `PORT=<n> npm run dev` must bind that port and serve 200. Both are servers with no
window. What those two cannot reach is the `dev.mjs` → registry → `devUrl`-override → Tauri CLI leg, which
has no headless path and is the only part a dev run has to confirm.
