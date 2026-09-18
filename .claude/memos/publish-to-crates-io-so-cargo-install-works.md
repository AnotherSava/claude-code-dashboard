---
created: 2026-09-17 10:23:25
---

# Publish the app to crates.io so `cargo install claude-code-dashboard` works as a third install route

Alongside the two routes `docs/pages/install.md` documents today: the NSIS
installer from the Releases page on Windows, and the Homebrew cask on macOS.

Two things block it, both checked rather than assumed:

- **The frontend is outside the crate.** `tauri.conf.json` sets
  `frontendDist: "../dist"` with `beforeBuildCommand: "npm run build"`, and
  `dist` is gitignored. `cargo publish` packages the package directory, so a
  path above `src-tauri/` is not in the tarball at all — and a crates.io build
  has no node and no network to run Vite itself. Whatever the answer is, it has
  to put built frontend assets inside `src-tauri/`.
- **No license field.** `src-tauri/Cargo.toml` has `description` and `authors`
  but neither `license` nor `license-file`, and crates.io requires one. Note the
  repo-level decision in the global licensing rule before picking a value.

Open question worth settling before any of that: `cargo install` puts a bare
binary in `~/.cargo/bin`, not a bundle — so does the macOS side still work? The
tray icon and `ActivationPolicy::Accessory` are set up expecting a `.app`, and
an unbundled binary may not get either. If it doesn't, this route is Windows and
Linux only, which is worth knowing before the packaging work starts.
