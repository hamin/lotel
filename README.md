# lotel — local OpenTelemetry, one binary

OTLP ingest → capped DuckDB store (age/size/count retention enforced by the
storage layer) → Topcoat UI. Uses the `capped` DuckDB extension (this repo's
sibling project) for storage and retention, and the community `otlp` extension
for the protocol.

Status: M0 — embedded store + status UI. Ingest and signal views are next.

## Build

The storage layer is a prebuilt DuckDB extension shipped inside the binary:

```sh
just fetch-extensions   # copies ../capped/shell/build/release/…/capped.duckdb_extension
cargo run               # http://127.0.0.1:8080
```

## Configuration (env-only, read at boot)

| Var | Default | Meaning |
|---|---|---|
| `LOTEL_ADDR` | `127.0.0.1:8080` | UI bind address |
| `LOTEL_DATA_DIR` | `./lotel-data` | data directory |
| `LOTEL_EXTENSION_DIR` | — | use external `.duckdb_extension` files instead of embedded |
| `LOTEL_TTL_LOGS` / `_TRACES` / `_METRICS` | `7 days` / `3 days` / `30 days` | per-signal age caps |
| `LOTEL_MAX_BYTES` | `2GB` | per-table size cap |
