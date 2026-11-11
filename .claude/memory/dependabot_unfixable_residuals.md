---
name: dependabot-unfixable-residuals
description: Which Dependabot alerts here cannot be closed by a lockfile update (glib, deepmerge) and how the docs Gemfile holds openssl so `bundle install` works on the Windows box
metadata:
  type: project
---

Measured 2026-11-11 while closing 23 open alerts for a shepherd session.

- **glib 0.18.5 (GHSA-wrw7-89jp-8q8g, fixed in 0.20)**: Linux-only, pinned by gtk-rs 0.18 (`gtk`, `gdk`, `webkit2gtk 2.0`) through tauri 2.12 / wry 0.57 / tao 0.37 / muda / tray-icon. `cargo update -p glib` cannot move it; it closes only when tauri moves its Linux stack to gtk-rs 0.20+. Not compiled on Windows or macOS.
- **deepmerge 4.3.1 (GHSA-gcqx-w48m-w2h4)**: no patched version exists; dev-only, via `@sveltejs/vite-plugin-svelte`. `npm audit fix` reports "No fix available".
- **docs/Gemfile.lock**: rubyzip was capped `< 3.0` by `jekyll-remote-theme 0.4.3` under `github-pages 232`. `github-pages 233` brings `jekyll-remote-theme 0.6.2`, which lifts the cap and needs `openssl >= 3.1.2`. Left free, bundler locks the newest openssl (4.x), which has no prebuilt Windows gem: RubyInstaller then tries to install OpenSSL headers through MSYS2 pacman, whose keyring on the Windows box is stale ("key … is unknown", "keyring is not writable"), and `bundle install` fails. So `docs/Gemfile` pins `gem "openssl", "~> 3.2.4"`, matching the default gem of the box's Ruby 3.3.12; with that, `bundle check` reports the dependencies satisfied and nothing is built. A Ruby with a different default openssl (an older 3.3 patch, or 3.4+, likely the Mac's) downloads and compiles openssl 3.2.x for the pin. GitHub Pages ignores this Gemfile, so only a local docs build is affected.

**How to apply:** on the next Dependabot sweep, report glib and deepmerge as blocked upstream rather than re-investigating them. For the docs Gemfile, `bundle lock --update <gems> --conservative` then `bundle install` both work on the Windows box while the openssl pin holds. Keep the pin unless the box's Ruby moves to a different default openssl; dropping it brings back the MSYS2 build, and refreshing that keyring is a change to the machine that needs the user's go-ahead.
