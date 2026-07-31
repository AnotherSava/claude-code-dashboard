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
backend), whose C needs `windows.h`.

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
