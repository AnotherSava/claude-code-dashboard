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
# the commands below identical to the ones the workflows in `.github/workflows/`
# run, in the same order — `build.yml` for every step up to the last, `docs.yml`
# for the figure check. If a workflow changes, change this with it, and if a new
# workflow gates `main`, add it here too. That rule runs both ways: a check added
# here belongs in the workflow as well, or the two disagree about what `main`
# requires and the stricter one is whichever you happened to run.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

echo "==> npm run check"
npm run check

echo "==> cargo check --all-targets (a warning fails the build)"
# `cargo test` below compiles only the TEST cfg, so a warning that exists solely
# in the plain build is invisible to it — which is how nine dead-code warnings
# sat in every `deploy` unread. Both cfgs matter, and so does every target: this
# crate has three (`cargo metadata` lists lib, the `src/main.rs` bin, and the
# `build.rs` custom-build), and a first cut of this check said `--lib --tests`,
# which compiled none of the last two. Both were proved by injecting an unused fn
# into each and watching it pass. `--all-targets` covers lib, bins, tests,
# benches and examples in one pass and costs nothing measurable here.
#
# The platform axis is the other half: the cluster behind `observe_caption` is
# dead on macOS and live on Windows, so each leg of the CI matrix is the only
# thing that can see the other's unreachable code. Warnings are failed on rather
# than printed because a warning nobody must act on is one nobody reads, and the
# tenth would have hidden among the nine. `-D warnings` reaches this crate only:
# cargo compiles registry dependencies with `--cap-lints allow`, so a warning in
# someone else's crate cannot fail this. It costs one rebuild of the dependency
# graph the first time, the flag being part of cargo's fingerprint; after that it
# is cached beside the unflagged artifacts. The failure is cargo's own exit status
# rather than a grep for "warning", which would report a compile error as a clean
# run.
RUSTFLAGS="-D warnings" cargo check --manifest-path src-tauri/Cargo.toml --all-targets

echo "==> cargo test --lib"
cargo test --manifest-path src-tauri/Cargo.toml --lib

echo "==> npm run build"
npm run build

echo "==> python docs/screenshots/check-figures.py"
python docs/screenshots/check-figures.py

echo "All CI checks passed."
