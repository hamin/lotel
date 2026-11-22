#!/usr/bin/env python3
"""Post today's usage numbers for lotel to lotel.io/api/stats.

    STATS_URL=https://lotel-io.pages.dev/api/stats STATS_TOKEN=... \\
    GITHUB_TOKEN=... python3 scripts/stats-snapshot.py [--dry-run]

Metrics (all running totals, as of today):
  stars, forks, watchers                   the GitHub repo
  release.<tag>.<file>                     download count of each release asset
  release.downloads                        the sum over all assets
  crates.downloads, crates.recent          crates.io (0 until the crate is published)
And, when TRAFFIC_TOKEN is set (a token that can read repo traffic, which the
workflow's GITHUB_TOKEN cannot), GitHub's per-day traffic for the last 14 days,
posted under each of those days:
  traffic.views, traffic.views_unique, traffic.clones, traffic.clones_unique

The .github/workflows/stats.yml workflow runs this daily. Standard library only.
"""
import datetime
import json
import os
import sys
import urllib.error
import urllib.request

REPO = os.environ.get("STATS_REPO", "hamin/lotel")
CRATE = "lotel"
UA = "lotel-stats-snapshot"


def get(url, token=None):
    req = urllib.request.Request(url, headers={"User-Agent": UA, "Accept": "application/json"})
    if token:
        req.add_header("Authorization", f"Bearer {token}")
    with urllib.request.urlopen(req, timeout=30) as r:
        return json.load(r)


def gh(path, token):
    return get(f"https://api.github.com{path}", token)


def collect(today):
    tok = os.environ.get("GITHUB_TOKEN")
    days = {today: {}}
    m = days[today]

    repo = gh(f"/repos/{REPO}", tok)
    m["stars"] = repo["stargazers_count"]
    m["forks"] = repo["forks_count"]
    m["watchers"] = repo["subscribers_count"]

    total, page = 0, 1
    while True:
        rels = gh(f"/repos/{REPO}/releases?per_page=100&page={page}", tok)
        for rel in rels:
            for a in rel.get("assets", []):
                m[f"release.{rel['tag_name']}.{a['name']}"] = a["download_count"]
                total += a["download_count"]
        if len(rels) < 100:
            break
        page += 1
    m["release.downloads"] = total

    try:
        c = get(f"https://crates.io/api/v1/crates/{CRATE}")["crate"]
        m["crates.downloads"] = c.get("downloads") or 0
        m["crates.recent"] = c.get("recent_downloads") or 0
    except urllib.error.HTTPError as e:
        if e.code != 404:
            raise
        m["crates.downloads"] = m["crates.recent"] = 0

    traffic = os.environ.get("TRAFFIC_TOKEN")
    if traffic:
        for kind in ("views", "clones"):
            data = gh(f"/repos/{REPO}/traffic/{kind}?per=day", traffic)
            for d in data.get(kind, []):
                day = d["timestamp"][:10]
                days.setdefault(day, {})
                days[day][f"traffic.{kind}"] = d["count"]
                days[day][f"traffic.{kind}_unique"] = d["uniques"]
    return days


def post(url, token, day, metrics):
    body = json.dumps({"day": day, "metrics": metrics}).encode()
    req = urllib.request.Request(url, data=body, method="POST", headers={
        "User-Agent": UA, "Content-Type": "application/json", "Authorization": f"Bearer {token}"})
    with urllib.request.urlopen(req, timeout=30) as r:
        return json.load(r)


def main():
    today = datetime.datetime.now(datetime.timezone.utc).date().isoformat()
    days = collect(today)
    if "--dry-run" in sys.argv:
        print(json.dumps(days, indent=1, sort_keys=True))
        return
    url, token = os.environ["STATS_URL"], os.environ["STATS_TOKEN"]
    for day in sorted(days):
        print(day, post(url, token, day, days[day]))


if __name__ == "__main__":
    main()
