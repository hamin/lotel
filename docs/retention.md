# Retention

lotel forgets on a schedule. Every table has a use-by policy: a maximum age and a maximum size. Old data is deleted by the storage layer, the [`capped`](https://github.com/hamin/duckdb-capped) DuckDB extension, not by a cleanup job you have to run.

## The defaults

| Signal | Tables | Max age | Size cap |
| --- | --- | --- | --- |
| Logs | `otlp_logs` | 7 days | 2GB |
| Traces | `otlp_traces` | 3 days | 2GB |
| Metrics | `otlp_metrics_gauge`, `otlp_metrics_sum`, `otlp_metrics_histogram`, `otlp_metrics_exp_histogram` | 30 days | 2GB each |

The size cap applies to each table on its own, not to the store as a whole. With six tables, the store can hold up to six times the cap. `2GB` means 2,000,000,000 bytes.

Rows age by their own timestamp: `time_unix_nano` for logs and metrics, `start_time_unix_nano` for spans. They do not age by when lotel received them.

## How forgetting works

Each table is a set of Parquet files called segments. New rows go into one open segment. When it reaches about 128 MB, 1 million rows or a one hour span, it is sealed into a file and a new open segment starts.

Retention deletes whole sealed files:

- A segment is deleted for age only when every row in it is older than the max age. One recent row keeps its whole segment.
- When a table is over its size cap, the oldest segments are deleted until it fits.
- The open segment is never deleted, so a table can sit a little over its cap until the next seal.
- Nothing that stays is rewritten. Forgetting a segment costs one file delete.

The store applies retention when lotel starts, then again on a background thread every 60 seconds.

The result: data lives at least as long as the max age, and at most about one segment's span longer.

## Change the policies

Set these environment variables before lotel starts:

| Variable | Default | Sets |
| --- | --- | --- |
| `LOTEL_TTL_LOGS` | `7 days` | Max age of the logs table |
| `LOTEL_TTL_TRACES` | `3 days` | Max age of the traces table |
| `LOTEL_TTL_METRICS` | `30 days` | Max age of all four metrics tables |
| `LOTEL_MAX_BYTES` | `2GB` | Size cap of every table |

Ages take a number and a unit: `s`, `m` or `min`, `h`, `d` or `day`, `w` or `week`, with or without a space (`12 hours`, `12h`, `2 weeks`). Sizes take `B`, `KB`, `MB`, `GB` or `TB` (decimal) or `KiB`, `MiB`, `GiB` or `TiB` (binary).

```sh
LOTEL_TTL_LOGS='1 day' LOTEL_TTL_TRACES='12 hours' LOTEL_MAX_BYTES=500MB lotel
```

### Policies are fixed when a table is created

lotel creates its tables on the first start in a data directory and writes the policies into them. On later starts it does not change tables that already exist. If you change `LOTEL_TTL_*` or `LOTEL_MAX_BYTES` for an existing data directory, the stored tables keep their original policy, and lotel prints a warning at start naming each value it ignored.

To apply new values, start lotel with a new data directory:

```sh
LOTEL_DATA_DIR=lotel-data-1d LOTEL_TTL_LOGS='1 day' lotel
```

The Storage page, the stickers in the top bar and the "kept" notes on the agent pages all show the policy the tables actually have.

## The Storage page

`/storage` shows what is on disk and what forgets next.

- **What forgets next**: one row per table. Its segments are laid on the table's retention window. The left edge is the use-by line, the right edge is now. The right column names the oldest sealed segment and how long until it is deleted, for example `segment 3 forgets in 6d 16h`.
- **Tables**: one row per table from `capped_tables()`. It shows segment counts (sealed and open), rows, bytes, the time range, the time column, the age and size caps, index size, bytes written, files waiting for deletion and any maintenance errors.
- **Segments**: one row per segment from `capped_status()`. It shows whether the segment is sealed, its rows and bytes, its oldest and newest timestamp in microseconds, its age and size caps, and whether it has a search index.

The Storage page formats sizes in binary units, so a `2GB` cap reads as `1.9 GB`.

## Other places retention shows up

- The top bar shows a "forgets after" sticker per signal, such as `logs 7d`.
- The Sessions, Spend and Tools pages say how long the data behind them is kept.
- Agent sessions disappear from Sessions once their logs and spans age out. See [Privacy](privacy.md).

## Look at the files yourself

The store is plain Parquet files under `<data dir>/store`, one directory per table. You can read them with any Parquet tool. Treat the directory as sensitive: it holds everything your apps and agents sent.
