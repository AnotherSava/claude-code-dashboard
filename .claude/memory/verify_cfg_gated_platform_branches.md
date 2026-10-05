---
name: verify_cfg_gated_platform_branches
description: macOS cargo test never compiles the #[cfg(not(macos))] stub — invert the gates in a scratch copy, since cross-compiling to Windows is blocked
metadata:
  type: project
---

A `cargo test` on macOS compiles **only** the `#[cfg(target_os = "macos")]` arm,
so a rename inside a macOS `platform` module leaves its `#[cfg(not(...))]` stub
silently stale: green locally, red only on the Windows runner of
`.github/workflows/build.yml` (a windows-latest + macos-latest matrix — that
Windows job is the sole automatic guard for this class). Seen 2026-07-29, when
`lid_awake::platform::clamshell_causes_sleep` became `sleep_disabled` and the
stub kept the old name, breaking the build on `1d69821`.

Cross-compiling to check it does **not** work here — `cargo check --target
x86_64-pc-windows-msvc` dies in `aws-lc-sys` (pulled in by reqwest's rustls
backend), whose C needs `windows.h`. Re-confirmed 2026-10-04 on
`aws-lc-sys-0.43.0`, compiling `jitterentropy-timer.c`, after installing the
target; the target install is not the missing piece and adding it buys nothing.

**A stale stub is not the only class this hides — an UNUSED IMPORT is the other,
and it is easier to produce.** Windows compiles `composite.rs` and
`agwinterm.rs` in the *lib* build, where their `#[cfg(test)]` modules do not
exist, and compiles `agterm.rs` only in the *test* build, where its
`#[cfg(target_os = "macos")]` adapter impl does not exist. So a type imported at
the top of any of those files, used only from inside one of those regions, has no
user on the other platform and `-D warnings` refuses the build. macOS cannot see
it in either direction: a file gated `cfg(any(windows, test))` compiles there
only under `test`, which is exactly where the use does exist. Four such errors
shipped a red Windows build on 2026-10-04, from one seam change that added
`LabelBudget` to two files and `agterm_wire`/`LabelTarget`/`LabelWrite` to a
third.

The scratch-copy recipe below catches it, and so does a static check that costs
seconds: for each symbol imported at the top of a multi-platform file, find its
use sites and confirm each one lies **outside** every `cfg(test)` module and
every `cfg(target_os = ...)` region — any symbol whose uses are all inside one
needs its import moved in there with them.

What does work, locally and in seconds:

1. Copy the module to a scratch sibling (`<mod>_wincheck.rs`).
2. Invert its gates — `#[cfg(any())]` on the macOS arm (a never-true cfg), and
   delete the gate on the `#[cfg(not(target_os = "macos"))]` stub so it becomes
   the live one. Do the same for any `#[cfg(target_os = "macos")]` test.
3. Add `mod <mod>_wincheck;` to `lib.rs` and run `cargo check --lib`.
4. Revert both.

That compiles the other platform's arm against the real dependency graph, so
name-resolution and signature drift surface immediately; the only noise is
`dead_code` warnings from the duplicate module. A name-resolution error aborts
before type checking, so CI's "1 previous error" never proves the rest is clean
— this is how to find out.

Related: [[macos_signing_strategy]], [[verify_macos_window_geometry_via_ax]].
