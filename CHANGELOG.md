# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] - Unreleased

First release: one static binary that ingests OTLP over HTTP into a capped
DuckDB store and serves a server-rendered UI — telemetry that stays, then is
forgotten on schedule. Backfilled from `git log` (M0–M18); each entry cites
the commit(s) it traces to.

### Added

- **M0–M2 — OTLP ingest + Topcoat UI**: ingest via `otlp_serve` into capped
  DuckDB; dashboard, logs explorer with live tail, traces, metrics and
  storage pages (2f77af2).
- **M3 — Trace waterfall**: tree-ordered CSS bars, service colors, error
  outline, span detail card (22cbc9c).
- **M4 — Metric charts**: uPlot vendored into the binary,
  `/api/metrics/series` JSON endpoint, catalog drill-down with header stats
  (45a1287).
- **M5 — Test and CI scaffolding**: README rewrite, MIT license, 9 unit
  tests, `build.rs` extension stubs for CI, smoke suite over `testdata/`,
  GitHub Actions CI, `just` tasks (d4d0c61).
- **Multi-tenancy proof**: `otlp_serve` shown to scale to per-tenant servers
  (isolation, auth, TTL, same-catalog guard); the native Rust receiver is
  dropped from the critical path (ecb7c81).
- **M6 — Distributions**: histogram and exp-histogram metrics as
  server-rendered CSS bucket bars (explicit bounds; exponent-keyed buckets
  with edges rebuilt from base 2^(2^-scale)); the catalog unions all four
  metric shapes (e4e6ab3).
- **M7 — Explore**: Honeycomb-style query builder (datasets, computations
  including p50/p90/p95/p99, AND filters, breakdown) plus a read-only SQL
  console gated before DuckDB; attributes queryable as `@col:key`; every
  token whitelisted, user values bound as parameters (2a1aa0e).
- **M8 — Explore facets sidebar**: top values with count bars recomputed
  under the current filters, click-through drill via server-rendered links,
  per-column stats (count-distinct, min/max) (23d65ea).
- **M9 — Explore Events tab**: the raw matching rows, newest first, over the
  logs / spans / metrics datasets (9a5695e).
- **M10 — Service map + home error sparklines**: `/services` with a
  deterministic layered layout, per-node error-rate sparklines on a shared
  scale, red edges at ≥2% errors; dashboard error-rate sparkline card
  (f60077a).
- **Traffic generator**: `scripts/otlp-gen.py` — stdlib-only OTLP/HTTP
  traces/metrics/logs with `--for/--rate/--interval/--warmup/--once`
  (96da75e).
- **M11–M13 — Freshness seam**: service-map live auto-refresh from a bare
  poll fragment (2fbfd00), unified into one `/api/live?topic=` endpoint and
  a shared `LotelLive` client controller that pauses when the tab is hidden
  (2e1c5db); the logs tail folded in (2211375).
- **M15 — Log facets + BM25 search**: Honeycomb-style facets on the logs
  page, and a "Best matches (BM25)" panel fed by `capped_fts` over sealed
  segments; the `fts` extension is embedded and loaded at boot — no network
  dependency (a623573).
- **M16 — Latency analytics**: amber p95 trend sparklines under each
  service-map node, an overall p95-per-bucket trend on the traces feed, and
  a span-duration histogram with log-ish buckets (366bdff).
- **M17 — Concurrency under load**: reads no longer serialize behind one
  connection — handlers get pooled `try_clone()` siblings (cap 8) while the
  writer keeps its own thread; `LOTEL_MEMORY_LIMIT` (default 1 GB) and
  `LOTEL_THREADS` set DuckDB's leashes at open; local `just stress` and
  `just soak` gates (2a2dc86).
- **Load-testing reference**: `docs/load-testing.md` — gates, tunables, M17
  results, the macOS RSS investigation (1a53e6f).
- **M18 docs**: release-engineering plan with Definition of Done and tasks
  T1–T8 (b90941d); `docs/roadmap.md` with M19 release automation, M20
  multi-tenant control plane and the backlog, plus a corrected README
  status (7ca648f).
- **Gate hardening**: smoke checks the p95 sparkline structure; soak reports
  write amplification (eb5020b).
- **Use-by design reference**: `design/useby` — tokens, `useby.css`, mocks,
  styleguide, `build.py` (45daf08).
- **M18 T1 — Version stamping**: `lotel --version` / `-V` prints
  `lotel <version> (<git-sha> · duckdb 2.0.0 · capped 0.1.0)`; `build.rs`
  stamps the short git SHA (`"unknown"` outside a checkout) and the startup
  banner reuses the same constant (78ad0f2).

### Changed

- **M18-pre — Skin port**: the round-7 bake-off design became the app's
  actual chrome, baked from `design/bakeoff` into the embedded `skin.rs` by
  `scripts/gen-skin.py`, with a server-side theme panel (palette/type/scheme
  as GET links backed by a `lotel_skin` cookie; works with JS off)
  (828b097).
- **Skin: use-by design** (supersedes the bake-off skin): four-axis skin
  with a new style axis (chunky/crisp), retention stickers, the storage
  ribbon laid out on the server clock, thousands separators, chart x-axis
  fixed to epoch seconds; latin woff2 faces embedded in the binary and
  served from `/assets/fonts/` — no Google Fonts at runtime;
  `scripts/gen-skin.py` now sources `design/useby` (0214e5f).
- **M18-pre — Row hover**: one paper wash across the hovered row (text
  promoted to ink, the hairline below turns accent) replaces the per-cell
  inset accent bars that clipped text at cell boundaries (235a664).
- **DuckDB v2.0.0**: `duckdb` crate pinned to `=1.20000.1`; extension paths
  moved to `v2.0.0`; no Rust changes needed (d2b5d2e).
- **Apache-2.0**: relicensed from MIT (added at M5, d4d0c61); `NOTICE`
  lists bundled components with their license texts under `licenses/`
  (2053815).
- **Build paths**: follow capped's move to the repo root
  (`../capped/shell/…` → `../capped/…`) in the justfile, `tenant-proof.sh`
  and docs (f6437c7).
- **Soak live-heap ceiling** raised 220 → 320 MB: still indicts a real
  leak, without indicting DuckDB's warm buffer pool (fcb8df0).

### Fixed

- **M14 — Query failures no longer take the UI down**: a failed query
  propagates with its real cause logged per request instead of panicking
  inside duckdb-rs and poisoning the shared connection until restart;
  regression tests keep the connection usable after a failure (695e51c).
- **M17 — Renderer panics on absent columns**: every table render goes
  through `ui::cell`, which prints "–" for the fabricated single-cell rows
  that query-failure paths return, instead of panicking (2a2dc86).
- **otlp-gen false errors**: OTLP receivers answer 202 Accepted; the
  generator now treats any 2xx as success instead of printing a false error
  per logs push (fcb8df0).
- **Explore first-paint bugs found by the skin port**: a fresh `/query` no
  longer fails its first screen with "unknown dataset" (`QueryState` now
  defaults to logs), and the ungrouped Explore chart draws again (the
  vendored uPlot needs a proper `points.show` function) (828b097).
