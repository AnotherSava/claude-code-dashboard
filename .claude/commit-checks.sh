#!/bin/bash
# What CI will run, run here first.
#
# This exists because it did not, once: a type error reached `main` and was
# caught by the Build workflow on GitHub rather than before the commit. Both
# `npx svelte-check` and `npm run build` had been run by hand and both passed —
# `svelte-check` invoked bare picks up the *root* tsconfig, while CI passes
# `--tsconfig ./tsconfig.app.json`, and the file with the error was outside the
# first one's scope. A check pointed at the wrong config is not a weaker check,
# it is a differently-scoped one, and it reports success just as confidently.
#
# So the rule this file enforces is: don't approximate the gate, run it. Keep
# the commands below identical to `.github/workflows/build.yml`, in the same
# order — if that workflow changes, change this with it.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

echo "==> npm run check"
npm run check

echo "==> cargo test --lib"
cargo test --manifest-path src-tauri/Cargo.toml --lib

echo "==> npm run build"
npm run build

echo "All CI checks passed."
