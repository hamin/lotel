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
import json, sys, time, urllib.request, urllib.parse

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

check("log facets",     "/logs", 'class="facets"')
check("facet bars",     "/logs", "facet-n")
check("facet active",   "/logs?severity=ERROR", "facet-row on")

# BM25 best matches: force a seal through the read-only SQL console (sidecar
# index builds inside it), then poll for ranked hits from the sealed segments.
seal_sql = urllib.parse.quote("SELECT * FROM capped_seal('otel', 'otlp_logs')")
rb = ""
for _ in range(30):
    try:
        get(f"/query?mode=sql&sql={seal_sql}")
    except Exception:
        pass
    _, rb = get("/logs?q=gateway")
    if "badge score" in rb:
        break
    time.sleep(1)
ok = "Best matches (BM25)" in rb and "badge score" in rb and "gateway timeout" in rb
print(("PASS" if ok else "FAIL"), "BM25 best matches (ranked hits from sealed segments)")
if not ok:
    fails += 1

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

# Events tab: raw matching rows, newest first, reachable like /logs and /traces.
_, ep = get("/query" + q(dataset="logs", range="all"))
assert 'class="rtab on"' in ep and ">Events<" in ep, "events not default ungrouped"
assert 'data-stat="body"' in ep and 'data-stat="trace_id"' in ep, "events columns missing"
assert "cache miss ratio" in ep, "raw log body row missing"      # a real log line
assert "tab=series" in ep and "tab=events" in ep, "events tab links missing"
print("PASS  events tab (raw rows)")

_, tev = get("/query" + q(dataset="traces", range="all"))
assert ">Events<" in tev and 'data-stat="name"' in tev, "span events missing"
assert "/traces?trace=" in tev, "span events trace drill missing"
print("PASS  events tab opens trace")

_, gtab = get("/query" + q(dataset="logs", range="all", measures='["count"]', group='["service_name"]'))
assert ">Breakdown<" in gtab and 'class="rtab on"' in gtab and "tab=events" in gtab, "grouped tabs missing"
print("PASS  grouped shows events+series+breakdown")

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

# Service map + dashboard error-rate sparklines: both server-rendered SVG, no JS.
smap = wait_for("service map", "/services?range=all", 'class="sm-node"')
assert 'class="sm-edges"' in smap, "edge underlay svg missing"
assert "checkout" in smap, "service node missing"
assert "/traces?service=" in smap, "node drill link missing"
assert "No spans in this window" not in smap, "unexpected empty state"
assert 'class="spark"' in smap, "node error-rate sparkline missing"
print("PASS  service map (nodes + edge svg + sparkline)")
assert 'id="sm-live"' in smap and "live-toggle" in smap, "auto-refresh mount missing"
assert "LotelLive.sub" in smap and "/api/live?topic=" in smap and "setInterval" in smap, "live controller missing"
print("PASS  service-map live refresh wiring (LotelLive + /api/live topic)")

# The seam endpoint renders the same fragments bare, keyed by topic.
_, lmap = get("/api/live?topic=map&range=all")
assert "<!DOCTYPE" not in lmap, "live map topic must render bare"
assert 'class="sm-node"' in lmap and 'class="sm-edges"' in lmap, "live map topic missing canvas"
print("PASS  /api/live?topic=map (bare fragment)")

_, sfrag = get("/api/services?range=all")
assert "<!DOCTYPE" not in sfrag, "map fragment must render bare (no layout)"
assert 'class="sm-node"' in sfrag and 'class="sm-edges"' in sfrag, "fragment missing canvas"
print("PASS  service-map live fragment (bare)")

# Dashboard card renders with or without recent data (fixtures are historical).
_, home = get("/")
assert "Error rate" in home and 'class="card errcard"' in home, "error-rate card missing"
assert 'id="err-live"' in home and "err-live-toggle" in home and "LotelLive" in home, "dashboard live refresh missing"
print("PASS  dashboard error-rate card + live refresh")

_, lerr = get("/api/live?topic=err")
assert "<!DOCTYPE" not in lerr, "live err topic must render bare"
assert 'class="card errcard"' in lerr, "live err topic missing card"
print("PASS  /api/live?topic=err (bare fragment)")

_, llogs = get("/api/live?topic=logs&range=all")
assert "<!DOCTYPE" not in llogs, "live logs topic must render bare"
assert "<tr>" in llogs and "cache miss ratio" in llogs, "live logs topic missing rows"
print("PASS  /api/live?topic=logs (bare fragment)")

_, lpage = get("/logs")
assert "LotelLive.sub" in lpage and 'id="logrows"' in lpage and "/api/live?topic=" in lpage, "logs live tail wiring missing"
assert "TAIL_JS" not in lpage, "old TAIL_JS still present"
print("PASS  logs live tail folded into LotelLive")

check("storage ttl",    "/storage", "7d")
check("storage segs",   "/storage", "Segments")

sys.exit(1 if fails else 0)
PY

echo "smoke: all checks passed"
