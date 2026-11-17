//! `include_bytes!` needs the extension blobs at compile time. Local builds
//! run `just fetch-extensions` for the real files; CI compiles and unit-tests
//! against zero-byte stubs (only runtime loading needs the real thing).

use std::fs;
use std::path::Path;

fn main() {
    // Re-run when the extension blobs appear/disappear/change.
    println!("cargo:rerun-if-changed=extensions");
    println!("cargo:rerun-if-changed=build.rs");

    // Stamp the commit this binary was built from so `lotel --version` can
    // name it (M18 T1). `.git/HEAD` is two levels up from this manifest;
    // outside a checkout (vendored builds) git fails and we fall back to
    // "unknown" rather than breaking the build.
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    let git_sha = git_out(&["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=LOTEL_GIT_SHA={git_sha}");

    // ...but .git/HEAD is a symref: its mtime only moves on branch switches,
    // so a plain commit would leave the stamp stale. Also fingerprint the
    // branch file HEAD points at (`symbolic-ref -q HEAD` → refs/heads/main,
    // resolved via `rev-parse --git-path`, e.g. ../../.git/refs/heads/main)
    // and packed-refs. Without git nothing extra is emitted and the stamp
    // above stays "unknown".
    if let Some(branch) = git_out(&["symbolic-ref", "-q", "HEAD"])
        && let Some(path) = git_out(&["rev-parse", "--git-path", branch.as_str()])
    {
        rerun_if_changed_resolved(&path);
    }
    if let Some(path) = git_out(&["rev-parse", "--git-path", "packed-refs"]) {
        rerun_if_changed_resolved(&path);
    }

    for name in ["capped.duckdb_extension", "otlp.duckdb_extension", "fts.duckdb_extension"] {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("extensions").join(name);
        if !path.exists() {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, []).unwrap();
            println!("cargo:warning=stubbed missing extension blob {name} (fetch the real one before running)");
        }
    }
}

/// Run git, capturing trimmed stdout; None when git or a checkout is missing.
fn git_out(args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    (!text.is_empty()).then_some(text)
}

/// `rerun-if-changed` for a path git reported relative to this script's cwd
/// (crates/lotel/): emitted canonicalised so cargo fingerprints the real
/// file wherever the repo root sits relative to the manifest. Paths that do
/// not exist (a never-gc'd repo has no packed-refs) are skipped — a
/// listed-missing path makes cargo re-run the script on every build, and a
/// file appearing later also changes its containing dir anyway.
fn rerun_if_changed_resolved(path: &str) {
    if let Ok(resolved) = Path::new(path).canonicalize() {
        println!("cargo:rerun-if-changed={}", resolved.display());
    }
}
