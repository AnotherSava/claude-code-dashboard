---
name: vite-dev-port-launch-chain
description: package.json's "dev": "vite" must stay bare — the Tauri CLI runs it with PORT already set by scripts/dev.mjs, which wraps "tauri"
metadata:
  type: project
---

`npm run tauri dev` goes through `scripts/dev.mjs`, which resolves the port (env `PORT`, else the ports
registry's `tauri-dashboard-vite-dev`, else a kernel `listen(0)` probe), writes a per-pid `--config` file
overriding `build.devUrl`, and re-enters itself with `PORT` set. The Tauri CLI then runs
`beforeDevCommand`, which is `npm run dev` — so a bare `vite` is correct there, and `vite.config.ts`
reads `process.env.PORT` out of the environment that chain built.

**Why:** the `ports-from-registry` convention (v15) tells an adopting repo to point a dev script at
`scripts/dev.mjs`, and its example is a Next project where `next dev` is itself the server. Rewriting
`"dev"` rather than `"tauri"` resolves the port *after* the Tauri CLI has read `build.devUrl`, so Vite
binds the registry's number while the webview opens on the committed one — that is the generic failure,
and v15 step 3 now states it. This repo fails louder: `scripts/dev.mjs` forwards every subcommand other
than `dev` straight to the Tauri CLI, so a `"dev": "node scripts/dev.mjs vite"` re-entered through
`beforeDevCommand` hands the CLI `vite` as a subcommand. The entry point is the `"tauri"` script.

**How to apply:** leave `"dev": "vite"` alone in any ports or dev-script refactor, and keep a real port in
`build.devUrl` rather than emptying it. A hand-run `npm run dev` refusing with "PORT is unset" is the
`requireResolvedPort` plugin in `vite.config.ts` working as designed, not something to route around. See
[[debug_dev_build_alongside_installed]].

The Vite half is checkable **without launching the widget**, which matters because the window needs the
user's go-ahead: `npm run dev` with `PORT` unset must fail inside `configureServer` during `_createServer`
and exit on its own, and `PORT=<n> npm run dev` must bind that port and serve 200. Both are servers with no
window. What those two cannot reach is the `dev.mjs` → registry → `devUrl`-override → Tauri CLI leg, which
has no headless path and is the only part a dev run has to confirm.
