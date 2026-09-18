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
#
# The conventions checker below is the one deliberate exception to that lockstep,
# and it belongs in no workflow: it measures the machine-local conventions this
# repo has adopted (`.claude/conventions` says how far), several of which are
# about files and links that only exist on a developer's box. Nothing else
# invokes it, so without this line every rule the repo takes on would be measured
# exactly once — by the `/adopt` walk, on the day it ran — and the migrations
# would be done with the enforcement nowhere. It runs first because a convention
# violation should be reported before the slow suites, and `python` rather than
# `python3` to match the figure check at the bottom and the Windows box this is
# most often run on.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

echo "==> python ~/.claude/conventions/check.py ."
python ~/.claude/conventions/check.py .

echo "==> npm run check"
npm run check

echo "==> cargo check --all-targets (a warning fails the build)"
# `cargo test` below compiles only the TEST cfg, so a warning that exists solely
# in the plain build is invisible to it — which is how nine dead-code warnings
# sat in every `deploy` unread. `--all-targets` rather than the `--lib --tests`
# this check first said, for one measured reason and not the one first written
# here: that pair already forces `build.rs` (a build script is compiled before
# anything else in the package) and already compiles `src/main.rs` as the bin's
# TEST target, so a plainly-dead fn in either is caught by both commands. What it
# never performs is the bin's PLAIN build, so an item live only under `cfg(test)`
# there escapes it — measured, `--lib --tests` exits 0 on exactly that and
# `--all-targets` exits 101. One hole, not the three targets claimed before.
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
