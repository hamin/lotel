#!/usr/bin/env bash
# Stress (M17, local by design): a short, sharp storm. Several generators
# burst ingest while a fleet of readers hammers the surfaces that have to
# survive concurrency — aggregate SQL, facets, BM25, live topics, the query
# builder. Then we re-check every page and the store. Zero failures is the
# bar: this is the gate that would have caught the query-wedging and
# concurrent-transaction bugs the hard way.
#
#   STRESS_MINUTES=2 CONC=8 ./scripts/stress.sh
#
# Own ports (18091/14329), throwaway store; can run beside a dev server.
set -uo pipefail
cd "$(dirname "$0")/.."

BIN="${LOTEL_BIN:-target/debug/lotel}"
[ -x "$BIN" ] || BIN=target/release/lotel
[ -x "$BIN" ] || { echo "no binary; run cargo build first" >&2; exit 1; }

MINUTES="${STRESS_MINUTES:-2}"
TOTAL=$((MINUTES * 60))
CONC="${CONC:-8}"
GENS="${GENS:-3}"
GEN_RATE="${GEN_RATE:-60}"          # traces/sec per generator
P95_BUDGET_MS="${P95_BUDGET_MS:-3000}"

TMP="$(mktemp -d)"
LOG="$TMP/lotel.log"
touch "$TMP/.stress-start"
SRV=""; PIDS=(); RC=0
cleanup() {
    [ -n "$SRV" ] && kill -9 "$SRV" 2>/dev/null
    for p in "${PIDS[@]:-}"; do kill -9 "$p" 2>/dev/null; done
    wait 2>/dev/null
}
trap cleanup EXIT

export LOTEL_ADDR=127.0.0.1:18091
export LOTEL_OTLP_ADDR=127.0.0.1:14329
export LOTEL_DATA_DIR="$TMP/store"
export LOTEL_INGEST_TOKEN=lotel-stress-token-0123456789
export LOTEL_MAX_BYTES="${LOTEL_MAX_BYTES:-256MB}"

"$BIN" >"$LOG" 2>&1 &
SRV=$!
up=""
for _ in $(seq 1 50); do
    if curl -fsS --max-time 2 http://127.0.0.1:18091/api/health >/dev/null 2>&1; then
        up=1; break
    fi
    sleep 0.2
done
[ -n "$up" ] || { echo "FAIL: server did not come up"; tail -5 "$LOG"; exit 1; }

# ———————————————————————————————— ingest storm —————————————————————————————
export LOTEL_OTLP_URL="http://127.0.0.1:14329"
for i in $(seq 1 "$GENS"); do
    python3 scripts/otlp-gen.py --token lotel-stress-token-0123456789 \
        --warmup 30 --for "$TOTAL" --interval 1 --rate "$GEN_RATE" \
        >"$TMP/gen$i.log" 2>&1 &
    PIDS+=($!)
done

# ————————————————————————————————— read storm ———————————————————————————————
# Weighted to the expensive paths: each reader loops over aggregates,
# search and fragments, timing every request.
python3 - "$TMP/readstorm.json" "$CONC" "$TOTAL" <<'PY' &
import json, random, statistics, sys, threading, time, urllib.parse, urllib.request

out_path, conc, total = sys.argv[1], int(sys.argv[2]), float(sys.argv[3])
BASE = "http://127.0.0.1:18091"

SQL = [
    "SELECT quantile_cont(duration_time_unix_nano/1e6, 0.95) FROM otel.main.otlp_traces",
    "SELECT service_name, count(*) FROM otel.main.otlp_logs GROUP BY 1 ORDER BY 2 DESC",
    "SELECT time_bucket(INTERVAL '1 minute', start_time_unix_nano) b, count(*) "
    "FROM otel.main.otlp_traces GROUP BY 1 ORDER BY 1",
]
PATHS = [
    "/logs", "/logs?range=1h", "/logs?severity=ERROR",
    "/logs?q=" + urllib.parse.quote("cache miss"), "/logs?q=payment&range=1h",
    "/traces", "/traces?service=payments", "/services?range=1h", "/metrics",
    "/storage", "/query",
    "/api/live?topic=logs&range=1h", "/api/live?topic=map&range=1h",
    "/api/live?topic=err&range=1h",
]

lock = threading.Lock()
lat, fails = [], []

def one(url):
    t0 = time.time()
    try:
        with urllib.request.urlopen(BASE + url, timeout=10) as r:
            body = r.read()
        dt = (time.time() - t0) * 1000
        with lock:
            lat.append(dt)
            if r.status != 200 or not body:
                fails.append(f"{r.status} {url}")
    except Exception as e:
        with lock:
            fails.append(f"{type(e).__name__} {url}")

def reader():
    end = time.time() + total
    while time.time() < end:
        r = random.random()
        if r < 0.25:
            sql = urllib.parse.quote(random.choice(SQL))
            one(f"/query?mode=sql&sql={sql}")
        elif r < 0.45:
            one('/api/query/chart?dataset=logs&measures=%5B%22count%22%5D&range=1h')
        else:
            one(random.choice(PATHS))

threads = [threading.Thread(target=reader, daemon=True) for _ in range(conc)]
t0 = time.time()
for t in threads:
    t.start()
for t in threads:
    t.join()

report = {
    "requests": len(lat) + len(fails),
    "failures": len(fails),
    "failed": fails[:20],
    "p50_ms": round(statistics.median(lat), 1) if lat else None,
    "p95_ms": round(statistics.quantiles(lat, n=20)[-1], 1) if len(lat) > 20 else None,
    "max_ms": round(max(lat), 1) if lat else None,
    "rps": round(len(lat) / max(1.0, time.time() - t0), 1),
}
with open(out_path, "w") as f:
    json.dump(report, f)
PY
READER=$!
PIDS+=("$READER")

echo "stress: ${MINUTES}m · ${GENS}×${GEN_RATE}/s ingest · ${CONC} readers · pid $SRV"
GEN_FAILS=0
for p in "${PIDS[@]:-}"; do
    wait "$p" || GEN_FAILS=$((GEN_FAILS + 1))
done
sleep 2   # let in-flight ingest settle before the post-storm checks

python3 - "$TMP/readstorm.json" "$P95_BUDGET_MS" "$GEN_FAILS" "$LOG" <<'PY'
import json, re, sys, urllib.parse, urllib.request

path, p95_budget, gen_fails, logpath = sys.argv[1:5]
rep = json.load(open(path))
fails = []

def check(ok, label, detail=""):
    print(("PASS " if ok else "FAIL ") + label + (f" — {detail}" if detail else ""))
    if not ok:
        fails.append(label)

check(gen_fails == "0", "all storm processes exited clean", f"{gen_fails} died")
check(rep["failures"] == 0, "zero read-storm failures",
      f"{rep['failures']}/{rep['requests']} failed: {rep['failed'][:5]}")
check(rep["requests"] > 500, "the storm was real",
      f"{rep['requests']} requests at {rep['rps']}/s")
check(rep["p95_ms"] is not None and rep["p95_ms"] <= float(p95_budget),
      "read latency under budget",
      f"p50 {rep['p50_ms']}ms p95 {rep['p95_ms']}ms max {rep['max_ms']}ms")

log = open(logpath, "rb").read()
check(re.search(rb"panic", log, re.I) is None, "no panics during storm")

# post-storm: every surface answers, store has data, search works
try:
    with urllib.request.urlopen("http://127.0.0.1:18091/api/health", timeout=5) as r:
        check(r.status == 200, "post-storm health")
except Exception as e:
    check(False, "post-storm health", str(e))
for p in ("/", "/logs", "/traces", "/services", "/metrics", "/storage", "/query"):
    try:
        with urllib.request.urlopen("http://127.0.0.1:18091" + p, timeout=10) as r:
            check(r.status == 200 and len(r.read()) > 500, f"post-storm {p} renders")
    except Exception as e:
        check(False, f"post-storm {p}", str(e))
q = urllib.parse.quote("SELECT count(*) AS n FROM otel.main.otlp_traces")
try:
    with urllib.request.urlopen(f"http://127.0.0.1:18091/query?mode=sql&sql={q}", timeout=10) as r:
        page = r.read().decode()
    m = re.search(r"<td[^>]*>(\d+)</td>", page)
    check(bool(m) and int(m.group(1)) > 1000, "spans survived the storm",
          f"{m.group(1) if m else '?'} spans")
except Exception as e:
    check(False, "post-storm SQL", str(e))

print("stress: FAILED" if fails else "stress: all checks passed")
sys.exit(1 if fails else 0)
PY
RC=$?

SINCE=$(find ~/Library/Logs/DiagnosticReports -name 'lotel-*.ips' -newer "$TMP/.stress-start" 2>/dev/null | wc -l | tr -d ' ')
[ "$SINCE" = "0" ] || { echo "FAIL: $SINCE lotel crash report(s) written during the storm"; RC=1; }

exit "$RC"
