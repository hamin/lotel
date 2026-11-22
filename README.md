# lotel — local OpenTelemetry, one binary

**See what your coding agents did.** `lotel` is a single Rust binary
that takes their OpenTelemetry: it accepts OTLP/HTTP, stores logs, traces and
metrics in a capped DuckDB data directory, and serves a server-rendered UI on
localhost. One binary; data that forgets. No JVM, no ClickHouse cluster, no
Docker Compose — `./lotel` and you're collecting, with retention enforced by
the storage layer itself rather than a background job you have to remember to
run.

It's built for the local-development and indie-SaaS end of the market: point
your SDK at it, look at your telemetry, and know exactly when it will be
forgotten.

```
OTLP/HTTP :4318 ──▶ otlp extension ──▶ capped DuckDB tables ──▶ parquet segments + manifests
                                                      │
                                           Topcoat UI :8080 (sessions · spend · tools · logs · traces · metrics · storage)
```

## Install

Pick one. Both need the DuckDB extension blobs embedded at build time
(`just build` and `just docker` fetch them first), so the finished artifact
needs nothing at runtime.

**Docker** — build the image locally (the `capped` extension still builds from
source, so there's no published image yet), then run it with a mounted store:

```sh
just docker    # multi-stage build → lotel:0.1.0 (build context is the parent dir)
docker run -p 8080:8080 -p 4318:4318 -v "$PWD/lotel-docker-data:/data" lotel:0.1.0
```

**Binary** — build the release tarball, then install the binary into
`~/.local/bin` (falls back to `/usr/local/bin` with sudo):

```sh
just build     # fetches the real extension blobs, then cargo build --release
just dist      # dist/lotel-0.1.0-<platform>.tar.gz + SHA256SUMS.txt (boot + ingest gate)
scripts/install.sh --local dist/lotel-0.1.0-darwin-arm64.tar.gz
```

> The one-line curl-pipe installer is wired but **not live until the repo is
> public** — the default `LOTEL_REPO` is a placeholder:
>
> ```sh
> # PLACEHOLDER — available once the repo is public (M19). Until then use
> # --local with a tarball from `just dist`, or build the image with just docker.
> LOTEL_REPO=OWNER/lotel curl -fsSL https://raw.githubusercontent.com/OWNER/lotel/main/scripts/install.sh | sh
> ```

## Run

```sh
lotel                     # or: ./target/release/lotel, or `just run` for a dev build
# UI:        http://127.0.0.1:8080
# OTLP:      http://127.0.0.1:4318  (bearer token printed in the startup banner)
```

Copy the bearer token the banner prints; every exporter below needs it.

## Ingest

Point an SDK at it — protocol, endpoint and the token from the banner:

```sh
export OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf
export OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:4318
export OTEL_EXPORTER_OTLP_HEADERS="Authorization=Bearer <token printed at boot>"
```

No SDK handy? The bundled generator streams a realistic `api-gateway →
checkout → payments` topology — real traces, linked error logs, and metrics
that move over time — so charts have shape immediately:

```sh
scripts/otlp-gen.py --once            # one burst, then exit
scripts/otlp-gen.py --for 60          # stream for 60s (token from $LOTEL_TOKEN or --token)
```

## Agents

lotel has pages built for coding-agent telemetry, grouped under **Agents** in
the sidebar. Claude Code, Codex and opencode all speak OpenTelemetry; point
one at lotel and its sessions, spend and tool calls show up within seconds.

- **Setup** (`/setup`) — copy-paste blocks with this instance's endpoint and
  token already filled in: a shell `export` block for Claude Code (the same
  variables work in the `env` map of `~/.claude/settings.json`), an `[otel]`
  section for `~/.codex/config.toml`, and the same Codex settings as `-c`
  overrides for one `codex exec` run.
- **Sessions** (`/sessions`) — one row per agent session across all agents:
  agent, events, spans, cost, tokens and when it was last seen, with links to
  its logs and traces. Open a session for its timeline:
  every event in order, repeats folded, each with its attribute chips.
- **Spend** (`/spend`) — cost and tokens per day, model and agent, from the
  agents' own cost metrics (cumulative counters are never summed), with event
  rows filling in days a metric didn't cover.
- **Tools** (`/tools`) — per tool: calls, failures, rejections, p50/p95
  duration, each count linking to the spans and logs behind it.

Logs and traces link to each other wherever the other side exists: a log row
links to its trace when that trace is still stored, and a trace or span shows
how many logs carry its id.

For Claude Code, the short version (`/setup` prints it with your token):

```sh
# in the shell you run claude from
export CLAUDE_CODE_ENABLE_TELEMETRY=1
export CLAUDE_CODE_ENHANCED_TELEMETRY_BETA=1
export OTEL_LOGS_EXPORTER=otlp
export OTEL_METRICS_EXPORTER=otlp
export OTEL_TRACES_EXPORTER=otlp
export OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf
export OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:4318
export OTEL_EXPORTER_OTLP_HEADERS="Authorization=Bearer <token lotel prints>"
```

**Privacy: hide, then forget.** Some agents send prompt or tool text (opencode,
for example, logs its full command line, prompt included). lotel stores what
it receives, but every page masks that content when it renders: prompts,
tool input and output, and command lines show as `[REDACTED]` everywhere the
UI shows stored text. The rows themselves are dropped at the
retention line like everything else. To see the content on your own machine,
start lotel with `LOTEL_AGENT_SHOW_CONTENT=1`. Either way, treat the data
directory as sensitive.

Known gap: Codex currently sends its log records with a zero timestamp, which
the store reads as "no time", so Codex logs don't list on `/logs` yet (its
traces and sessions do). The fix belongs in the OTLP extension.

## What you get

- **All three signals.** Logs, traces and metrics over OTLP/HTTP
  (`/v1/logs`, `/v1/traces`, `/v1/metrics`), bearer-token authenticated.
- **Retention you can reason about.** Every table is capped: logs default to
  7 days, traces to 3, metrics to 30, each with a per-table size cap. The caps
  live in the storage layer, so old data physically cannot accumulate.
- **Logs explorer** with text search, service/severity/time-range filters and
  a live tail that updates in place. JSON log bodies (including Claude Code and
  opencode event rows) render as a readable message plus `key=value` attribute
  chips; click a chip to filter on it.
- **Trace waterfall** — spans tree-ordered and positioned by start/duration,
  service-colored, errored spans outlined, click-through span detail with
  attributes.
- **One filter bar, shared by logs and traces.** Write `key=value`,
  `key!=value` or `key~text`; `resource.` and `body.` prefixes pick the source
  column, and the traces bar adds span name, status and a minimum duration. A
  line that won't parse is flagged inline, never a crash. The filter box spells
  the syntax back to you as its placeholder.

  | write | matches |
  | --- | --- |
  | `key=value` | rows whose attribute `key` is exactly `value` |
  | `key!=value` | rows that carry `key` with a different value |
  | `key~text` | rows whose `key` contains `text`, case-insensitive; `%` and `_` act as wildcards |
  | `resource.key=value` | the same against `resource_attributes` |
  | `body.key=value` | the same against the parsed JSON body (logs only) |

  One filter per line; they AND together. `!=` and `~` skip rows that lack
  the key, so filters only ever narrow. Explore's builder names the same
  reads `@col:key`: its `@log_attributes:k` / `@span_attributes:k` are the
  bar's bare `k`, its `@resource_attributes:k` the bar's `resource.k`.
  Typical lines for agent telemetry: `tool_name=Bash`, `success=false`,
  `resource.service.name=opencode` — or straight as a URL:
  `/logs?f=tool_name%3DBash&f=success%3Dfalse`. An empty list under a time
  range narrower than all offers a link to the same query at `range=all`.
- **Metric charts** — uPlot charts embedded in the binary (no CDN), per
  `(shape, name, service)`, plus server-rendered histogram distributions.
- **Storage truth** — every segment on disk: rows, bytes, time window, TTL,
  seal state. The store is just parquet files; inspect it with plain DuckDB.
- **First-run demo data** so the UI has a heartbeat before you send anything.
- **Release artifacts.** `lotel --version` stamps the git SHA, duckdb and
  capped versions; `just dist` builds a checksummed, boot-and-ingest-gated
  tarball; `scripts/install.sh` installs it offline; `just docker` builds a
  non-root runtime image.

## How it works

Two DuckDB extensions, embedded into the binary at compile time and
materialized to `<data>/extensions/` at boot:

- [`capped`](https://github.com/hamin/duckdb-capped) provides
  the storage layer: tables
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

All operator-facing settings are environment variables read once at boot.
The first ten are read in
[`crates/lotel/src/config.rs`](crates/lotel/src/config.rs); `LOTEL_MEMORY_LIMIT`
and `LOTEL_THREADS` are read in
[`crates/lotel/src/db.rs`](crates/lotel/src/db.rs). The defaults below are the
literals in those files.

| Var | Default | Meaning |
|---|---|---|
| `LOTEL_ADDR` | `127.0.0.1:8080` | UI/API bind address |
| `LOTEL_OTLP_ADDR` | `127.0.0.1:4318` | OTLP/HTTP listen address |
| `LOTEL_DATA_DIR` | `lotel-data` (relative to cwd) | data directory |
| `LOTEL_INGEST_TOKEN` | random, per boot | bearer token required on every ingest request |
| `LOTEL_EXTENSION_DIR` | unset | load `.duckdb_extension` files from here instead of the embedded ones |
| `LOTEL_TTL_LOGS` | `7 days` | per-signal age cap for logs |
| `LOTEL_TTL_TRACES` | `3 days` | per-signal age cap for traces |
| `LOTEL_TTL_METRICS` | `30 days` | per-signal age cap for metrics |
| `LOTEL_MAX_BYTES` | `2GB` | per-table size cap |
| `LOTEL_AGENT_SHOW_CONTENT` | unset (content masked) | `1`/`true`/`on`/`yes` shows agents' prompt and tool text instead of masking it at render |
| `LOTEL_MEMORY_LIMIT` | `1GB` | DuckDB `memory_limit` — a leash on DuckDB's own buffer-pool accounting (not third-party extension allocations) |
| `LOTEL_THREADS` | unset (DuckDB's own default: one per core) | `SET threads` — cap DuckDB's worker threads |

## Building

```sh
just fetch-extensions   # copies capped + otlp extension files in for embedding
cargo build --release   # target/release/lotel
cargo test              # unit tests (pure logic; no extensions needed)
just smoke              # boots the binary, ingests testdata/, checks every page
just stress             # 30s storm: concurrent ingest + every endpoint, p95 gates
just soak               # 10 min sustained: watchdog over all pages, leak/fd/heap gates
```

`just fetch-extensions` expects the sibling `capped` repo built
(`cd ../capped && GEN=ninja make release`) and the community `otlp`
extension installed for the pinned DuckDB version. Both are then
`include_bytes!`-embedded, so the shipped binary needs nothing at runtime.
`just dist` refuses to run against stubbed (zero-byte) blobs and boots the
tarball's own binary as an ingest gate, so a release can't silently ship dead.

## The test ladder

| gate | command | what it proves |
| --- | --- | --- |
| units | `cargo test` | query builders, facet/latency logic, pool concurrency |
| smoke | `just smoke` | every page renders, ingest + caps + search work end to end |
| stress | `just stress` | 12 threads x 30s storm: zero failures, p95 under budget, no panics |
| soak | `just soak` | 10 min sustained ingest: live heap bounded, no fd/thread growth, caps held |
| agent stress | `N=500 just agent-stress` | agent pages over 500 replayed fixture copies: p95 under 500 ms, no query errors, live heap and caps held |

Soak and stress are deliberately **local**, not CI: they run on their own
ports beside a dev server, and a 10-minute soak on a shared runner buys
nothing a nightly laptop run doesn't. The DuckDB/extension side has its own
memory gate: `cd ../capped && GEN=ninja make asan` builds Release with
AddressSanitizer and runs the whole sqllogictest suite inside it. Full
reference — gates, tunables, results, and the macOS allocator investigation —
in [`docs/load-testing.md`](docs/load-testing.md).

One honest caveat, measured rather than guessed: on macOS, RSS after a
traffic flood reads high (gigabytes) while the *live heap* is ~40 MB —
libmalloc retains freed pages and `malloc_zone_pressure_relief` cannot force
them back (verified with a C probe). That is allocator policy, not lotel
holding data, so the soak gates the live heap (vmmap bytes-allocated) plus
RSS drift, instead of pretending an RSS ceiling is meaningful there.

## Status

lotel 0.1.0 has OTLP ingest into a capped store, a dashboard, a logs explorer
with live tail, facets and BM25 search, a trace waterfall, metric charts,
latency analytics, a service map, a storage view, a theme panel, and the
agent pages (`/sessions` with per-session timelines, `/spend`, `/tools`,
`/setup`) with log and trace cross-links and render-time content masking.

## License

Apache-2.0 — see [LICENSE](LICENSE) and [NOTICE](NOTICE). NOTICE names the
bundled third-party components: DuckDB, the embedded `otlp` and `fts`
extensions (MIT), uPlot (MIT), the five UI fonts (SIL OFL 1.1, full texts
under
[`crates/lotel/assets/fonts/LICENSES/`](crates/lotel/assets/fonts/LICENSES/)),
and [`capped`](https://github.com/hamin/duckdb-capped) (Apache-2.0). The Rust crates compiled into the binary ship
their own licenses as `THIRD-PARTY-LICENSES` next to `LICENSE` in the tarball
and image.
