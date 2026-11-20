# Load testing (M17) — the local ladder, results, and the memory story

Everything here runs **on your machine, by design**. A 10-minute soak on a
shared CI runner buys nothing a nightly laptop run doesn't, and the storm
hammers every endpoint of a live server — awkward at best on a runner that
also has to schedule other people's builds. The scripts use their own ports
(soak 18090/14328, stress 18091/14329) so they can run beside a dev server
on the defaults.

## The ladder

| gate | command | ~time | what it indicts |
| --- | --- | --- | --- |
| units | `cargo test` | 1s | query builders, facet/latency logic, pool concurrency |
| clippy | `cargo clippy --all-targets -- -D warnings` | 3s | lint debt |
| smoke | `just smoke` | 1m | any page that fails to render, ingest + caps + search end to end |
| stress | `just stress` | 1m | behavior **under storm**: request failures, p95 blowups, panics, data loss |
| soak | `just soak` | 11m | behavior **over time**: leaks, fd/thread growth, cap/TTL churn, wedges |
| agent stress | `N=500 just agent-stress` | 1m | the agent pages over a full store: p95, query OOM, empty-on-error, live heap, caps |
| tenants | `scripts/tenant-proof.sh` | 1m | per-tenant isolation: catalogs, tokens, TTLs, double-serve |
| ASan | `cd ../capped/shell && GEN=ninja make asan` | 25m+ | memory safety of capped C++ (UAF, overflow, double-free) |

## How to run

```sh
just stress                      # 30 s storm: 12 threads, ingest + every page
just soak                        # 10 min sustained ingest + 1 s watchdog
SOAK_MINUTES=3 bash scripts/soak.sh        # quick pre-commit pass
RATE=80 SOAK_MINUTES=30 bash scripts/soak.sh   # heavier / longer
```

Both scripts boot their **own** server with tight knobs
(`LOTEL_MAX_BYTES=64MB`, TTLs at *10 minutes*) so seal → evict → TTL-prune
all churn during the run instead of only the happy path.

Tunables (env, per run):

| var | default | meaning |
| --- | --- | --- |
| `SOAK_MINUTES` | 10 | soak duration |
| `RATE` | 40 | generator requests per tick (soak) |
| `LIVE_HEAP_MAX_MB` | 320 | soak gate: vmmap bytes-allocated ceiling |
| `RSS_RUNAWAY_MB` | 5120 | soak gate: RSS ceiling before we call it OOM |
| `RSS_DRIFT_MB` | 200 | soak gate: RSS growth warm-up → final |

## What the gates actually assert

**stress** — fails on *any* of: a failed request out of ~5.8k, p95 read
latency over budget, a panic in the server log, or fewer spans in the store
than the storm should have produced (125,820 survived the M17 run). Every
endpoint is read concurrently *while* ingest floods, which is how it caught
renderers indexing absent columns on query-failure rows — a class of 500
that unit tests and smoke structurally cannot see.

**soak** — a watchdog polls every live surface once per second (pages,
facets, SSE fragments) and samples `ps` for RSS/fd/threads every 30s. It
aborts early on a panic, a dead server, or 10 consecutive failures (wedged).
The final report additionally asserts: zero failures, real cadence, RSS
drift bounded, **live heap** small, caps held (`capped_tables('otel')` bytes
≤ max_bytes + one segment), and all pages render after the storm.

## The macOS RSS story (or: why we gate live heap, not RSS)

The first soak read **4.1 GB RSS over a 9 MB store**. Three experiments
pinned it down:

1. **Is it lotel or capped?** A stock release `duckdb` CLI, plain
   (non-capped) tables, same DDL, same flood: same ~3.7 GB. Not capped,
   not lotel — the churn is inside the otlp extension's Arrow path, and
   DuckDB's `memory_limit` does not cover third-party extension allocations.
2. **Is it a leak?** No. `vmmap` after the storm: `BYTES DIRTY ALLOCATED`
   (live heap) ≈ **39 MB**, RSS ≈ 3.6 GB — 91% fragmentation, allocator
   holding freed pages. macOS `free()` returns memory to libmalloc, not the
   OS; idle RSS is a high-water mark.
3. **Can we force it back?** No. `malloc_zone_pressure_relief` returned
   KERN_SUCCESS every 30 s from a janitor thread and freed nothing; a pure-C
   probe (200k×16 KB malloc/free + relief) reproduced the exact behavior —
   1.2 GB retained, relief a no-op. So gating RSS would be gating Apple's
   allocator, not our code.

Honest gate instead: **live heap < 320 MB** (vmmap bytes-allocated), **RSS
drift < 200 MB** once warmed, **RSS peak < 5 GB** as a runaway/OOM guard.
A real leak still dies on drift + live heap; allocator retention indicts
nobody. (Linux behaves itself: `MADV_DONTNEED` returns pages, so RSS tracks
live heap there.)

Two genuine wins fell out along the way, both in `scripts/otlp-gen.py`:
one keep-alive connection per process (like a real SDK) and a paced warmup
(50 ms between backfill bursts) — flood peak dropped 4.1 → 2.6 GB.

## Concurrency findings (stress's rap sheet)

- **Read pool.** One `Connection` serialized every page render behind one
  mutex: storm p95 was 3.7 s. `Db` now hands out `Connection::try_clone()`
  siblings (cap 8) from the seed connection that ran `LOAD`/`ATTACH` —
  clones share the instance (catalog, attached `otel`, loaded extensions)
  without reopening the file. p95 → **770 ms**. Failed clones are discarded,
  never pooled.
- **Arity panics.** Query-failure paths fabricate single-cell rows for the
  error UI; renderers indexed columns blindly → panics under storm. All
  renderers go through `ui::cell(row, i)` now ("–" for absent columns).
- **OTLP says 202.** Receivers answer `202 Accepted`; a client that demands
  exactly 200 reports false failures. Any 2xx is success.

## Reading the output

- `PASS live heap small … 136 MB allocated / limit 320 MB` — the store is
  healthy; the RSS number you'd see in Activity Monitor is allocator
  retention, not lotel holding data.
- Soak prints the per-table cap table (`capped_tables('otel')`); a row's
  bytes may exceed the cap by roughly one open segment — that is allowed,
  the cap is enforced at seal.
- If the watchdog aborts, `dump()` in the script persists stats to
  `watchdog.json` and the server log tail is printed at the end.

## M17 run record (2026-11-10, M-series Mac, 24 GB)

| gate | result |
| --- | --- |
| units / clippy | 51 green / clean |
| stress | 0 failures / 5801, p50 57 ms, p95 766 ms, 0 panics, 125,820 spans |
| soak 10m | all PASS — peak RSS 2628 MB, live heap 136 MB, fd 19→20, threads flat 57, caps held |
| soak 3m (final tree) | all PASS — live heap 202 MB |
| smoke / tenant-proof | 53 checks / all isolation gates |
| capped ASan | 239 assertions, 3 files, all passed |

## M-agents run record (2026-11-19, Apple M1 Max, 10 cores)

`scripts/agent-stress.sh` replays the committed agent fixtures
(`testdata/agent/`: one Claude Code + opencode capture, one Codex capture) N
times. Each copy gets fresh session, trace, span and opencode run ids and has
its timestamps spread back over the last two days, then the agent pages are
timed against the full store (ports 18092/14330, `LOTEL_MAX_BYTES=256MB`,
`LOTEL_MEMORY_LIMIT=1GB`, release binary). A page showing a query error box,
or an empty session list over a full store, fails the gate.

| gate | result |
| --- | --- |
| units / clippy | 190 green / clean |
| smoke | all checks passed |
| stress (2m, 8 readers, debug binary) | 0 failures / 2295, p50 182 ms, p95 1401 ms (budget 3000), 0 panics, 126,900 spans |
| agent stress N=500 | 617 MB of OTLP JSON in 5 s; 117,003 logs, 2,000 replayed sessions |
| — `/sessions` (default range / all) | p95 330 / 276 ms |
| — one session's timeline | p95 293 ms |
| — `/spend` (default range / all) | p95 92 / 81 ms |
| — `/tools` (default range / all) | p95 102 / 89 ms |
| — memory / caps | live heap 43 MB (RSS 1889 MB, allocator retention); largest table 18 MB of 256 MB |

What the first N=500 run found, and what changed:

- **The sessions rollup ran out of memory** at the 1 GB limit about three
  runs in four (`Out of Memory Error … 957/953 MiB used`) with DuckDB's
  default 10 threads; 8 threads or fewer always passed, so this was a
  query at the edge of the limit, not a leak. The session id was read with
  seven `json_valid` + `json_extract_string` pairs per attribute bag, so
  every row's JSON was parsed fourteen times. `session_expr` now parses each
  bag once (`json_extract_string` over the list of key paths, first
  non-NULL), giving the same result (11,501 groups, 191,500 events, 56,000
  spans on the probe store) at ~220 ms instead of ~480 ms or OOM.
- **A failed query read as "No agent sessions."** `/sessions` and `/spend`
  now show a "Couldn't load …" error box in place of the rows, and
  agent-stress fails on it.
- **A session's timeline was over budget** (p95 510 ms). It had rolled up
  every session to fill its header row; it now rolls up only the session it
  shows. The cross-agent session filter now parses each bag once
  (`list_contains`), behind an exact substring pre-filter: an id made only
  of `[A-Za-z0-9._:-]` appears verbatim in any JSON encoding, so a bag that
  doesn't contain it as text is skipped before parsing, and any other id
  skips the pre-filter (unit-tested with escaped ids). Running the
  timeline's three reads in parallel was tried and made it slower
  (0.5–0.8 s): each DuckDB query already uses every core.

## File map

- `scripts/soak.sh` — watchdog + gates (self-contained; ports 18090/14328)
- `scripts/stress.sh` — storm + latency/loss gates (ports 18091/14329)
- `scripts/agent-stress.sh` — agent fixture replay + agent-page gates (ports 18092/14330)
- `scripts/otlp-gen.py` — the traffic generator: correlated traces→logs→
  metrics, keep-alive, paced warmup (`--warmup M --for S --interval I --rate R`)
- `../capped/Makefile` — `asan` target (Release + `-fsanitize=address`
  across extension sources; `detect_container_overflow=0` is documented there
  and indicts only duckdb core's own vector annotations)
