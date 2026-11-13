# lotel — local OpenTelemetry, one binary

`lotel` is a self-contained OpenTelemetry backend: a single Rust binary that
accepts OTLP/HTTP, stores logs, traces and metrics in a DuckDB data directory
with hard age-and-size caps, and serves a browsable UI on localhost. No JVM,
no ClickHouse cluster, no Docker Compose — `./lotel` and you're collecting.

It's built for the local-development and indie-SaaS end of the market: point
your SDK at it, look at your telemetry, and know exactly when it will be
forgotten — retention isn't a background job you have to remember to run, it's
enforced by the storage layer itself.

```
OTLP/HTTP :4318 ──▶ otlp extension ──▶ capped DuckDB tables ──▶ parquet segments + manifests
                                                     │
                                          Topcoat UI :8080 (dashboard · logs · traces · metrics · storage)
```

## What you get

- **All three signals.** Logs, traces and metrics over OTLP/HTTP
  (`/v1/logs`, `/v1/traces`, `/v1/metrics`), bearer-token authenticated.
- **Retention you can reason about.** Every table is capped: logs default to
  7 days, traces to 3, metrics to 30, each with a per-table size cap. The caps
  live in the storage layer, so old data physically cannot accumulate.
- **Logs explorer** with text search, service/severity/time-range filters and
  a live tail that updates in place.
- **Trace waterfall** — spans tree-ordered and positioned by start/duration,
  service-colored, errored spans outlined, click-through span detail with
  attributes.
- **Metric charts** — uPlot charts embedded in the binary (no CDN), per
  `(shape, name, service)`.
- **Storage truth** — every segment on disk: rows, bytes, time window, TTL,
  seal state. The store is just parquet files; inspect it with plain DuckDB.
- **First-run demo data** so the UI has a heartbeat before you send anything.

## Quick start

```sh
cargo run                 # or: just run (fetches extensions first)
# UI:        http://127.0.0.1:8080
# OTLP:      http://127.0.0.1:4318  (bearer token printed at startup)
```

Then point a collector or SDK at it:

```sh
export OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf
export OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:4318
export OTEL_EXPORTER_OTLP_HEADERS="Authorization=Bearer <token printed at boot>"
```

## How it works

Two DuckDB extensions, embedded into the binary at compile time and
materialized to `<data>/extensions/` at boot:

- [`capped`](../capped) (sibling project) provides the storage layer: tables
  are sealed parquet segments plus one open segment, `WITH (time_col, ttl,
  max_bytes)` policies enforce retention at the segment level, and
  `capped_tables()` / `capped_status()` expose the truth.
- the community [`otlp`](https://github.com/smithclay/duckdb-otlp) extension
  provides `otlp_serve()`: an OTLP/HTTP receiver that appends directly into
  the capped tables (lotel pre-creates the six canonical `otlp_*` tables with
  cap policies so the receiver never creates an uncapped one).

The UI is [Topcoat](https://github.com/tokio-rs/topcoat) server-rendered
views; the live tail and charts poll small bare fragment/JSON endpoints.

## UI & theming

The settings panel in the top bar switches skins along four axes — no
scripting, just links that rewrite these query parameters:

| axis | param | values | default |
| --- | --- | --- | --- |
| palette | `pal` | `lime` · `sherbet` · `riso` · `mint` | `lime` |
| colourway | `scheme` | `day` · `night` · `system` (shown "auto") | `day` |
| type | `font` | `recursive` · `figtree` · `onest` | `recursive` |
| structure | `style` | `chunky` · `crisp` | `chunky` |

So any look is a deep link: `/?pal=riso&scheme=night&font=onest&style=crisp`.
Query parameters beat the settings cookie, the cookie beats the defaults, and
unknown values fall through. The latin woff2 faces for all three pairings are
embedded in the binary and served from `/assets/fonts/` — no Google Fonts or
any other runtime network dependency.

The look is owned by `design/useby` (its `build.py` generates the token
blocks; `useby.css` is the component sheet), and `src/skin.rs` is generated
from it — never hand-edit. Restyle flow:

```sh
# edit design/useby/build.py or useby.css
python3 design/useby/build.py --check   # WCAG contrast gate; must be 0 failing pairs
python3 scripts/gen-skin.py             # regenerates src/skin.rs from design/useby
cargo build                             # rebuild
```

## Configuration (env-only, read at boot)

| Var | Default | Meaning |
|---|---|---|
| `LOTEL_ADDR` | `127.0.0.1:8080` | UI bind address |
| `LOTEL_OTLP_ADDR` | `127.0.0.1:4318` | OTLP/HTTP listen address |
| `LOTEL_DATA_DIR` | `./lotel-data` | data directory |
| `LOTEL_INGEST_TOKEN` | random per boot | bearer token for ingest (≥16 chars) |
| `LOTEL_EXTENSION_DIR` | — | use external `.duckdb_extension` files instead of embedded |
| `LOTEL_TTL_LOGS` / `_TRACES` / `_METRICS` | `7 days` / `3 days` / `30 days` | per-signal age caps |
| `LOTEL_MAX_BYTES` | `2GB` | per-table size cap |

## Building

```sh
just fetch-extensions   # copies capped + otlp extension files in for embedding
cargo build --release   # target/release/lotel
cargo test              # unit tests (pure logic; no extensions needed)
just smoke              # boots the binary, ingests testdata/, checks every page
just stress             # 30s storm: concurrent ingest + every endpoint, p95 gates
just soak               # 10 min sustained: watchdog over all pages, leak/fd/heap gates
```

`just fetch-extensions` expects the sibling `capped` shell built
(`cd ../capped/shell && GEN=ninja make release`) and the community `otlp`
extension installed for the pinned DuckDB version. Both are then
`include_bytes!`-embedded, so the shipped binary needs nothing at runtime.

## The test ladder

| gate | command | what it proves |
| --- | --- | --- |
| units | `cargo test` | query builders, facet/latency logic, pool concurrency |
| smoke | `just smoke` | every page renders, ingest + caps + search work end to end |
| stress | `just stress` | 12 threads x 30s storm: zero failures, p95 under budget, no panics |
| soak | `just soak` | 10 min sustained ingest: live heap bounded, no fd/thread growth, caps held |
| tenants | `scripts/tenant-proof.sh` | per-tenant isolation: catalogs, tokens, TTLs |

Soak and stress are deliberately **local**, not CI: they run on their own
ports beside a dev server, and a 10-minute soak on a shared runner buys
nothing a nightly laptop run doesn't. The DuckDB/extension side has its own
memory gate: `cd ../capped/shell && GEN=ninja make asan` builds Release with
AddressSanitizer and runs the whole sqllogictest suite inside it. Full
reference — gates, tunables, results, and the macOS allocator investigation —
in [`docs/load-testing.md`](docs/load-testing.md).

One honest caveat, measured rather than guessed: on macOS, RSS after a
traffic flood reads high (gigabytes) while the *live heap* is ~40 MB —
libmalloc retains freed pages and `malloc_zone_pressure_relief` cannot force
them back (verified with a C probe). That is allocator policy, not lotel
holding data, so the soak gates the live heap (vmmap bytes-allocated) plus
RSS drift, instead of pretending an RSS ceiling is meaningful there.

## Multi-tenancy (hosted flavor)

The `otlp` extension keys each server by its write target (`IngestTargetKey`
= catalog + schema) and enforces **one server per target**, with its own token
and listen URI. That maps cleanly onto multi-tenancy without a custom
receiver: **one tenant = one attached capped catalog + one `otlp_serve`
instance**, and isolation is free.

`scripts/tenant-proof.sh` proves this against the real extensions: two
tenants in one process, each landing rows only in its own catalog, one
tenant's token rejected (401) at the other's endpoint, and per-tenant TTLs
(1 day vs 30 days) honored — plus the extension refusing a second server on a
taken catalog.

The hosted control plane then just needs to: mint a tenant (attach a capped
catalog + start a serve with a fresh token/port), and route each tenant's
token to its listener's port (a reverse proxy on OTLP/HTTP, or per-tenant
endpoints). DuckDB is single-writer, so at high tenant counts you shard
tenants across processes rather than running thousands of listeners in one.

## Status & roadmap

Shipped through M17 + the certified skin (M18-pre): OTLP ingest → capped
store, dashboard, logs explorer with live tail and facets + BM25, trace
waterfall, metric charts, latency analytics (p95 trends, duration
histogram), service map with sparklines, storage view, theme/skin panel,
and the local test ladder (smoke / stress / soak / tenant-proof).

M18 is release engineering (`docs/M18-release-plan.md`); M19 (public
release automation), M20 (multi-tenant control plane) and the backlog are
in [docs/roadmap.md](docs/roadmap.md).

## License

MIT — see [LICENSE](LICENSE).
