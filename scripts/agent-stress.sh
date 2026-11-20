#!/usr/bin/env bash
# Agent stress (M-agents T10, local by design): replay the committed agent
# fixtures (testdata/agent/) N times — each copy with its own session ids,
# trace/span ids and opencode run ids, and its timestamps spread back over
# the last two days — then time the agent pages against the full store.
#
#   N=500 ./scripts/agent-stress.sh
#
# Gates: every copy ingests; no agent page shows a query error or an empty
# list over a full store; /sessions (list + one timeline), /spend and
# /tools answer under P95_BUDGET_MS at p95; the session count matches what
# was sent; the live heap stays under LOTEL_MEMORY_LIMIT; every table stays
# within LOTEL_MAX_BYTES; no panics.
#
# Own ports (18092/14330), throwaway store; can run beside a dev server.
set -uo pipefail
cd "$(dirname "$0")/.."

BIN="${LOTEL_BIN:-target/release/lotel}"
[ -x "$BIN" ] || BIN=target/debug/lotel
[ -x "$BIN" ] || { echo "no binary; run cargo build first" >&2; exit 1; }

N="${N:-500}"
WORKERS="${WORKERS:-8}"
ROUNDS="${ROUNDS:-20}"              # timed requests per page
P95_BUDGET_MS="${P95_BUDGET_MS:-500}"

TMP="$(mktemp -d)"
LOG="$TMP/lotel.log"
touch "$TMP/.stress-start"
SRV=""
cleanup() {
    [ -n "$SRV" ] && kill -9 "$SRV" 2>/dev/null
    wait 2>/dev/null
    [ -n "${KEEP:-}" ] && echo "kept $TMP" || rm -rf "$TMP"
}
trap cleanup EXIT

export LOTEL_ADDR=127.0.0.1:18092
export LOTEL_OTLP_ADDR=127.0.0.1:14330
export LOTEL_DATA_DIR="$TMP/store"
export LOTEL_INGEST_TOKEN=lotel-agent-stress-token-0123456789
export LOTEL_MAX_BYTES="${LOTEL_MAX_BYTES:-256MB}"
export LOTEL_MEMORY_LIMIT="${LOTEL_MEMORY_LIMIT:-1GB}"

"$BIN" >"$LOG" 2>&1 &
SRV=$!
up=""
for _ in $(seq 1 50); do
    if curl -fsS --max-time 2 http://127.0.0.1:18092/api/health >/dev/null 2>&1; then
        up=1; break
    fi
    sleep 0.2
done
[ -n "$up" ] || { echo "FAIL: server did not come up"; tail -5 "$LOG"; exit 1; }
echo "agent-stress: N=$N · $WORKERS senders · $(basename "$(dirname "$(dirname "$BIN")")")/$(basename "$(dirname "$BIN")") binary · pid $SRV"

python3 - "$N" "$WORKERS" "$ROUNDS" "$P95_BUDGET_MS" "$LOG" "$SRV" \
    "$LOTEL_MAX_BYTES" "$LOTEL_MEMORY_LIMIT" <<'PY'
import hashlib, json, re, statistics, subprocess, sys, time, urllib.parse, urllib.request
import multiprocessing
from concurrent.futures import ProcessPoolExecutor

n, workers, rounds, budget = int(sys.argv[1]), int(sys.argv[2]), int(sys.argv[3]), float(sys.argv[4])
logpath, pid, max_bytes, mem_limit = sys.argv[5], sys.argv[6], sys.argv[7], sys.argv[8]
UI, OTLP = "http://127.0.0.1:18092", "http://127.0.0.1:14330"
TOKEN = "lotel-agent-stress-token-0123456789"
FILES = (("claude-opencode-logs", "logs"), ("claude-opencode-traces", "traces"),
         ("claude-opencode-metrics-sum", "metrics"), ("codex-logs", "logs"),
         ("codex-traces", "traces"))
SPREAD_NS = 2 * 86400 * 10**9      # inside the 3-day trace TTL

fails = []
def check(ok, label, detail=""):
    print(("PASS " if ok else "FAIL ") + label + (f" — {detail}" if detail else ""), flush=True)
    if not ok:
        fails.append(label)

RAW = {f: open(f"testdata/agent/{f}.json").read() for f, _ in FILES}
TS = re.compile(r'("[A-Za-z]*UnixNano": ?)"(\d+)"')
UUID = re.compile(r'"([0-9a-f]{8})-([0-9a-f]{4})-([0-9a-f]{4})-([0-9a-f]{4})-([0-9a-f]{12})"')
HEX = re.compile(r'"([0-9a-f]{32}|[0-9a-f]{16}|[0-9a-f]{8})"')   # trace, span, opencode run
newest = max(int(m.group(2)) for s in RAW.values() for m in TS.finditer(s) if m.group(2) != "0")
SID = re.compile(r'"key": ?"(?:session\.id|conversation\.id)", ?"value": ?\{"stringValue": ?"([^"]+)"')
SIDS = sorted({m.group(1) for f in ("claude-opencode-logs", "codex-logs") for m in SID.finditer(RAW[f])})

def fresh(k, orig, length):
    return hashlib.blake2b(f"{k}:{orig}".encode(), digest_size=32).hexdigest()[:length]

def copy_of(k, text, shift):
    def ts(m):
        v = int(m.group(2))
        return m.group(0) if v == 0 else f'{m.group(1)}"{v + shift}"'
    def uuid(m):
        h = fresh(k, m.group(0), 32)
        return f'"{h[:8]}-{h[8:12]}-{h[12:16]}-{h[16:20]}-{h[20:]}"'
    text = TS.sub(ts, text)
    text = UUID.sub(uuid, text)
    return HEX.sub(lambda m: f'"{fresh(k, m.group(1), len(m.group(1)))}"', text)

def send(k):
    shift = time.time_ns() - newest - (k * SPREAD_NS) // max(1, n)
    for f, sig in FILES:
        req = urllib.request.Request(f"{OTLP}/v1/{sig}", data=copy_of(k, RAW[f], shift).encode(),
              method="POST", headers={"Content-Type": "application/json",
                                      "Authorization": f"Bearer {TOKEN}"})
        with urllib.request.urlopen(req, timeout=60) as r:
            if not 200 <= r.status < 300:
                return f"copy {k} {f}: HTTP {r.status}"
    return None

t0 = time.time()
# fork: a heredoc script has no file for spawn to re-import
with ProcessPoolExecutor(workers, mp_context=multiprocessing.get_context("fork")) as ex:
    errs = [e for e in ex.map(send, range(n)) if e]
mb = sum(len(s) for s in RAW.values()) * n / 1e6
check(not errs, f"all {n} copies ingested", f"{mb:.0f} MB of OTLP JSON in {time.time()-t0:.0f}s"
      + (f"; {len(errs)} failed: {errs[:3]}" if errs else ""))

def get(path, timeout=60):
    t = time.time()
    with urllib.request.urlopen(UI + path, timeout=timeout) as r:
        body = r.read().decode()
    return r.status, body, (time.time() - t) * 1000

def sql(q):
    _, body, _ = get("/query?mode=sql&sql=" + urllib.parse.quote(q))
    return re.findall(r"<td[^>]*>([^<]*)</td>", body)

# Settle: ingest is buffered (202 "buffered") and flushed on an interval, so
# the store is done when the log count holds still across a flush window.
last, stable = -1, 0
for _ in range(180):
    c = int((sql("SELECT count(*) AS n FROM otel.main.otlp_logs") or ["0"])[0])
    stable = stable + 1 if c == last else 0
    if stable >= 5 and c > 3:
        break
    last = c
    time.sleep(2)
print(f"store settled: {last} logs", flush=True)

# Every copy carries the fixtures' sessions under fresh ids.
want = n * len(SIDS)
got = sql("SELECT count(DISTINCT COALESCE("
          "json_extract_string(log_attributes, '$.\"session.id\"'),"
          "json_extract_string(log_attributes, '$.\"conversation.id\"'))) AS n "
          "FROM otel.main.otlp_logs")
check(got and int(got[0]) >= want, "every copy's sessions stored", f"{got[0] if got else '?'} of {want}")

# Pick a session the copies made, and check the timeline opens it.
_, sl, _ = get("/sessions?range=all")
m = re.search(r"/sessions\?range=all&amp;session=([^&\"]+)\"", sl)
check(bool(m), "sessions list links timelines")
detail = f"/sessions?range=all&session={m.group(1)}" if m else "/sessions?range=all"

PAGES = ["/sessions", "/sessions?range=all", detail, "/spend", "/spend?range=all",
         "/tools", "/tools?range=all"]
for p in PAGES:
    lat, bad = [], []
    for _ in range(rounds):
        try:
            st, body, ms = get(p)
            lat.append(ms)
            # a failed query renders as a red box, never as an empty page
            if st != 200 or len(body) < 500 or "Internal Server Error" in body \
                    or 'class="qerr"' in body or "No agent sessions" in body:
                bad.append(st)
        except Exception as e:
            bad.append(type(e).__name__)
    p95 = statistics.quantiles(lat, n=20, method="inclusive")[-1] if len(lat) >= 2 else float("inf")
    check(not bad and p95 <= budget, f"{p} p95 under {budget:.0f}ms",
          f"p50 {statistics.median(lat):.0f}ms p95 {p95:.0f}ms max {max(lat):.0f}ms"
          + (f"; errors {bad[:3]}" if bad else ""))

_, sp, _ = get("/spend?range=all")
check("$" in sp, "spend shows cost over the replayed copies")
_, tl, _ = get("/tools?range=all")
check(">Read<" in tl or ">Bash<" in tl, "tools lists the replayed tool calls")

# Caps: every table within LOTEL_MAX_BYTES (the storage layer's cap).
def to_bytes(s):
    m = re.match(r"\s*([\d.]+)\s*([KMGT]?)i?B?\s*$", s, re.I)
    return float(m.group(1)) * 1024 ** "_KMGT".index((m.group(2) or "_").upper())
cap = to_bytes(max_bytes)
rows = sql("SELECT table_name, bytes FROM capped_tables('otel')")
sizes = dict(zip(rows[0::2], (int(float(b)) for b in rows[1::2]))) if rows else {}
check(bool(sizes) and all(v <= cap for v in sizes.values()), f"tables within {max_bytes}",
      ", ".join(f"{k} {v/1e6:.0f}MB" for k, v in sorted(sizes.items())) or "no sizes found")

# Memory: the live heap (macOS RSS over-reports freed pages; see soak.sh).
rss = subprocess.run(["ps", "-o", "rss=", "-p", pid], capture_output=True, text=True).stdout.strip()
live = None
try:
    out = subprocess.run(["vmmap", "-summary", pid], capture_output=True, text=True, timeout=90).stdout
    for line in out.splitlines():
        if line.startswith("DefaultMallocZone"):
            raw = line.split()[6]
            mult = 1 / 1048576 if raw[-1].isdigit() else {"K": 1 / 1024, "M": 1, "G": 1024}[raw[-1]]
            live = float(raw.rstrip("KMG")) * mult
except Exception:
    pass
limit_mb = to_bytes(mem_limit) / 1048576
check(live is not None and live < limit_mb, f"live heap under LOTEL_MEMORY_LIMIT ({mem_limit})",
      (f"{live:.0f} MB allocated" if live is not None else "vmmap probe failed")
      + (f"; RSS {int(rss)/1024:.0f} MB" if rss else ""))

check(re.search(rb"panic", open(logpath, "rb").read(), re.I) is None, "no panics")
print("agent-stress: FAILED" if fails else "agent-stress: all checks passed")
sys.exit(1 if fails else 0)
PY
RC=$?

SINCE=$(find ~/Library/Logs/DiagnosticReports -name 'lotel-*.ips' -newer "$TMP/.stress-start" 2>/dev/null | wc -l | tr -d ' ')
[ "$SINCE" = "0" ] || { echo "FAIL: $SINCE lotel crash report(s) written during the run"; RC=1; }
exit "$RC"
