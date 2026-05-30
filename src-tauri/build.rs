use std::hash::{Hash, Hasher};
use std::path::Path;

fn main() {
    // The Python hook script is embedded into the binary via include_str! in
    // src/setup.rs; tell cargo to recompile when it changes.
    println!("cargo:rerun-if-changed=../integrations/claude_hook.py");
    register_release_date();
    register_frontend_fingerprint(Path::new("../dist"));
    tauri_build::build()
}

/// Embed today's date as the app's "release date". For CI release builds
/// (which run on the same day as the tag), this equals the tag date. For
/// dev builds, it's "the date this binary was built" — which matches what
/// a user expects to see for the binary they just installed. No git
/// dependency: works for tarball / sparse-checkout / missing-`.git` builds.
/// Format: YYYY-MM-DD; Rust-side formats it to "Month D, YYYY" at runtime.
fn register_release_date() {
    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
    println!("cargo:rustc-env=APP_RELEASE_DATE={date}");
}

/// `tauri-build` registers only `tauri.conf.json` and `capabilities/` as
/// `rerun-if-changed` inputs — not the frontend `dist` dir. On a stable
/// toolchain the `generate_context!` proc macro can't track the asset files it
/// embeds, so an incremental local build that changes only the frontend won't
/// recompile the crate and ships a stale UI (CI/clean builds are unaffected).
/// Fold a content fingerprint of `dist` into a `rustc-env` that `lib.rs` reads:
/// when the frontend changes the env value changes and cargo recompiles the
/// crate, re-running `generate_context!` against the fresh `dist`. The
/// `rerun-if-changed` lines make this build script re-run to recompute it.
fn register_frontend_fingerprint(dist: &Path) {
    println!("cargo:rerun-if-changed={}", dist.display());
    // (relative path, size) per file — vite content-hashes asset filenames, so
    // any content change alters the path set; sizes catch fixed-name files like
    // index.html. Deliberately not mtime, so a no-op `vite build` (identical
    // output, fresh timestamps) doesn't force a needless recompile.
    let mut files: Vec<(String, u64)> = Vec::new();
    let mut stack = vec![dist.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            println!("cargo:rerun-if-changed={}", path.display());
            match entry.file_type() {
                Ok(ft) if ft.is_dir() => stack.push(path),
                Ok(_) => {
                    let len = entry.metadata().map(|m| m.len()).unwrap_or(0);
                    files.push((path.to_string_lossy().into_owned(), len));
                }
                _ => {}
            }
        }
    }
    files.sort();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    files.hash(&mut hasher);
    println!("cargo:rustc-env=FRONTEND_FINGERPRINT={:016x}", hasher.finish());
}
