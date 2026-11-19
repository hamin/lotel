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
SRV2=""
trap 'kill $SRV 2>/dev/null || true; kill $SRV2 2>/dev/null || true; rm -rf "$TMP"' EXIT

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
# agent-style log bodies (JSON arrays/objects, Claude Code event rows)
curl -fsS -X POST "http://127.0.0.1:14318/v1/logs" \
    -H 'Authorization: Bearer lotel-smoke-token-0123456789' \
    -H 'Content-Type: application/json' \
    --data "@testdata/logs-agents.json" >/dev/null
# M-Agents T2 canaries (invented by this script, never copied from a real
# capture): the shapes the agent CLIs send when their content gates are open —
# a prompt attribute, opencode's `[message, "<command line>"]` body, a tool
# input on a span, plus an email and a home path in free text.
python3 - "$TMP" <<'PY'
import json, sys

t = sys.argv[1]
PROMPT, COMMAND, MAIL, PATH = "canary-prompt-4f7d2a", "canary-command-9be1c3", "jane.doe@corp.example", "/Users/jane/keys.pem"


def attr(key, value):
    shape = {"boolValue": value} if isinstance(value, bool) else (
        {"intValue": str(value)} if isinstance(value, int) else {"stringValue": value})
    return {"key": key, "value": shape}


def resource(name):
    return {"attributes": [attr("service.name", name)]}


cli = {"timeUnixNano": "1795023724725000000", "severityText": "INFO",
       "body": {"stringValue": json.dumps(["cli starting", json.dumps({"args": ["run", PROMPT], "cwd": PATH})])},
       "attributes": [attr("role", "cli")]}
prompt = {"timeUnixNano": "1795023724726000000", "severityText": "INFO",
          "body": {"stringValue": "claude_code.user_prompt"},
          "attributes": [attr("event.name", "user_prompt"), attr("session.id", "ses_canary"),
                         attr("prompt", PROMPT), attr("prompt_length", 18), attr("tool_name", "canary-bash")]}
logs = {"resourceLogs": [
    {"resource": resource("canary-agent"),
     "scopeLogs": [{"scope": {"name": "effect"}, "logRecords": [cli, prompt]}]}]}
span = {"traceId": "0123456789abcdef0123456789abcdef", "spanId": "0123456789abcdef",
        "name": f"tool execute for {MAIL}", "kind": 1,
        "startTimeUnixNano": "1795023724725000000", "endTimeUnixNano": "1795023725725000000",
        "status": {"code": 2, "message": f"denied: {PATH}"},
        "attributes": [attr("tool_name", "canary-bash"),
                       attr("tool_input", json.dumps({"command": COMMAND})),
                       attr("duration_ms", 1500)]}
traces = {"resourceSpans": [{"resource": resource("canary-agent"),
                             "scopeSpans": [{"scope": {"name": "claude_code"}, "spans": [span]}]}]}
open(f"{t}/canary-logs.json", "w").write(json.dumps(logs))
open(f"{t}/canary-traces.json", "w").write(json.dumps(traces))
PY
for s in logs traces; do
    curl -fsS -X POST "http://127.0.0.1:14318/v1/$s" \
        -H 'Authorization: Bearer lotel-smoke-token-0123456789' \
        -H 'Content-Type: application/json' \
        --data "@$TMP/canary-$s.json" >/dev/null
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
check("dashboard",      "/", "Store: capped tables")
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

# Agent-style bodies: JSON renders as message + key=value chips (no JS), and
# a row expands via <details>. Non-JSON rows stay verbatim.
wait_for("agents array body", "/logs", "cli starting")
# M-Agents T2 (§5) retires J1's expectation here. opencode's `cli starting`
# payload is the one real prompt leak in the capture, so its whole element is
# masked at render: the message stays, the fields decoded out of it (2.3.0,
# args) no longer reach the page. The `f=body.version%3D2.3.0` check below
# still proves the payload decodes and stays filterable — filters read the
# stored row, rendering reads the masked copy.
_, abb = get("/logs")
assert "2.3.0" not in abb, "the cli-starting payload must not render its fields"
assert "[REDACTED]" in abb, "the masked element shows the placeholder instead"
print("PASS  cli-starting payload masked (message kept, fields hidden)")
check("agents object body",  "/logs", "session created")
check("agents object chips", "/logs", "anthropic")
check("claude code row",     "/logs", "claude_code.tool_result")
check("claude code chips",   "/logs", "duration_ms")      # log_attributes chips
check("row expansion",       "/logs", 'class="logx"')     # <details>, no JS

# STEP 0: expandable rows carry a stable data-key so the live tail can keep
# open <details> across a swap (LotelLive reads them by key).
check("row data-key",        "/logs", "data-key=")
_, lfrag = get("/api/live?topic=logs&range=all")
assert "data-key=" in lfrag, "tail fragment rows must carry the data-key too"
print("PASS  data-key on tail fragment rows")

# J2: the shared filter bar. Attribute filters ride `f` params; parse errors
# show inline next to the input, never as a 500.
check("bar attribute box",   "/logs", 'name="f"')
check("f filter equals",     "/logs?f=tool_name%3Dbash", "claude_code.tool_result")
_, fb = get("/logs?f=tool_name%3Dbash")
assert "payment authorized" not in fb, "f filter must narrow the list"
assert "Query error" not in fb and "500" not in fb[:200]
check("f filter resource",   "/logs?f=resource.deployment.environment%3Ddev", "cli starting")  # opencode resource only
check("f filter body json",  "/logs?f=body.provider%3Danthropic", "session created")
check("f filter array body", "/logs?f=body.version%3D2.3.0", "cli starting")
check("f negation",          "/logs?f=tool_name%21%3Dbash", "logrows")
_, nb = get("/logs?f=tool_name%21%3Dbash")
assert "claude_code.tool_result" not in nb, "!= must drop the bash row"
check("f parse error inline", "/logs?f=just-junk", 'class="ferr"')
_, eb = get("/logs?f=just-junk")
assert "payment authorized" in eb, "a junk filter line must not narrow or 500"
check("chip is a filter link", "/logs", 'href="/logs?')
_, cb = get("/logs")
assert 'class="kchip" href=' in cb, "attribute chips must be plain GET links"
print("PASS  shared attribute filters (equals / resource / body / != / inline errors)")

# M-Agents T2b: the Fields panel — what the bar's rows carry. A <details>
# (no JS) under the facets: a name filter (ff), one closed line per key that
# opens on its stats and top values, 20 keys a page (fp), empty keys hidden
# unless fe=1. Each key line is itself a <details>, so the panel ends at its
# pager, not at the first </details>.
def fields_tag(body):
    """The whole <details ... id="fields" ...> open tag: attribute order varies per render."""
    b = unesc(body); i = b.index('id="fields"')
    return b[b.rindex("<", 0, i):b.index(">", i)]

def fields_panel(body):
    head, _, tail = unesc(body).split('class="card fields"')[-1].partition('class="fpager"')
    return head + tail.split("</p>")[0]

_, lbp = get("/logs")
lpanel = fields_panel(lbp)
assert 'class="card fields"' in lbp and "from the latest 5,000 rows" in lpanel, "fields panel on /logs"
assert ">Measures<" in lpanel and "duration_ms" in lpanel, "numeric keys lead as Measures"
assert "tool_name" in lpanel and 'class="fval"' in lpanel, "string keys list top values with counts"
assert "/logs?f=tool_name%3Dbash" in unesc(lbp), "a top value links like a chip"
_, tb = get("/logs?f=tool_name%3Dbash")
assert "claude_code.tool_result" in tb and "payment authorized" not in tb, "the top-value link returns only matching rows"
assert "/query?dataset=logs" in lpanel and "%40log_attributes%3A" in lpanel, "numeric keys open Explore on @col:key"
print("PASS  fields panel on /logs: measures, coverage, top-value link narrows, explore links")

_, ffb = get("/logs?ff=duration")
ffp = fields_panel(ffb)
assert "duration_ms" in ffp and "tool_name" not in ffp, "ff narrows the key list by name"
assert 'name="ff" value="duration"' in ffp and 'open="open"' in fields_tag(ffb), "the filter echoes and keeps the panel open"
_, fsb = get("/logs?ff=duration&service=checkout")
assert 'type="hidden" name="service" value="checkout"' in fields_panel(fsb), "the filter form re-sends the bar"
lines = lambda b: fields_panel(b).count('class="fline"')
_, feb = get("/logs?fe=1")
assert lines(feb) >= lines(lbp), "fe=1 lists empty keys too"
_, fpb = get("/logs?fp=2")
assert "fields" in fields_panel(fpb) and fpb.count("payment authorized") == lbp.count("payment authorized"), "fp pages the panel, never the table"
print("PASS  fields panel: name filter, empty keys, paging leave the table alone")

# J3: an empty list under a time range narrower than all shows a one-line
# hint linking to the same query at range=all (plain <a>, works with JS off).
_, hb = get("/logs?range=1h&f=tool_name%3Dnothing-matches-this")
assert "over all time" in hb and "range=all" in unesc(hb), "empty + narrow range must offer all time"
assert "/logs?range=all&f=tool_name%3Dnothing-matches-this" in unesc(hb), "hint keeps the filters"
_, hb = get("/logs?range=1h")
assert "over all time" not in hb, "a list with rows needs no hint"
_, hb = get("/traces?range=1h")
assert "over all time" in hb and "range=all" in unesc(hb), "empty span feed must offer all time"
_, hb = get("/traces?range=all&name=zzz-no-such-span")
assert "over all time" not in hb, "range=all is already all: no hint"
print("PASS  empty + narrow range links to the same query at range=all")

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
assert "Overall p95 per bucket" in body, "latency trend card"
assert "Span duration distribution" in body, "duration histogram"
assert "sm-spark sm-lat" in body, "per-service p95 trend sparkline"
trace = unesc(body).split("/traces?trace=")[1].split('"')[0]
check("waterfall",      f"/traces?trace={trace}", "wfbar")
check("waterfall tree", f"/traces?trace={trace}", "padding-left:")
body = unesc(get(f"/traces?trace={trace}")[1])
span = body.split("&span=")[1].split('"')[0]
check("span detail",    f"/traces?trace={trace}&span={span}", "<pre>")
# M18 T7: span names ride the own class namecell (logbody's overflow-wrap:
# anywhere starves the column to one char per line), and the status cell
# always shows ok/unset/error — never blank.
check("span name cell",   "/traces", 'class="namecell"')
check("span status text", "/traces", ">unset<")

# J2: the traces page shares the same filter bar; feed and p95 trend obey it.
check("trace bar",        "/traces", 'name="mindur"')
check("trace bar attrs",  "/traces", 'name="f"')
_, mb = get("/traces?mindur=3500")
assert "POST /checkout" in mb, "slow root span passes the 3.5s floor"
assert "POST /payments" not in mb, "3.2s span must be cut by min duration"
assert "Overall p95 per bucket" in mb, "trend card stays with the bar"
_, tb = get("/traces?f=http.method%3DPOST&status=error")
assert "POST /payments" in tb, "attribute+status filter keeps the error span"
assert "POST /checkout" not in tb, "status=error must drop the ok span"
_, jb = get("/traces?mindur=soon&f=nonsense-line")
assert "Recent spans" in jb and 'class="ferr"' in jb, "bad input is inline, not a 500"
print("PASS  traces shared bar: feed respects min duration, status and attributes")

# T2b: the same Fields panel on /traces, profiling span attributes + resource.
_, tbp = get("/traces")
tplanel = fields_panel(tbp)
assert 'class="card fields"' in tbp and "from the latest 5,000 rows" in tplanel, "fields panel on /traces"
assert "http.method" in tplanel and "/traces?f=http.method%3DPOST" in tplanel, "span keys and top-value links"
assert 'class="fval"' in tplanel and "%40span_attributes%3A" in tplanel, "value counts + explore links"
print("PASS  fields panel on /traces profiles span and resource attributes")

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
assert "sm-lat" in smap and 'class="sm-spark sm-lat"><svg class="spark"' in smap and any('class="spark-line"' in d for d in smap.split('class="sm-spark sm-lat">')[1:]), "node p95 latency sparkline missing"
print("PASS  service map (nodes + edge svg + error/p95 sparklines)")
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

# the tail fragment filters exactly like the page it feeds
_, lfilt = get("/api/live?topic=logs&range=all&f=tool_name%3Dbash")
assert "claude_code.tool_result" in lfilt and "payment authorized" not in lfilt, \
    "live fragment must honour the bar's f filters"
print("PASS  /api/live?topic=logs honours the filter bar")

_, lpage = get("/logs")
assert "LotelLive.sub" in lpage and 'id="logrows"' in lpage and "/api/live?topic=" in lpage, "logs live tail wiring missing"
assert "TAIL_JS" not in lpage, "old TAIL_JS still present"
# STEP 0: the swap keeps open <details> (by data-key) and yields to a text selection
assert "querySelectorAll('details[data-key]')" in lpage, "live swap must track open details by data-key"
assert "getSelection" in lpage, "live swap must skip while text inside the target is selected"
print("PASS  logs live tail folded into LotelLive (open-details + selection guard)")

# M-Agents T2 · privacy default (plan §5): prompt text, tool input/output and
# the opencode command line are never displayed. The canaries this script
# invented must be absent from the row list, the tail fragment, a filtered
# list, the SQL console's result cells and Explore's result table — while the
# keys, counts and event names that make a row readable do survive.
CANARY_PROMPT, CANARY_CMD, CANARY_MAIL = "canary-prompt-4f7d2a", "canary-command-9be1c3", "jane.doe@corp.example"
cab = wait_for("canary agent rows", "/logs?service=canary-agent", "canary-bash")
for name, path in (("tail fragment", "/api/live?topic=logs&range=all&service=canary-agent"),
                   ("attribute filter", "/logs?f=session.id%3Dses_canary")):
    _, b = get(path)
    assert CANARY_PROMPT not in b, f"{name} leaked the prompt"
    print("PASS  no prompt in the", name)
assert "[REDACTED]" in cab, "the masked placeholder is what the row shows"
assert "cli starting" in cab, "the message element of the cli-starting body stays"
assert "prompt_length" in cab and "user_prompt" in cab and "canary-bash" in cab, \
    "keys, event names and numeric companions survive masking"
assert "canary-command" not in cab, "the whole payload element goes, keys included"
print("PASS  masked row keeps its shape: message, keys, counts — no contents")

_, cb = get("/query?mode=sql&sql=" + urllib.parse.quote(
    "SELECT body, log_attributes FROM otel.main.otlp_logs WHERE service_name = 'canary-agent'"))
assert CANARY_PROMPT not in cb and "[REDACTED]" in cb, "SQL console result cells must be masked"
print("PASS  sql console result cells masked")

_, eb = get("/query" + q(dataset="logs", range="all",
              filters='[{"col":"service_name","op":"=","val":"canary-agent"}]'))
table = eb.split('class="qtablewrap"')[-1].split("</table>")[0]
assert CANARY_PROMPT not in table, "Explore result cells must be masked, and their links too"
assert "[REDACTED]" in table, "…and show the placeholder"
print("PASS  explore result cells masked")
assert CANARY_PROMPT not in eb, "Explore facet labels and links must be masked too"
print("PASS  explore page (facets included) masked")

_, tb = get("/traces?service=canary-agent")
assert CANARY_MAIL not in tb and "[redacted-email@example.invalid]" in tb, "span name must not carry an address"
trace = unesc(tb).split("/traces?trace=")[1].split('"')[0]
_, sb = get(f"/traces?trace={trace}&span=0123456789abcdef")
assert CANARY_CMD not in sb and "tool_input" in sb and "[REDACTED]" in sb, "span attributes masked, key kept"
assert "denied: /Users/jane" not in sb, "the status message loses the home path"
print("PASS  waterfall + span card mask stored text, keep ids and keys")

# M-Agents T3: /sessions lists one row per agent session across all three
# agents, each keyed the way that agent carries it.
for f, sig in (("claude-opencode-logs", "logs"), ("claude-opencode-traces", "traces"),
               ("claude-opencode-metrics-sum", "metrics"), ("codex-logs", "logs"),
               ("codex-traces", "traces")):
    with open(f"testdata/agent/{f}.json", "rb") as fh:
        req = urllib.request.Request(f"http://127.0.0.1:14318/v1/{sig}", data=fh.read(), method="POST",
              headers={"Content-Type": "application/json",
                       "Authorization": "Bearer lotel-smoke-token-0123456789"})
        urllib.request.urlopen(req, timeout=10).read()
CC_SID = "218acd0a-68ea-4150-84e2-5df4bf0af31c"
CX_SID = "01a1efa1-f56a-7743-b054-c3905d84e95d"
wait_for("sessions: codex session listed", "/sessions?range=all", CX_SID)
ss = unesc(wait_for("sessions: claude code session listed", "/sessions?range=all", CC_SID))
assert "3e122abc" in ss, "an opencode run is a session too"
assert f"/sessions?range=all&session={CC_SID}" in ss, "each row links its timeline"
assert f"/logs?range=all&f=session.id%3D{CC_SID}" in ss, "each row links its logs"
assert f"/traces?range=all&f=session.id%3D{CC_SID}" in ss, "a session with spans links its traces"
_, tsb = get(f"/traces?range=all&f=session.id%3D{CC_SID}")
import re
assert re.search(r"trace=[0-9a-f]{32}", tsb), "the traces link lists the session's traces"
assert ">11<" not in ss and 'session=11"' not in ss, "codex OS thread numbers are not sessions"
print("PASS  sessions page: three agents, timeline + logs links, no thread-number sessions")
for sid in (CC_SID, CX_SID, "3e122abc"):
    _, nb = get(f"/sessions?range=all&f=session.id%3D{sid}")
    others = {CC_SID, CX_SID, "3e122abc"} - {sid}
    assert sid in nb and not any(o in nb for o in others), f"f=session.id narrows to {sid}"
    _, lb = get(f"/logs?range=all&f=session.id%3D{sid}")
    # opencode runs only carry the key on spans; Codex log rows land with a NULL
    # time (it sends timeUnixNano=0, observed time only), so /logs can't list
    # them yet: follow-up in the OTLP extension, not a /sessions concern.
    if sid == CC_SID:
        body_rows = lb.split('id="logrows"')[1].split("</tbody>")[0]
        assert "<tr" in body_rows, f"the logs link finds {sid}'s rows"
print("PASS  f=session.id narrows the list for every agent's key, and the logs link lands")
_, lb = get("/logs?range=all&f=session.id%3Dnone")
assert "<tr" not in lb.split('id="logrows"')[1].split("</tbody>")[0], "an unknown session finds no logs"
# Log <-> trace links: a trace with logs links to them (whole trace and the
# selected span); /logs?trace= narrows to exactly those rows and links back.
TR, SP = "db4c1d903e60737b19c678d31bf02a9a", "0fc994a7c16b904b"
wb = unesc(wait_for("trace page links its logs", f"/traces?trace={TR}", "30 logs"))
assert f"/logs?range=all&trace={TR}" in wb, "the trace header links /logs?trace="
_, spb = get(f"/traces?trace={TR}&span={SP}")
assert "27 logs" in spb and f"/logs?range=all&trace={TR}&span={SP}" in unesc(spb), "the span card links its own logs"
_, ltb = get(f"/logs?range=all&trace={TR}")
rows_html = ltb.split('id="logrows"')[1].split("</tbody>")[0]
assert rows_html.count("<tr") == 30, f"trace-scoped /logs lists the trace's 30 logs, got {rows_html.count('<tr')}"
assert "show all logs" in ltb and f'name="trace" value="{TR}"' in ltb, "scope is shown, clearable, and kept by the form"
assert rows_html.count(f"/traces?trace={TR}") == 30, "each row links back to its stored trace"
print("PASS  log <-> trace links: trace + span log counts, scoped /logs, links back")
# M-Agents T4: /sessions?session= is the timeline — the session's spans as a
# waterfall, the span card on ?span=, and the event band with glance chips.
_, tlb = get(f"/sessions?range=all&session={CC_SID}")
tl = unesc(tlb)
assert 'class="wf"' in tl and f"/sessions?session={CC_SID}&span=" in tl, "the session's spans render as a waterfall"
assert 'class="events"' in tl and "<b>tool_name</b>" in tl and "<b>cost_usd</b>" in tl, "event band with glance chips"
assert "\u00d7" in tl or "×" in tl, "startup bursts fold into one row with a count"
for k in ("<b>prompt</b>", "<b>tool_input</b>", "<b>tool_output</b>", "<b>response</b>"):
    assert k not in tl, f"content key {k} must never be a chip"
_, scb = get(f"/sessions?range=all&session={CC_SID}&span={SP}")
assert "27 logs" in scb and f"/traces?trace={TR}&amp;span={SP}" in scb, "span card: its logs and its trace"
for sid in (CX_SID, "3e122abc"):
    _, ob = get(f"/sessions?range=all&session={sid}")
    assert 'class="wf"' in ob, f"{sid}: other agents' sessions get a waterfall too"
print("PASS  session timeline: waterfall, span card, event band, codex + opencode")
check("sessions empty", "/sessions?range=all&f=session.id%3Dnone", "No agent sessions")

_, hb = get("/")
assert "\n.seg {" not in hb and "}.seg {" not in hb and ".ribbon-track .seg {" in hb, \
    "the ribbon's .seg box style must stay scoped: header .seg labels (trace, services, metrics) share the class"
print("PASS  .seg segment style scoped to the storage ribbon")
check("storage ttl",    "/storage", "7d")
check("storage segs",   "/storage", "Segments")

sys.exit(1 if fails else 0)
PY

# M-Agents T2: LOTEL_AGENT_SHOW_CONTENT=1 is the only way to see content, and
# it announces itself (§5) — a lotel that displays prompts must never look
# like the private default. Its own throwaway instance, store and ports.
export LOTEL_ADDR=127.0.0.1:18081 LOTEL_OTLP_ADDR=127.0.0.1:14319 LOTEL_DATA_DIR="$TMP/store-show"
export LOTEL_AGENT_SHOW_CONTENT=1
"$BIN" >"$TMP/log-show" 2>&1 &
SRV2=$!
for _ in $(seq 1 50); do
    curl -fsS http://127.0.0.1:18081/api/health >/dev/null 2>&1 && break
    sleep 0.2
done
curl -fsS -X POST "http://127.0.0.1:14319/v1/logs" \
    -H 'Authorization: Bearer lotel-smoke-token-0123456789' \
    -H 'Content-Type: application/json' \
    --data "@$TMP/canary-logs.json" >/dev/null
shown=""
for _ in $(seq 1 60); do
    curl -fsS http://127.0.0.1:18081/logs | grep -q 'canary-prompt-4f7d2a' && { shown=yes; break; }
    sleep 0.5
done
grep -q 'agent content: shown' "$TMP/log-show" \
    || { echo "FAIL opt-in banner line missing"; exit 1; }
[ -n "$shown" ] || { echo "FAIL opt-in did not display the content"; exit 1; }
grep -q 'agent content: shown' "$TMP/log" \
    && { echo "FAIL the default instance printed the opt-in banner"; exit 1; }
kill $SRV2 2>/dev/null || true
wait $SRV2 2>/dev/null || true
SRV2=""
echo "smoke: opt-in (LOTEL_AGENT_SHOW_CONTENT) displays content and says so"

echo "smoke: all checks passed"
