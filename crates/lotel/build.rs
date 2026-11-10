//! `include_bytes!` needs the extension blobs at compile time. Local builds
//! run `just fetch-extensions` for the real files; CI compiles and unit-tests
//! against zero-byte stubs (only runtime loading needs the real thing).

use std::fs;
use std::path::Path;

fn main() {
    // Re-run when the extension blobs appear/disappear/change.
    println!("cargo:rerun-if-changed=extensions");
    println!("cargo:rerun-if-changed=build.rs");
    for name in ["capped.duckdb_extension", "otlp.duckdb_extension"] {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("extensions").join(name);
        if !path.exists() {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, []).unwrap();
            println!("cargo:warning=stubbed missing extension blob {name} (fetch the real one before running)");
        }
    }
}
