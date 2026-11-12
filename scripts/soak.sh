#!/usr/bin/env bash
# Soak (M17, local by design): run the binary under sustained OTLP ingest
# while a watchdog polls every live surface the way a browser does, samples
# process stats, and fails fast on the first sign of trouble — a panic, a
# wedged endpoint, or unbounded RSS/fd growth. Caps and TTLs are set tight so
# seal, eviction and TTL pruning all churn during the soak, not just the
# happy path.
#
#   SOAK_MINUTES=10 RATE=40 ./scripts/soak.sh
#
# Runs on its own ports (18090/14328) so it can live beside a dev server.
# Nothing here belongs in CI: a soak is a nightly-on-your-machine discipline,
# and a 4GB store does not belong on a shared runner.
set -uo pipefail
cd "$(dirname "$0")/.."

BIN="${LOTEL_BIN:-target/debug/lotel}"
[ -x "$BIN" ] || BIN=target/release/lotel
[ -x "$BIN" ] || { echo "no binary; run cargo build first" >&2; exit 1; }

MINUTES="${SOAK_MINUTES:-10}"
RATE="${RATE:-40}"
TOTAL=$((MINUTES * 60))
# macOS's libmalloc keeps freed pages mapped: after a warmup flood the
# process shows gigabytes of RSS that are really ~40 MB of live heap. So
# the soak asserts what is actually ours to guarantee — the live heap stays
# small, RSS stops climbing once ingest stops, and nothing runs away toward
# OOM. (Verified with a C probe: malloc_zone_pressure_relief cannot force
# libmalloc to return these pages, so holding the process to an RSS number
# would only test Apple's allocator.)
LIVE_HEAP_MAX_MB="${LIVE_HEAP_MAX_MB:-320}"  # vmmap bytes-allocated ceiling
RSS_RUNAWAY_MB="${RSS_RUNAWAY_MB:-5120}"     # ceiling before we call it OOM
RSS_DRIFT_MB="${RSS_DRIFT_MB:-200}"  # and its growth over the first minute

TMP="$(mktemp -d)"
LOG="$TMP/lotel.log"
touch "$TMP/.soak-start"
SRV=""; GEN=""; WD=""; WD_RC=0; GEN_RC=0
cleanup() {
    for p in "$WD" "$GEN" "$SRV"; do
        [ -n "$p" ] && kill -9 "$p" 2>/dev/null
    done
    wait 2>/dev/null
}
trap cleanup EXIT

export LOTEL_ADDR=127.0.0.1:18090
export LOTEL_OTLP_ADDR=127.0.0.1:14328
export LOTEL_DATA_DIR="$TMP/store"
export LOTEL_INGEST_TOKEN=lotel-soak-token-0123456789
export LOTEL_MAX_BYTES="${LOTEL_MAX_BYTES:-64MB}"
export LOTEL_TTL_LOGS="${LOTEL_TTL_LOGS:-10 minutes}"
export LOTEL_TTL_TRACES="${LOTEL_TTL_TRACES:-10 minutes}"

"$BIN" >"$LOG" 2>&1 &
SRV=$!
up=""
for _ in $(seq 1 50); do
    if curl -fsS --max-time 2 http://127.0.0.1:18090/api/health >/dev/null 2>&1; then
        up=1; break
    fi
    sleep 0.2
done
[ -n "$up" ] || { echo "FAIL: server did not come up"; tail -5 "$LOG"; exit 1; }

export LOTEL_OTLP_URL="http://127.0.0.1:14328"
python3 scripts/otlp-gen.py --token lotel-soak-token-0123456789 \
    --warmup 60 --for "$TOTAL" --interval 1 --rate "$RATE" >"$TMP/gen.log" 2>&1 &
GEN=$!

# ———————————————————————————————— watchdog ————————————————————————————————
# One request per second round-robin over the live surfaces, process stats
# every 30s, and a periodic stats dump so the summary survives a Ctrl-C.
python3 - "$SRV" "$LOG" "$TMP/watchdog.json" <<'PY' &
import json, os, re, signal, subprocess, sys, time, urllib.request

pid, logpath, outpath = int(sys.argv[1]), sys.argv[2], sys.argv[3]
BASE = "http://127.0.0.1:18090"
PAGES = [
    "/api/health", "/", "/logs", "/logs?range=1h", "/logs?q=cache&range=1h",
    "/logs?severity=ERROR", "/traces", "/traces?service=payments",
    "/services?range=1h", "/metrics", "/query",
    "/api/live?topic=logs&range=1h", "/api/live?topic=map&range=1h",
    "/api/live?topic=err&range=1h",
]

stats = {"requests": 0, "failures": 0, "failed_urls": [], "panics": 0,
         "rss_mb": [], "fds": [], "threads": [], "aborted": None, "seconds": 0}

def dump():
    with open(outpath, "w") as f:
        json.dump(stats, f)

def sample_process():
    try:
        rss = subprocess.run(["ps", "-o", "rss=", "-p", str(pid)],
                             capture_output=True, text=True).stdout.strip()
        if rss:
            stats["rss_mb"].append(round(int(rss) / 1024, 1))
        fds = subprocess.run("lsof -nP -p %d | wc -l" % pid, shell=True,
                             capture_output=True, text=True).stdout.strip()
        if fds:
            stats["fds"].append(int(fds))
        th = subprocess.run("ps -M %d | wc -l" % pid, shell=True,
                            capture_output=True, text=True).stdout.strip()
        if th:
            stats["threads"].append(int(th))
    except Exception:
        pass  # sampling must never be the reason a soak fails

def alive():
    return os.path.exists(f"/proc/{pid}") or subprocess.run(
        ["ps", "-p", str(pid)], capture_output=True).returncode == 0

start, consecutive, i = time.time(), 0, 0
while True:
    url = BASE + PAGES[i % len(PAGES)]
    i += 1
    stats["requests"] += 1
    ok = False
    try:
        with urllib.request.urlopen(url, timeout=5) as r:
            ok = r.status == 200 and len(r.read()) > 0
    except Exception:
        ok = False
    if ok:
        consecutive = 0
    else:
        consecutive += 1
        stats["failures"] += 1
        if len(stats["failed_urls"]) < 25:
            stats["failed_urls"].append(f"t+{int(time.time()-start)}s {url}")
    if stats["requests"] % 30 == 0:
        sample_process()
    try:
        if re.search(rb"panic", open(logpath, "rb").read(), re.I):
            stats["panics"] += 1
    except OSError:
        pass
    stats["seconds"] = int(time.time() - start)
    dump()
    if stats["panics"]:
        stats["aborted"] = "panic in server log"
        dump(); sys.exit(2)
    if not alive():
        stats["aborted"] = "server process died"
        dump(); sys.exit(2)
    if consecutive >= 10:
        stats["aborted"] = "10 consecutive request failures (wedged?)"
        dump(); sys.exit(2)
    time.sleep(1)
PY
WD=$!

echo "soak: ${MINUTES}m · rate ${RATE}/s · pid $SRV · store $TMP/store"
wait "$GEN"; GEN_RC=$?
kill -TERM "$WD" 2>/dev/null
wait "$WD"; WD_RC=$?

python3 - "$TMP/watchdog.json" "$LOG" "$LIVE_HEAP_MAX_MB" "$RSS_RUNAWAY_MB" \
    "$RSS_DRIFT_MB" "$GEN_RC" "$SRV" <<'PY'
import json, re, subprocess, sys, urllib.request, urllib.parse

wd_path, logpath, live_max, runaway, drift_mb, gen_rc, pid = sys.argv[1:8]
stats = json.load(open(wd_path))
fails = []

def check(ok, label, detail=""):
    print(("PASS " if ok else "FAIL ") + label + (f" — {detail}" if detail else ""))
    if not ok:
        fails.append(label)

check(gen_rc == "0", "generator completed", f"exit {gen_rc}")
check(stats.get("aborted") is None, "watchdog never aborted",
      str(stats.get("aborted")))
check(stats["failures"] == 0, "zero request failures",
      f"{stats['failures']} failures over {stats['seconds']}s: "
      f"{stats['failed_urls'][:5]}")
check(stats["requests"] * 2 >= stats["seconds"], "watchdog kept real cadence",
      f"{stats['requests']} requests / {stats['seconds']}s")
check(stats["panics"] == 0, "no panics in server log")

rss = stats["rss_mb"]
if rss:
    warm = rss[: min(60, len(rss) // 4 or 1)]
    base, peak, final = max(warm), max(rss), rss[-1]
    check(peak < float(runaway), "RSS under runaway guard",
          f"peak {peak} MB / guard {runaway} MB")
    check(final - base <= float(drift_mb), "RSS drift bounded",
          f"after warm-up {base} MB -> final {final} MB (budget {drift_mb} MB)")
# The honest ceiling is the *live* heap: vmmap's DefaultMallocZone
# BYTES-DIRTY-ALLOCATED counts pages the allocator still owes someone.
live = None
try:
    out = subprocess.run(["vmmap", "-summary", str(pid)],
                         capture_output=True, text=True, timeout=90).stdout
    for line in out.splitlines():
        if line.startswith("DefaultMallocZone"):
            raw = line.split()[6]  # BYTES DIRTY ALLOCATED
            mult = (1 / 1048576 if raw[-1].isdigit()
                    else {"K": 1 / 1024, "M": 1, "G": 1024}[raw[-1]])
            live = float(raw.rstrip("KMG")) * mult
except Exception:
    pass
check(live is not None and live < float(live_max),
      "live heap small (extra RSS is macOS allocator retention)",
      f"{live:.0f} MB allocated / limit {live_max} MB" if live is not None
      else "vmmap probe failed")
fds = stats["fds"]
if len(fds) >= 2:
    check(max(fds) - fds[0] <= 64, "fd count bounded",
          f"first {fds[0]} -> max {max(fds)}")
th = stats["threads"]
if len(th) >= 2:
    check(max(th) - th[0] <= 32, "thread count bounded",
          f"first {th[0]} -> max {max(th)}")

# caps held under churn: no table may overrun its max_bytes by more than a
# segment's worth (the open segment is allowed past the cap mid-write)
try:
    q = urllib.parse.quote("SELECT table_name, bytes, max_bytes FROM capped_tables('otel')")
    with urllib.request.urlopen(f"http://127.0.0.1:18090/query?mode=sql&sql={q}", timeout=10) as r:
        page = r.read().decode()
    cells = re.findall(r"<td[^>]*>([^<]*)</td>", page)
    rows = [cells[i:i + 3] for i in range(0, len(cells) - len(cells) % 3, 3)]
    ok = bool(rows)
    for name, byt, cap in rows:
        over = int(byt) > int(cap) * 1.5
        ok = ok and not over
        print(f"      {name}: {int(byt)/1e6:.1f} MB / cap {int(cap)/1e6:.0f} MB")
    check(ok, "store respects size caps", f"{len(rows)} tables")
except Exception as e:
    check(False, "store caps queryable", str(e))

# write amplification over the soak (fragments plan T10, informational):
# bytes_written / bytes from capped_tables('otel'), summed over all tables
try:
    q = urllib.parse.quote("SELECT coalesce(sum(bytes), 0), coalesce(sum(bytes_written), 0) FROM capped_tables('otel')")
    with urllib.request.urlopen(f"http://127.0.0.1:18090/query?mode=sql&sql={q}", timeout=10) as r:
        page = r.read().decode()
    sealed, written = [int(x) for x in re.findall(r"<td[^>]*>([^<]*)</td>", page)[:2]]
    print(f"      amplification: bytes_written {written/1e6:.1f} MB / bytes {sealed/1e6:.1f} MB = {written/max(sealed, 1):.2f}x")
except Exception as e:
    check(False, "store amplification queryable", str(e))

# still fully alive after the whole run
try:
    for p in ("/logs", "/traces", "/services"):
        with urllib.request.urlopen("http://127.0.0.1:18090" + p, timeout=10) as r:
            check(r.status == 200, f"post-soak {p} renders")
except Exception as e:
    check(False, "post-soak pages", str(e))

print("soak: FAILED" if fails else "soak: all checks passed")
sys.exit(1 if fails else 0)
PY
RC=$?

# a fresh crash report for our binary during the run is a failure even if the
# watchdog missed the moment it happened
SINCE=$(find ~/Library/Logs/DiagnosticReports -name 'lotel-*.ips' -newer "$TMP/.soak-start" 2>/dev/null | wc -l | tr -d ' ')
[ "$SINCE" = "0" ] || { echo "FAIL: $SINCE lotel crash report(s) written during the soak"; RC=1; }

exit "$RC"
