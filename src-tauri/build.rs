use std::hash::{Hash, Hasher};
use std::path::Path;

fn main() {
    register_frontend_fingerprint(Path::new("../dist"));
    tauri_build::build()
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
