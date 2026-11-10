#!/usr/bin/env bash
# Smoke test: boots the binary against a throwaway data dir, ingests the
# OTLP fixtures in `testdata/`, then asserts every page renders its truth.
#
# Requires the real extensions embedded (run `just fetch-extensions` first).
# Exits non-zero on the first failed assertion.
set -euo pipefail
cd "$(dirname "$0")/.."

BIN="${LOTEL_BIN:-target/debug/lotel}"
[ -x "$BIN" ] || BIN=target/release/lotel
[ -x "$BIN" ] || { echo "no binary; run cargo build first" >&2; exit 1; }

TMP="$(mktemp -d)"
trap 'kill $SRV 2>/dev/null || true; rm -rf "$TMP"' EXIT

export LOTEL_ADDR=127.0.0.1:18080
export LOTEL_OTLP_ADDR=127.0.0.1:14318
export LOTEL_DATA_DIR="$TMP/store"
export LOTEL_INGEST_TOKEN=lotel-smoke-token-0123456789

"$BIN" >"$TMP/log" 2>&1 &
SRV=$!

for _ in $(seq 1 50); do
    curl -fsS http://127.0.0.1:18080/api/health >/dev/null 2>&1 && break
    sleep 0.2
done

for s in logs traces metrics; do
    curl -fsS -X POST "http://127.0.0.1:14318/v1/$s" \
        -H 'Authorization: Bearer lotel-smoke-token-0123456789' \
        -H 'Content-Type: application/json' \
        --data "@testdata/$s.json" >/dev/null
done
sleep 3

python3 - <<'PY'
import json, sys, time, urllib.request

B = "http://127.0.0.1:18080"

def get(path):
    with urllib.request.urlopen(B + path, timeout=10) as r:
        return r.status, r.read().decode()

def unesc(s):
    return s.replace("&amp;", "&")

fails = 0
def check(name, path, want):
    global fails
    status, body = get(path)
    ok = status == 200 and want in body
    print(("PASS" if ok else f"FAIL (want {want!r})"), name)
    if not ok:
        fails += 1

def wait_for(name, path, want, timeout=30):
    """Ingest is buffered in the receiver; poll until rows become visible."""
    global fails
    end = time.time() + timeout
    body = ""
    while time.time() < end:
        try:
            _, body = get(path)
            if want in body:
                print("PASS", name)
                return body
        except Exception:
            pass
        time.sleep(1)
    print(f"FAIL timeout (want {want!r}) {name}")
    fails += 1
    sys.exit(1)

check("health",         "/api/health", "ok")
check("dashboard",      "/", "Store — capped tables")
check("snippet",        "/", "http://127.0.0.1:14318")
check("logs page",      "/logs", "logrows")
check("logs search",    "/logs?q=gateway", "gateway timeout")
check("logs severity",  "/logs?severity=ERROR", "badge error")
check("fragment bare",  "/api/logs", "<tr>")

status, body = get("/api/logs")
assert "<!DOCTYPE" not in body, "fragment endpoint must not carry the layout"

body = wait_for("trace feed", "/traces", "/traces?trace=")
assert "By service" in body
trace = unesc(body).split("/traces?trace=")[1].split('"')[0]
check("waterfall",      f"/traces?trace={trace}", "wfbar")
check("waterfall tree", f"/traces?trace={trace}", "padding-left:")
body = unesc(get(f"/traces?trace={trace}")[1])
span = body.split("&span=")[1].split('"')[0]
check("span detail",    f"/traces?trace={trace}&span={span}", "<pre>")

wait_for("metric catalog", "/metrics", "checkout.queue.depth")
body = unesc(get("/metrics")[1])
link = body.split('href="/metrics?')[1].split('"')[0]
check("metric chart",   "/metrics?" + link, "uPlot (v1.6.31)")

body = wait_for("metric series", "/api/metrics/series?" + link.replace("&chart=1", ""), "[")
d = json.loads(body)
assert d.get("xs"), f"empty series: {body[:120]}"
print("PASS  metric series  points:", len(d["xs"]))

# Explore: query builder, chart endpoint, attribute filters, SQL console.
import urllib.parse as U
def q(**k): return "?" + U.urlencode(k)

b = wait_for("query builder", "/query" + q(dataset="logs", range="all",
                measures='["count"]', group='["service_name"]'), "by <b>service_name")
assert "checkout" in b, "breakdown value missing"

_, cbody = get("/api/query/chart" + q(dataset="logs", range="all",
                measures='["count"]', group='["service_name"]'))
cd = json.loads(cbody)
assert cd.get("stacked") and cd.get("series"), f"chart: {cbody[:120]}"
print("PASS  query chart  series:", len(cd["series"]))

_, ab = get("/query" + q(dataset="logs", range="all",
                filters='[{"col":"@log_attributes:code","op":"=","val":"ETIMEDOUT"}]'))
assert "json_extract_string" in ab and "Query error" not in ab, ab[:200]
print("PASS  query attribute filter")

_, sb = get("/query" + q(mode="sql", sql="SELECT count(*) AS n FROM otel.main.otlp_logs"))
assert "SELECT count(*) AS n" in sb and "Query error" not in sb, sb[:200]
print("PASS  sql console")

_, gb = get("/query" + q(mode="sql", sql="DELETE FROM otel.main.otlp_logs"))
assert "got 'delete'" in gb, gb[:200]
print("PASS  sql read-only guard")

_, fpage = get("/query" + q(dataset="logs", range="all", measures='["count"]'))
assert 'class="qfacets"' in fpage and 'class="fval"' in fpage, "facet sidebar missing"
assert '"stats":' in fpage, "stats not embedded"
print("PASS  facet sidebar + stats")

_, cpage = get("/query" + q(dataset="logs", range="all", measures='["count"]',
                filters='[{"col":"service_name","op":"=","val":"cart"}]'))
assert "Query error" not in cpage and 'class="qfacets"' in cpage, cpage[:200]
print("PASS  facet drill filters")

_, tpage = get("/query" + q(dataset="traces", range="all", measures='["count"]', group='["trace_id"]'))
assert 'class="fcell"' in tpage and "/traces?trace=" in tpage, tpage[:200]
print("PASS  drill-through opens trace")

# Server-rendered distributions for the two histogram shapes.
HG = "/metrics?shape=histogram&name=http.request.duration&service=api-gateway&chart=1"
wait_for("histogram catalog", "/metrics", "http.request.duration")
check("histogram bars",  HG, 'class="hbar"')
check("histogram count", HG, ">480<")           # tallest bucket = 480
check("histogram mean",  HG, "mean 21.5")       # sum/count = 21500/1000

XG = "/metrics?shape=exp_histogram&name=http.server.active&service=api-gateway&chart=1"
wait_for("exp histogram catalog", "/metrics", "http.server.active")
check("exp bars",        XG, 'class="hbar"')
check("exp edge",        XG, "0.841")           # 2^(2^-2) reconstruction: (0.841, 1]
check("exp count",       XG, ">140<")
check("exp zero bucket", XG, "40")              # zero_count

check("storage ttl",    "/storage", "7d")
check("storage segs",   "/storage", "Segments")

sys.exit(1 if fails else 0)
PY

echo "smoke: all checks passed"
