#!/usr/bin/env python3
"""otlp-gen — a dependency-free OTLP/HTTP traffic generator for lotel.

Streams realistic microservice telemetry into a running lotel instance so the
Explore page, the trace waterfall, and the metric charts show live data:

    api-gateway  ->  checkout  ->  payments

Each request is a real trace (parent/child spans sharing a trace id), some
requests fail and emit error logs linked to that trace, and metrics move over
time (random-walk gauge, monotonic sum, varying histograms).

Python stdlib only. OTLP/HTTP JSON, matching testdata/*.json.

Usage:
    scripts/otlp-gen.py                       # warm up + stream for 10 min
    scripts/otlp-gen.py --for 60              # stream for 60 seconds
    scripts/otlp-gen.py --rate 4 --interval 1 # 4 requests per tick
    scripts/otlp-gen.py --once                # one burst, then exit
    scripts/otlp-gen.py --token <HEX>         # override the bearer token

Token is read from --token, then $LOTEL_TOKEN, then scraped from /tmp/lotel.log.
"""
import argparse
import json
import os
import random
import re
import sys
import time
import urllib.error
import urllib.request

OTLP = os.environ.get("LOTEL_OTLP_URL", "http://127.0.0.1:4318")
import http.client
import urllib.parse

# One keep-alive connection for every POST, the way a real OTLP SDK sends.
# (The soak's 4 GB RSS scare traced to per-request churn inside the otlp
# extension that libmalloc then refused to give back — connection count
# wasn't the driver, but pooling is what a real exporter does anyway.)
_host, _port = urllib.parse.urlparse(OTLP).hostname, urllib.parse.urlparse(OTLP).port
UI = "http://127.0.0.1:8080"
NS = 1_000_000_000  # nanos per second


def now_ns() -> int:
    return int(time.time() * NS)


def hx(nbytes: int) -> str:
    return os.urandom(nbytes).hex()


def attr(key, val):
    """Encode an OTLP attribute; ints/floats/bools keep their typed value."""
    if isinstance(val, bool):
        return {"key": key, "value": {"boolValue": val}}
    if isinstance(val, int):
        return {"key": key, "value": {"intValue": str(val)}}
    if isinstance(val, float):
        return {"key": key, "value": {"doubleValue": val}}
    return {"key": key, "value": {"stringValue": str(val)}}


# ————————————————————————————————————————————————————————— traces ——————
# A tiny topology. Each request fans api-gateway -> checkout -> payments,
# sharing one trace id, with an occasional downstream failure.


def make_trace(now: int):
    tid = hx(16)
    gw, co, pay = hx(8), hx(8), hx(8)
    err = random.random() < 0.16
    slow = random.random() < 0.08

    pay_ms = random.uniform(15, 180) + (random.uniform(1500, 3800) if err else 0)
    co_ms = pay_ms + random.uniform(8, 70)
    gw_ms = co_ms + random.uniform(4, 50)
    if slow:
        gw_ms *= random.uniform(1.5, 3)

    # Root starts so its end lines up near `now`.
    gw_start = now - int(gw_ms * 1_000_000)
    co_start = gw_start + int(random.uniform(1, 8) * 1_000_000)
    pay_start = co_start + int(random.uniform(1, 8) * 1_000_000)
    rid = hx(6)

    def span(sid, parent, name, kind, svc, scope, start, ms, attrs, status):
        return {
            "span": {
                "traceId": tid,
                "spanId": sid,
                "parentSpanId": parent or "",
                "name": name,
                "kind": kind,
                "startTimeUnixNano": str(start),
                "endTimeUnixNano": str(start + int(ms * 1_000_000)),
                "attributes": attrs,
                "status": status,
            },
            "svc": svc,
            "scope": scope,
        }

    code = 500 if err else (202 if random.random() < 0.2 else 200)
    spans = [
        span(gw, None, "GET /api/checkout", 2, "api-gateway", "http.server", gw_start, gw_ms,
             [attr("http.method", "GET"), attr("http.target", "/api/checkout"),
              attr("http.status_code", code), attr("request.id", rid),
              attr("net.peer.ip", "10.0.%d.%d" % (random.randint(1, 8), random.randint(2, 254)))],
             {"code": 2, "message": "internal error"} if err else {"code": 0}),
        span(co, gw, "POST /checkout", 1, "checkout", "checkout.order", co_start, co_ms,
             [attr("http.method", "POST"), attr("checkout.items", random.randint(1, 7)),
              attr("checkout.currency", random.choice(["USD", "EUR", "GBP"])),
              attr("checkout.total_cents", random.randint(500, 45000))],
             {"code": 2} if err else {"code": 0}),
        span(pay, co, "POST /payments", 3, "payments", "checkout.payments", pay_start, pay_ms,
             [attr("http.method", "POST"), attr("payment.provider",
                                               random.choice(["stripe", "adyen", "braintree"])),
              attr("payment.amount_cents", random.randint(500, 45000))],
             {"code": 2, "message": "gateway timeout"} if err else {"code": 0}),
    ]

    # group spans by service into resourceSpans
    by_svc = {}
    for s in spans:
        by_svc.setdefault((s["svc"], s["scope"]), []).append(s["span"])
    resource_spans = []
    for (svc, scope), sps in by_svc.items():
        resource_spans.append({
            "resource": {"attributes": [attr("service.name", svc)]},
            "scopeSpans": [{"scope": {"name": scope, "version": "1.2.0"}, "spans": sps}],
        })
    return {"resourceSpans": resource_spans}, tid, gw, err


# ——————————————————————————————————————————————————————————— logs ——————
INFO_MSGS = [
    "payment authorized", "cart loaded", "session renewed", "cache hit",
    "inventory reserved", "shipping rate quoted", "coupon applied",
]
WARN_MSGS = [
    "cache miss ratio above 0.9", "retrying payment after transient error",
    "slow query detected", "connection pool near limit", "rate limit approaching",
]
ERR_MSGS = [
    "gateway timeout after 3 retries", "payment declined: issuer unavailable",
    "failed to reserve inventory", "upstream returned 500",
]


def make_logs(now: int, tid: str, sid: str, err: bool):
    recs = []

    def rec(svc, scope, sev_n, sev_t, body, linked, ex=None):
        r = {
            "timeUnixNano": str(now + random.randint(0, 50) * 1_000_000),
            "severityNumber": sev_n,
            "severityText": sev_t,
            "body": {"stringValue": body},
            "attributes": [],
        }
        if linked:
            r["traceId"] = tid
            r["spanId"] = sid
        if ex:
            r["attributes"].append(attr("exception.type", ex))
        return r, svc, scope

    if err:
        recs.append(rec("payments", "checkout.payments", 17, "ERROR",
                        random.choice(ERR_MSGS), True, "net.ConnectTimeout"))
        recs.append(rec("checkout", "checkout.order", 13, "WARN",
                        random.choice(WARN_MSGS), True))
    else:
        recs.append(rec("payments", "checkout.payments", 9, "INFO",
                        random.choice(INFO_MSGS), True))
    # a couple of ambient lines not tied to the trace
    for _ in range(random.randint(1, 2)):
        warn = random.random() < 0.25
        recs.append(rec("api-gateway", "http.server",
                        13 if warn else 9, "WARN" if warn else "INFO",
                        random.choice(WARN_MSGS if warn else INFO_MSGS), False))

    by_svc = {}
    for r, svc, scope in recs:
        by_svc.setdefault((svc, scope), []).append(r)
    resource_logs = []
    for (svc, scope), rr in by_svc.items():
        ra = [attr("service.name", svc)]
        if svc == "checkout":
            ra.append(attr("deployment.environment", "prod"))
        resource_logs.append({
            "resource": {"attributes": ra},
            "scopeLogs": [{"scope": {"name": scope, "version": "1.2.0"}, "logRecords": rr}],
        })
    return {"resourceLogs": resource_logs}


# ——————————————————————————————————————————————————————————— metrics ———
# Cumulative state so the monotonic sum keeps rising and the gauge walks.
STATE = {"queue": 42.0, "payments_total": 1200, "active": 60, "req_total": 0}


def make_metrics(now: int, err: bool):
    STATE["queue"] = max(0.0, min(250.0, STATE["queue"] + random.uniform(-4, 4.5)))
    STATE["payments_total"] += random.randint(0, 6) + (0 if err else random.randint(0, 9))
    STATE["active"] = max(1, int(STATE["active"] + random.randint(-6, 7)))
    STATE["req_total"] += random.randint(3, 20)

    # http request duration histogram, in ms, bounds [5,10,50,100,250]
    total = random.randint(80, 260)
    b = [random.randint(0, 60), random.randint(0, 90), random.randint(0, 70),
         random.randint(0, 30), random.randint(0, 10)]
    if err:
        b[-1] += random.randint(5, 25)
        b[-2] += random.randint(3, 15)
    mean = (sum(v * (lo + hi) / 2 for v, (lo, hi) in zip(
        b, [(0, 5), (5, 10), (10, 50), (50, 100), (100, 250)]))) / max(1, sum(b))

    checkout = {
        "resource": {"attributes": [attr("service.name", "checkout")]},
        "scopeMetrics": [{"scope": {"name": "checkout.order"}, "metrics": [
            {"name": "checkout.queue.depth", "unit": "{items}", "gauge": {
                "dataPoints": [{"timeUnixNano": str(now), "asDouble": round(STATE["queue"], 2)}]}},
            {"name": "checkout.payments.total", "unit": "{payments}", "sum": {
                "dataPoints": [{"timeUnixNano": str(now), "asInt": str(STATE["payments_total"])}],
                "aggregationTemporality": 2, "isMonotonic": True}},
        ]}],
    }
    gateway = {
        "resource": {"attributes": [attr("service.name", "api-gateway")]},
        "scopeMetrics": [{"scope": {"name": "http.server"}, "metrics": [
            {"name": "http.request.duration", "unit": "ms", "histogram": {
                "dataPoints": [{
                    "timeUnixNano": str(now), "count": str(sum(b)),
                    "sum": round(mean * sum(b), 2), "min": round(max(0.5, mean * 0.2), 2),
                    "max": round(mean * random.uniform(6, 20), 2),
                    "bucketCounts": [str(x) for x in b],
                    "explicitBounds": [5.0, 10.0, 50.0, 100.0, 250.0],
                }], "aggregationTemporality": 2}},
            {"name": "http.server.active", "unit": "{requests}", "exponentialHistogram": {
                "dataPoints": [{
                    "timeUnixNano": str(now), "count": str(STATE["active"]),
                    "sum": round(STATE["active"] * random.uniform(1.5, 3.5), 2),
                    "min": 0.05, "max": round(random.uniform(20, 60), 2),
                    "zeroCount": random.randint(0, 20), "scale": 2,
                    "positive": {"offset": 0, "bucketCounts": [
                        str(random.randint(5, 40)) for _ in range(4)]},
                    "negative": {"offset": 0, "bucketCounts": ["0", "0"]},
                }], "aggregationTemporality": 2}},
            {"name": "http.server.request.total", "unit": "{requests}", "sum": {
                "dataPoints": [{"timeUnixNano": str(now), "asInt": str(STATE["req_total"])}],
                "aggregationTemporality": 2, "isMonotonic": True}},
        ]}],
    }
    return {"resourceMetrics": [checkout, gateway]}


# ————————————————————————————————————————————————————————— transport ———
_conn = None


def post(token: str, path: str, payload):
    global _conn
    body = json.dumps(payload).encode()
    headers = {"Content-Type": "application/json",
               "Authorization": "Bearer " + token,
               "User-Agent": "lotel-otlp-gen/1.0",
               "Connection": "keep-alive"}
    for attempt in (1, 2):  # an idle keep-alive drop is normal: reconnect once
        try:
            if _conn is None:
                _conn = http.client.HTTPConnection(_host, _port, timeout=5)
            _conn.request("POST", path, body=body, headers=headers)
            r = _conn.getresponse()
            r.read()
            # OTLP receivers answer 200 or 202 (Accepted); both are success.
            if not 200 <= r.status < 300:
                sys.stderr.write("  ! %s -> HTTP %d\n" % (path, r.status))
                return False
            return True
        except Exception as e:  # noqa: BLE001 - demo tool, surface and continue
            if _conn is not None:
                try:
                    _conn.close()
                except Exception:
                    pass
                _conn = None
            if attempt == 2:
                sys.stderr.write("  ! %s -> %s\n" % (path, e))
                return False
    return False


def emit(token: str, ts_ns: int):
    """Emit one correlated burst (trace + linked logs + metrics) at ts_ns."""
    ok = 0
    traces = {"resourceSpans": []}
    errs = 0
    for _ in range(make_batch()):
        t, tid, gw_sid, err = make_trace(ts_ns)
        for rs in t["resourceSpans"]:
            traces["resourceSpans"].append(rs)
        if not post(token, "/v1/logs", make_logs(ts_ns, tid, gw_sid, err)):
            pass
        errs += err
    if post(token, "/v1/traces", traces):
        ok += 1
    if post(token, "/v1/metrics", make_metrics(ts_ns, errs > 0)):
        ok += 1
    return ok, errs


def make_batch():
    return BATCH


def resolve_token(cli):
    if cli:
        return cli
    if os.environ.get("LOTEL_TOKEN"):
        return os.environ["LOTEL_TOKEN"]
    try:
        log = open("/tmp/lotel.log").read()
        m = re.findall(r"bearer token:\s*([0-9a-f]{16,})", log)
        if m:
            return m[-1]
    except OSError:
        pass
    return None


def main():
    ap = argparse.ArgumentParser(description="OTLP traffic generator for lotel")
    ap.add_argument("--token")
    ap.add_argument("--for", dest="dur", type=float, default=600.0, help="seconds to stream")
    ap.add_argument("--interval", type=float, default=1.0, help="seconds per tick")
    ap.add_argument("--rate", type=int, default=3, help="requests (traces) per tick")
    ap.add_argument("--warmup", type=float, default=900.0,
                    help="minutes of history to backfill first (0 to skip)")
    ap.add_argument("--once", action="store_true", help="single burst then exit")
    a = ap.parse_args()

    global BATCH
    BATCH = a.rate

    token = resolve_token(a.token)
    if not token:
        sys.exit("no bearer token: pass --token, set $LOTEL_TOKEN, or run lotel and log to /tmp/lotel.log")

    print("otlp-gen -> %s  (token %s…)" % (OTLP, token[:8]))
    print("browse %s — try ranges 15m/1h as data streams in" % UI)

    # Backfill history so charts have a shape immediately.
    if a.warmup > 0 and not a.once:
        print("warming up %.0f min of history…" % a.warmup)
        step = 15  # one burst every 15s of "history"
        n = int(a.warmup * 60 // step)
        base = now_ns()
        for i in range(n, 0, -1):
            emit(token, base - i * step * NS)
            time.sleep(0.05)  # pace like an exporter, not like a flood
        print("  backfilled %d bursts" % n)

    if a.once:
        emit(token, now_ns())
        print("emitted one burst")
        return

    start = time.time()
    ticks = sent = fails = 0
    try:
        while time.time() - start < a.dur:
            ok, errs = emit(token, now_ns())
            ticks += 1
            sent += a.rate
            fails += errs
            left = int(a.dur - (time.time() - start))
            sys.stdout.write("\r  t+%3ds  %3d traces  %3d errored  (%ds left)   "
                             % (time.time() - start, sent, fails, left))
            sys.stdout.flush()
            time.sleep(a.interval)
    except KeyboardInterrupt:
        pass
    print("\ndone: %d ticks, ~%d traces, %d errored" % (ticks, sent, fails))


if __name__ == "__main__":
    main()
