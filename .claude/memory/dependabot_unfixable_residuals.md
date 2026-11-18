---
name: dependabot-unfixable-residuals
description: Which Dependabot alerts here cannot be closed by a lockfile update (glib) and how to update and verify the docs Gemfile on the Windows box, where `bundle install` fails
metadata:
  type: project
---

Measured 2026-11-11 while closing 23 open alerts for a shepherd session.

- **glib 0.18.5 (GHSA-wrw7-89jp-8q8g, fixed in 0.20)**: Linux-only, pinned by gtk-rs 0.18 (`gtk`, `gdk`, `webkit2gtk 2.0`) through tauri 2.12 / wry 0.57 / tao 0.37 / muda / tray-icon. `cargo update -p glib` cannot move it; it closes only when tauri moves its Linux stack to gtk-rs 0.20+. Not compiled on Windows or macOS.
- **deepmerge 4.3.1 (GHSA-gcqx-w48m-w2h4)**: no patched version exists; dev-only, via `@sveltejs/vite-plugin-svelte`. `npm audit fix` reports "No fix available".
- **docs/Gemfile.lock**: rubyzip was capped `< 3.0` by `jekyll-remote-theme 0.4.3` under `github-pages 232`. `github-pages 233` brings `jekyll-remote-theme 0.6.2`, which lifts the cap and needs `openssl >= 3.1.2`. Left free, bundler locks the newest openssl (4.x), which has no prebuilt Windows gem: RubyInstaller then tries to install OpenSSL headers through MSYS2 pacman, whose keyring on the Windows box is stale ("key … is unknown", "keyring is not writable"), and `bundle install` fails. So `docs/Gemfile` pins `gem "openssl", "~> 3.2.4"`, matching the default gem of the box's Ruby 3.3.12; with that, `bundle check` reports the dependencies satisfied. A Ruby with a different default openssl (an older 3.3 patch, or 3.4+, likely the Mac's) downloads and compiles openssl 3.2.x for the pin. GitHub Pages ignores this Gemfile, so only a local docs build is affected.

Re-checked 2026-11-18: the newest tao (0.37.2) still requires `gtk ^0.18`, so glib stays; deepmerge no longer appears among the open alerts.

Also measured 2026-11-18: `bundle install` on the Windows box fails on openssl 3.2.4 with the same MSYS2 keyring errors, on the lockfile before a change as well as after, even though `bundle check` reports the dependencies satisfied. The reason is the box's untracked `docs/.bundle/config` (written 2026-09-10), which sets `BUNDLE_PATH: vendor/bundle`: an install into that path compiles openssl 3.2.4 from source instead of taking Ruby's default gem. What does work is `bundle lock --update <gems> --conservative`, then `bundle exec jekyll build -d ../tmp/docs-build` to prove the bundle still builds.

**How to apply:** on the next Dependabot sweep, report glib as blocked upstream rather than re-investigating it. For the docs Gemfile, update with `bundle lock --update <gems> --conservative` and verify with `bundle check` plus a `jekyll build`; do not read a failing `bundle install` as caused by the update. Keep the pin unless the box's Ruby moves to a different default openssl; dropping it brings back the MSYS2 build, and refreshing that keyring is a change to the machine that needs the user's go-ahead.
