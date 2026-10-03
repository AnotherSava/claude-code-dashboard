---
created: 2026-10-03 06:48:07
---

# Add a rustfmt check to the commit gate, after triaging its baseline

`.claude/commit-checks.sh` runs the conventions checker, `npm run check`, `cargo check --all-targets` under `-D warnings`, `cargo test --lib`, `npm run build` and the two docs checks — and nothing that checks formatting. So a struct literal can be indented wrongly and still pass every gate green.

Found during the cross-machine read-ness work: adding two fields to `AgentSession` meant inserting an initializer line into every constructor, done with a scripted replace at a fixed indent. Four of them landed misindented in production files (`label_policy`, `http_server`, `terminal_title`, `log_watcher`, `lib`), and three separate review lenses reported it because nothing mechanical could. Reindented by hand.

`rustfmt` is not installed on this machine's stable toolchain at all — `cargo fmt -- --check` answers `'cargo-fmt' is not installed for the toolchain`. So adopting this is `rustup component add rustfmt` plus whatever the first full run turns up.

The reason this is a memo and not a one-line addition: the Best-Practice Adoption rule says measure before proposing, and this codebase has never been formatted, so the first `cargo fmt` would rewrite a very large fraction of it. Deciding that is a separate change from any feature work — it wants its own commit, a look at what rustfmt does to the long doc comments this repo leans on heavily, and a `rustfmt.toml` that fits the existing style (notably the single-line-expression preference in CLAUDE.md's Code Style, which `max_width` defaults would fight).

Worth checking whether CI should run it too, since the gate's own header says to keep it in lockstep with `.github/workflows/build.yml`.
