# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Build

- DuckDB is linked from its official prebuilt static libraries
  (`crates/lotel/duckdb.lock`, sha256-pinned) instead of compiled from
  source, so a clean build takes a couple of minutes instead of 15 or more.
  `--features bundled-duckdb` compiles it from source as before.

## [0.1.0] - 2026-11-22

First release: one static binary that ingests OpenTelemetry over HTTP into a
capped DuckDB store and serves a server-rendered UI. Telemetry stays for as
long as you set, then is forgotten on schedule.

### Ingest and storage

- OTLP/HTTP ingest of traces, logs and metrics (protobuf and JSON) through
  the `otlp` extension's `otlp_serve`, bearer-token authenticated.
- Storage in [`capped`](https://github.com/hamin/duckdb-capped) tables with
  per-signal retention (age and size caps); the `capped`, `otlp` and `fts`
  extensions are embedded in the binary, so there is no network dependency
  at runtime.
- Read queries run on a pool of connections while ingest keeps its own
  writer; `LOTEL_MEMORY_LIMIT` (default 1 GB) and `LOTEL_THREADS` bound
  DuckDB.
- A data directory keeps the retention it was created with. If
  `LOTEL_TTL_*` or `LOTEL_MAX_BYTES` disagree with it, lotel warns at start
  and uses the stored limits.

### UI

- **Dashboard** with signal counts, an error-rate trend and ingest and
  storage status.
- **Logs** with live tail, facets (top values under the current filters)
  and BM25 full-text search.
- **Traces** with a span waterfall, a p95 trend and a span-duration
  histogram.
- **Metrics** charts for gauges and sums, and bucket bars for histograms and
  exponential histograms.
- **Explore**: a query builder (datasets, count/percentile computations,
  filters, breakdowns), a facets sidebar, an events view, and a read-only
  SQL console.
- **Services**: a service map with per-service error and p95 sparklines.
- **Storage**: what each capped table holds and when it forgets.
- **Agents**: `/sessions` with per-session timelines, `/spend`, `/tools` and
  `/setup` for Claude Code, Codex and opencode telemetry, with cross-links to
  the matching logs and traces. Prompt and tool content is masked at render
  unless `LOTEL_AGENT_SHOW_CONTENT=1`.
- A theme panel (palette, type, colour scheme, style) that works without
  JavaScript; fonts are embedded and served by the binary.
- Phone-width layouts.

### Operations

- `lotel --version` prints the version, git SHA and the DuckDB and capped
  versions.
- Prebuilt binaries for macOS (Apple silicon, Intel) and Linux (x86_64,
  arm64) on GitHub Releases, with `SHA256SUMS.txt`.
- Install with `curl -fsSL https://lotel.io/install.sh | sh`, Homebrew
  (`brew install hamin/tap/lotel`), `cargo binstall lotel` or
  `cargo install lotel`. A container image builds with `just docker`.
- A failed query shows an error and leaves the server usable; it never takes
  the UI down.
- Test ladder: unit tests, `just smoke`, and local stress, soak and agent
  stress runs (see [docs/load-testing.md](docs/load-testing.md)).
- `scripts/otlp-gen.py`, a dependency-free OTLP traffic generator.

[0.1.0]: https://github.com/hamin/lotel/releases/tag/v0.1.0
