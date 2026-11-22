# syntax=docker/dockerfile:1
# Multi-stage release image for lotel.
#
# Build from the PARENT directory — capped has no published release yet, so
# its source tree must be visible in the build context:
#
#     just docker    # = docker build -f lotel/Dockerfile -t lotel:<ver> ..
#
# The context ships only source: lotel/Dockerfile.dockerignore drops target/,
# build/, dist/, lotel-data, node_modules and the .git internals cmake does
# not need.
#
# The binary embeds its extension blobs at compile time (include_bytes! in
# db.rs), so this image gets linux blobs: capped from its own builder stage,
# otlp/fts pinned + sha256-verified via extensions.lock (scripts/fetch-
# extensions.sh). Host-built macOS blobs are excluded from the context, so a
# build.rs stub or a platform mix-up cannot ride in unnoticed.

# ---- capped builder ------------------------------------------------------
# Own stage ⇒ its layer cache is keyed on capped/ contents only: lotel code
# edits never rebuild DuckDB's C++. rust is pinned to bookworm so the glibc
# matches the runtime stage.
FROM rust:1-slim-bookworm AS capped-builder
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential cmake ninja-build python3 git clang \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /build/capped
COPY capped/ ./
# GEN=ninja make release: cmake configure + ninja build of the vendored
# duckdb checkout; the Makefile pins OVERRIDE_GIT_DESCRIBE=v2.0.0 so the
# stamp matches the official 2.0.0 blobs (duckdb's CMake falls back cleanly
# without .git). cargo registry and cmake/ninja state are cache-mounted, so
# editing capped builds incrementally instead of recompiling DuckDB whole.
#
# CC/CXX=clang: Debian's GCC 12 rejects named-return unique_ptr conversions
# in capped's own capped_extension.cpp (the "could not convert 'catalog' /
# 'bind_data'" errors) while clang — the toolchain that builds the certified
# macOS blobs — compiles it. If the cache-mounted build dir was configured
# with a different compiler, cmake would refuse, so drop it first.
# /build/capped/{build,target} are cache mounts (invisible to COPY --from),
# so export the blob into a real layer directory.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/build/capped/target \
    --mount=type=cache,target=/build/capped/build \
    if [ -f build/release/CMakeCache.txt ] && ! grep -q "CMAKE_CXX_COMPILER:FILEPATH=/usr/bin/clang++" build/release/CMakeCache.txt; then rm -rf build/release; fi; \
    CC=clang CXX=clang++ GEN=ninja make release \
    && mkdir -p /out \
    && cp build/release/extension/capped/capped.duckdb_extension /out/

# ---- lotel builder --------------------------------------------------------
FROM rust:1-slim-bookworm AS builder
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential curl git ca-certificates \
    && rm -rf /var/lib/apt/lists/*
# THIRD-PARTY-LICENSES generator (Job D: statically linked crates must ship
# their licenses). Same pinned cargo-about as scripts/dist.sh uses, generated
# here rather than copied from dist/ — dist/ is build-host-local and excluded
# from the context; the builder regenerates it from the copied Cargo.lock.
# --features cli: 0.9.3 gates its binary behind that feature and cargo install
# otherwise "succeeds" while installing nothing; the --version check makes a
# silent skip impossible. Own layer ⇒ cached across lotel source edits.
RUN cargo install cargo-about --version 0.9.3 --locked --features cli \
    && cargo about --version
WORKDIR /build/lotel
# Extension blobs land before the full source copy: this layer (≈70 MB of
# downloads) re-runs only when the fetch script, the lock, or the capped
# blob changes, not on every lotel edit. COPY lotel/ later cannot clobber
# them — crates/lotel/extensions/ is not in the context (see .dockerignore).
COPY --from=capped-builder /out/capped.duckdb_extension /tmp/capped-blob/capped.duckdb_extension
COPY lotel/scripts/fetch-extensions.sh scripts/fetch-extensions.sh
COPY lotel/extensions.lock extensions.lock
RUN CAPPED_EXT=/tmp/capped-blob/capped.duckdb_extension \
    ./scripts/fetch-extensions.sh
COPY lotel/ ./
# Stub gate: a zero-byte blob would build
# fine and ingest nothing. The fetch layer already refuses to leave stubs
# behind; this is the belt before cargo runs.
RUN set -eu; for b in capped otlp fts; do \
        f="crates/lotel/extensions/$b.duckdb_extension"; \
        [ -s "$f" ] || { echo "lotel-builder: $f is missing or a 0-byte stub" >&2; exit 1; }; \
    done
# target/ is a cache mount (invisible to COPY --from) ⇒ export artifacts to
# /out. /out/lotel --version proves the binary at least execs on this base.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/build/lotel/target \
    cargo about generate about.hbs -o THIRD-PARTY-LICENSES \
    && cargo build --release \
    && mkdir -p /out \
    && cp target/release/lotel /out/lotel \
    && cp THIRD-PARTY-LICENSES /out/THIRD-PARTY-LICENSES \
    && /out/lotel --version

# ---- runtime ---------------------------------------------------------------
# Binary + the licensing set (Apache-2.0 relicense, Job D): LICENSE, NOTICE,
# the licenses/ texts NOTICE points at, and the generated crate licenses.
FROM debian:bookworm-slim AS runtime
# libstdc++6: the binary statically links bundled DuckDB C++ but still needs
# the system libstdc++ to exec. uid 1000 keeps bind-mounted /data writable
# from typical Linux hosts; named volumes inherit the /data ownership below.
RUN apt-get update && apt-get install -y --no-install-recommends libstdc++6 \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system --gid 1000 lotel \
    && useradd --system --uid 1000 --gid lotel --home-dir /nonexistent --shell /usr/sbin/nologin lotel
COPY --from=builder /out/lotel /usr/local/bin/lotel
COPY --from=builder /out/THIRD-PARTY-LICENSES /opt/lotel/THIRD-PARTY-LICENSES
COPY lotel/LICENSE lotel/NOTICE /opt/lotel/
COPY lotel/licenses /opt/lotel/licenses
RUN mkdir -p /data && chown lotel:lotel /data
# Inside the container the UI + OTLP listeners must answer on all interfaces
# (config.rs defaults to 127.0.0.1, which is unreachable from the host).
ENV LOTEL_DATA_DIR=/data \
    LOTEL_ADDR=0.0.0.0:8080 \
    LOTEL_OTLP_ADDR=0.0.0.0:4318
EXPOSE 8080 4318
VOLUME /data
USER lotel
ENTRYPOINT ["lotel"]
