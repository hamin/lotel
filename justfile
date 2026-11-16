# lotel task runner

CAPPED_EXT := "../capped/build/release/extension/capped/capped.duckdb_extension"
OTLP_EXT := "~/.duckdb/extensions/v2.0.0/osx_arm64/otlp.duckdb_extension"
FTS_EXT := "~/.duckdb/extensions/v2.0.0/osx_arm64/fts.duckdb_extension"
DEST := "crates/lotel/extensions"

# Copy the prebuilt extensions into the app for embedding.
# capped: built from the sibling repo (`cd ../capped && GEN=ninja make release`).
# otlp:   community extension, e.g. `duckdb -c "INSTALL otlp FROM community;"` fetches it.
fetch-extensions:
    mkdir -p {{DEST}}
    cp {{CAPPED_EXT}} {{DEST}}/capped.duckdb_extension
    cp {{OTLP_EXT}} {{DEST}}/otlp.duckdb_extension
    cp {{FTS_EXT}} {{DEST}}/fts.duckdb_extension

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

# Multi-tenant proof: two tenants, one process — isolation, per-tenant tokens
# and TTLs, same-catalog guard. Needs the real capped + otlp extensions.
tenant-proof:
    ./scripts/tenant-proof.sh

# Soak (M17, local by design): sustained ingest + UI polling with a watchdog
# asserting no panics, no wedges, bounded RSS/fds, and caps held.
#   SOAK_MINUTES=60 RATE=50 just soak
soak:
    ./scripts/soak.sh

# Stress (M17, local by design): burst ingest + concurrent read storm, then
# re-verify every surface. Zero failures is the bar.
#   STRESS_MINUTES=2 CONC=8 just stress
stress:
    ./scripts/stress.sh
