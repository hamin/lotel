# lotel task runner

version := `awk -F'"' '/^version =/ {print $2; exit}' crates/lotel/Cargo.toml`

# Fetch the extension blobs embedded into the app (build.rs stubs missing
# ones with zero bytes → a release built without these ingests nothing).
# otlp/fts: pinned URLs + sha256 in extensions.lock, verified on every fetch.
# capped: source build (no release yet) — $CAPPED_EXT or the sibling repo
# (`cd ../capped && GEN=ninja make release`). See scripts/fetch-extensions.sh.
fetch-extensions:
    ./scripts/fetch-extensions.sh

# Full release build (fetches the extension first)
build: fetch-extensions
    cargo build --release

# M18 release engineering: build the tarball + SHA256SUMS under dist/ and
# boot the tarball's own binary as an ingest sanity gate (refuses stubbed
# extensions; see scripts/dist.sh).
dist:
    ./scripts/dist.sh

# M18 T5: build the container image (lotel:<ver>). The build context is the
# PARENT dir because capped builds from source in its own Docker stage;
# lotel/Dockerfile.dockerignore keeps the context lean.
docker:
    docker build -f Dockerfile -t lotel:{{version}} ..

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
