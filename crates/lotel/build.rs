//! `include_bytes!` needs the extension blobs at compile time. They are
//! staged in OUT_DIR (a published crate may not write into its own source):
//!
//! 1. `crates/lotel/extensions/<ext>.duckdb_extension`, when present
//!    (`just fetch-extensions` puts them there), is copied as is;
//! 2. otherwise the blob pinned in `extensions.lock` for the target platform
//!    is downloaded with curl and its sha256 checked (`cargo install lotel`);
//! 3. `LOTEL_STUB_EXTENSIONS=1` writes zero-byte stubs instead, for builds
//!    that never load them (docs, offline clippy). A stubbed binary ingests
//!    nothing, and `just dist` refuses to package one.

use sha2::{Digest, Sha256};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

const EXTENSIONS: [&str; 3] = ["capped", "otlp", "fts"];

fn main() {
    // Re-run when the extension blobs appear/disappear/change.
    println!("cargo:rerun-if-changed=extensions");
    println!("cargo:rerun-if-changed=extensions.lock");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=LOTEL_STUB_EXTENSIONS");

    // Stamp the commit this binary was built from so `lotel --version` can
    // name it. `.git/HEAD` is two levels up from this manifest;
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

    stage_extensions();
}

fn stage_extensions() {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let stub = std::env::var("LOTEL_STUB_EXTENSIONS").is_ok_and(|v| v == "1");
    let target = std::env::var("TARGET").expect("TARGET");
    for ext in EXTENSIONS {
        let file = format!("{ext}.duckdb_extension");
        let dest = out.join(&file);
        let local = manifest.join("extensions").join(&file);
        if local.metadata().is_ok_and(|m| m.len() > 0) {
            copy_if_changed(&local, &dest);
        } else if stub {
            fs::write(&dest, []).unwrap();
            println!("cargo:warning=LOTEL_STUB_EXTENSIONS=1: {file} is a zero-byte stub; this binary ingests nothing");
        } else {
            let platform = duckdb_platform(&target);
            let (sha, url) = lock_row(&manifest.join("extensions.lock"), ext, platform);
            if !(dest.exists() && sha256_file(&dest) == sha) {
                download(&url, &sha, &dest);
            }
        }
    }
}

/// DuckDB's name for the platform the binary is being built for.
fn duckdb_platform(target: &str) -> &'static str {
    match target {
        "aarch64-apple-darwin" => "osx_arm64",
        "x86_64-apple-darwin" => "osx_amd64",
        "aarch64-unknown-linux-gnu" => "linux_arm64",
        "x86_64-unknown-linux-gnu" => "linux_amd64",
        other => panic!(
            "lotel: no DuckDB extension builds for target {other}. Supported: macOS and Linux (glibc) on arm64 or x86_64. \
             For a build that never loads extensions, set LOTEL_STUB_EXTENSIONS=1."
        ),
    }
}

/// The `<sha256> <url>` columns of the one lock row for `ext` on `platform`.
fn lock_row(lock: &Path, ext: &str, platform: &str) -> (String, String) {
    let text = fs::read_to_string(lock).unwrap_or_else(|e| panic!("lotel: read {}: {e}", lock.display()));
    let rows: Vec<Vec<&str>> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| l.split_whitespace().collect::<Vec<_>>())
        .filter(|f| f.len() == 4 && f[0] == ext && f[1] == platform)
        .collect();
    match rows.as_slice() {
        [row] => (row[2].to_owned(), row[3].to_owned()),
        [] => panic!("lotel: extensions.lock has no row for {ext} {platform}"),
        _ => panic!("lotel: extensions.lock has more than one row for {ext} {platform}"),
    }
}

/// curl the (gzipped) blob, gunzip it, check its sha256, then move it into place.
fn download(url: &str, want: &str, dest: &Path) {
    let gz = dest.with_extension("download");
    let status = Command::new("curl")
        .args(["-fsSL", "--retry", "3", "-o"])
        .arg(&gz)
        .arg(url)
        .status()
        .unwrap_or_else(|e| panic!("lotel: could not run curl to download {url}: {e}. Install curl, or fetch the extensions yourself (scripts/fetch-extensions.sh)."));
    assert!(status.success(), "lotel: download failed ({status}): {url}");
    let mut bytes = Vec::new();
    let file = fs::File::open(&gz).unwrap();
    if url.ends_with(".gz") {
        flate2::read::GzDecoder::new(file).read_to_end(&mut bytes)
    } else {
        std::io::BufReader::new(file).read_to_end(&mut bytes)
    }
    .unwrap_or_else(|e| panic!("lotel: could not read {url}: {e}"));
    fs::remove_file(&gz).ok();
    let got = hex(&Sha256::digest(&bytes));
    assert!(got == want, "lotel: sha256 mismatch for {url}\n  expected {want} (extensions.lock)\n  got      {got}");
    let tmp = dest.with_extension("tmp");
    fs::write(&tmp, &bytes).unwrap();
    fs::rename(&tmp, dest).unwrap();
}

fn copy_if_changed(from: &Path, to: &Path) {
    let same = to.metadata().is_ok_and(|m| m.len() == from.metadata().unwrap().len()) && sha256_file(from) == sha256_file(to);
    if !same {
        fs::copy(from, to).unwrap_or_else(|e| panic!("lotel: copy {}: {e}", from.display()));
    }
}

fn sha256_file(p: &Path) -> String {
    hex(&Sha256::digest(fs::read(p).unwrap_or_default()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
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
