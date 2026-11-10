# lotel task runner

CAPPED_EXT := "../capped/shell/build/release/extension/capped/capped.duckdb_extension"
OTLP_EXT := "~/.duckdb/extensions/v1.5.6/osx_arm64/otlp.duckdb_extension"
DEST := "crates/lotel/extensions"

# Copy the prebuilt extensions into the app for embedding.
# capped: built from the sibling shell (`cd ../capped/shell && GEN=ninja make release`).
# otlp:   community extension, e.g. `duckdb -c "INSTALL otlp FROM community;"` fetches it.
fetch-extensions:
    mkdir -p {{DEST}}
    cp {{CAPPED_EXT}} {{DEST}}/capped.duckdb_extension
    cp {{OTLP_EXT}} {{DEST}}/otlp.duckdb_extension

# Full release build (fetches the extension first)
build: fetch-extensions
    cargo build --release

# Dev loop
run: fetch-extensions
    cargo run

# Fast checks
check: fetch-extensions
    cargo check

# Pure-logic unit tests (no extension files needed; build.rs stubs them).
test:
    cargo test

# Lint
lint:
    cargo clippy --all-targets -- -D warnings

# End-to-end: boots the binary, ingests testdata/, asserts every page.
smoke: fetch-extensions
    cargo build
    ./scripts/smoke.sh
