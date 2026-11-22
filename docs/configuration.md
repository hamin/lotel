# Configuration

lotel has no config file and one command-line flag. Everything else is an environment variable, read once at start. Restart lotel to apply a change.

## Environment variables

| Variable | Default | Meaning |
| --- | --- | --- |
| `LOTEL_ADDR` | `127.0.0.1:8080` | Address the UI listens on, as `host:port`. |
| `LOTEL_OTLP_ADDR` | `127.0.0.1:4318` | Address the OTLP/HTTP receiver listens on, as `host:port`. |
| `LOTEL_DATA_DIR` | `lotel-data` | Data directory. A relative path is relative to the directory you start lotel in. |
| `LOTEL_INGEST_TOKEN` | Random, new on every start | Token every ingest request must carry. Must be at least 16 characters. An empty value counts as unset. |
| `LOTEL_TTL_LOGS` | `7 days` | Max age of the logs table. |
| `LOTEL_TTL_TRACES` | `3 days` | Max age of the traces table. |
| `LOTEL_TTL_METRICS` | `30 days` | Max age of each of the four metrics tables. |
| `LOTEL_MAX_BYTES` | `2GB` | Size cap of each table. |
| `LOTEL_AGENT_SHOW_CONTENT` | Unset (content hidden) | `1`, `true`, `on` or `yes` (any case) shows prompt and tool content in the UI. Anything else keeps it hidden. |
| `LOTEL_MEMORY_LIMIT` | `1GB` | DuckDB `memory_limit`. Caps DuckDB's own buffer pool, not memory used inside the extensions. |
| `LOTEL_THREADS` | Unset (DuckDB's default, one per core) | DuckDB `threads`. Caps DuckDB's worker threads. |
| `LOTEL_EXTENSION_DIR` | Unset (use the extensions built into the binary) | Load the DuckDB extensions from this directory instead. For development. |

More detail on some of these follows.

### Addresses

Both listeners bind to loopback by default, so only your machine can reach them. To accept telemetry from other machines or from a container's host, bind to another address:

```sh
LOTEL_ADDR=0.0.0.0:8080 LOTEL_OTLP_ADDR=0.0.0.0:4318 lotel
```

The ingest token still guards every OTLP request. The UI has no login: anyone who can reach the UI port can read your telemetry, including the ingest token, which the Dashboard and Setup pages print. Only expose the UI port on a network you trust.

`LOTEL_ADDR` must be `host:port` with a numeric port, or lotel exits with an error.

The Docker image sets `LOTEL_ADDR=0.0.0.0:8080`, `LOTEL_OTLP_ADDR=0.0.0.0:4318` and `LOTEL_DATA_DIR=/data`, because a container's loopback address is not reachable from the host.

### Ingest token

Without `LOTEL_INGEST_TOKEN`, lotel generates a 32-character hex token on each start and prints it in the banner. Exporters you set up with an old token stop working after a restart. Set your own token to avoid that:

```sh
export LOTEL_INGEST_TOKEN=$(openssl rand -hex 16)
```

A token shorter than 16 characters stops lotel at start.

### Retention

`LOTEL_TTL_*` and `LOTEL_MAX_BYTES` are written into the tables when lotel first creates them in a data directory. Changing them later does not change existing tables. See [Retention](retention.md) for the accepted formats and how to apply new values.

### Showing agent content

`LOTEL_AGENT_SHOW_CONTENT` only changes what the UI displays. It does not change what is stored. When it is on, the banner prints `lotel: agent content: shown` and the agent pages show a `content: shown` badge. See [Privacy](privacy.md).

### Extensions

lotel embeds three DuckDB extensions: `capped`, `otlp` and `fts`. On start it writes them to `<data dir>/extensions/` and loads them from there. If you set `LOTEL_EXTENSION_DIR`, lotel loads `capped.duckdb_extension`, `fts.duckdb_extension` and `otlp.duckdb_extension` from that directory instead, and exits with an error if one is missing.

## The data directory

```
lotel-data/
  lotel.duckdb      the app database
  store/            the capped store: one directory of Parquet files per table
  extensions/       the DuckDB extensions, written on start
```

Back it up, move it, or delete it while lotel is stopped. Deleting it removes all stored telemetry. lotel creates a fresh one on the next start.

## Command-line flag

```sh
lotel --version     # or -V
```

This prints the lotel version, the git commit it was built from, and the bundled DuckDB and capped versions, then exits without touching the data directory or any port.

## UI look

The settings panel in the top bar switches the UI's palette, light or dark scheme, font and style. These are per-browser choices stored in a cookie, not server settings. Any combination also works as URL parameters, for example `/?pal=riso&scheme=night&font=onest&style=crisp`. See [The UI](ui.md).

## Not read by lotel

The repository's helper scripts read their own variables, such as `LOTEL_TOKEN` and `LOTEL_OTLP_URL` in `scripts/otlp-gen.py` and `LOTEL_REPO` in `scripts/install.sh`. lotel itself ignores them.
