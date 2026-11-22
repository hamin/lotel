#!/usr/bin/env bash
# Build the release tarball, checksums, and prove it runs.
#
#   just dist   →   dist/lotel-<ver>-<platform>.tar.gz + dist/SHA256SUMS.txt
#
# Two mandatory guards: build.rs stubs
# missing extension blobs with zero bytes, so a release built without real
# blobs ingests nothing. Guard 1 refuses a stubbed tree; guard 2 boots the
# *tarball's own binary* from a clean extract dir, POSTs testdata/logs.json,
# and requires /api/health ok plus a row count > 0.
#
# The tarball is deterministic (fixed mtimes, pinned ownership, LC_ALL=C
# member order, gzip -n): two runs on an unchanged tree are byte-identical.
#
# Port discipline: the sanity gate binds 127.0.0.1:18096/14396 (the owner's
# dev lotel owns 8080/4318 and 18095/14395 — this script never touches or
# kills anything it did not start). Busy ports fail with a clear message.
set -euo pipefail
cd "$(dirname "$0")/.."

DIST_UI_PORT="${DIST_UI_PORT:-18096}"
DIST_OTLP_PORT="${DIST_OTLP_PORT:-14396}"

say() { printf '%s\n' "$*"; }
die() { printf 'dist: %s\n' "$*" >&2; exit 1; }

# --- 1. Stub guard: a missing or zero-byte blob means build.rs stubbed it
# and the binary would silently ingest nothing.
for blob in capped otlp fts; do
    f="crates/lotel/extensions/$blob.duckdb_extension"
    [ -s "$f" ] || die "$f is missing or a 0-byte stub — run 'just fetch-extensions' first"
done
command -v cargo-about >/dev/null || die "cargo-about not installed (cargo install cargo-about) — required for THIRD-PARTY-LICENSES"

# --- 2. Release build.
say 'dist: cargo build --release'
cargo build --release

VERSION="$(awk -F'"' '/^version =/ {print $2; exit}' crates/lotel/Cargo.toml)"
[ -n "$VERSION" ] || die "could not read version from crates/lotel/Cargo.toml"
PLATFORM="$(uname -s | tr '[:upper:]' '[:lower:]')-$(uname -m | tr '[:upper:]' '[:lower:]')"
TARBALL="dist/lotel-$VERSION-$PLATFORM.tar.gz"

# --- 3. Stage the tarball payload.
rm -rf dist/stage
mkdir -p dist/stage/bin
cp target/release/lotel dist/stage/bin/lotel
cp LICENSE README.md CHANGELOG.md NOTICE dist/stage/
cp -R licenses dist/stage/licenses

# Regenerated every run from committed Cargo.lock + about.toml + about.hbs,
# so the shipped list can never go stale; dist/ is gitignored, keeping this
# pure build artifact out of git.
say 'dist: cargo about generate THIRD-PARTY-LICENSES'
cargo about generate about.hbs -o dist/stage/THIRD-PARTY-LICENSES

# install.sh is a required tarball member — no tarball
# without it.
[ -f scripts/install.sh ] || die "scripts/install.sh missing — required tarball member"
cp scripts/install.sh dist/stage/install.sh

# --- 4. Deterministic tar. Fixed mtime + pinned ownership + sorted member
# order + gzip -n ⇒ byte-identical reruns on an unchanged tree.
find dist/stage -exec touch -t 202601010000.00 {} +
MEMBERS="$(cd dist/stage && find . -type f | LC_ALL=C sort | sed 's|^\./||')"
rm -f "$TARBALL" dist/SHA256SUMS.txt
( cd dist/stage && tar -cf ../lotel-dist.tar --format=ustar \
    --uid 0 --gid 0 --uname root --gname root $MEMBERS )
gzip -n -9 -c dist/lotel-dist.tar > "$TARBALL"
rm -f dist/lotel-dist.tar

# Hash only this build's tarball (not a dist/*.tar.gz glob) so a
# stale tarball from an earlier version can never poison install.sh's sibling-file
# verification.
shasum -a 256 "$TARBALL" > dist/SHA256SUMS.txt

# --- 5. Sanity gate: boot the tarball's binary from a clean extract.
TMP="$(mktemp -d)"
SRV=""
cleanup() {
    if [ -n "$SRV" ] && kill -0 "$SRV" 2>/dev/null; then
        kill "$SRV" 2>/dev/null || true
    fi
    if [ -n "$SRV" ]; then
        wait "$SRV" 2>/dev/null || true
    fi
    rm -rf "$TMP"
}
trap cleanup EXIT

port_free() {
    python3 -c '
import socket, sys
s = socket.socket()
try:
    s.bind(("127.0.0.1", int(sys.argv[1])))
except OSError:
    sys.exit(1)
finally:
    s.close()
' "$1"
}

for p in "$DIST_UI_PORT" "$DIST_OTLP_PORT"; do
    port_free "$p" || die "sanity port 127.0.0.1:$p is busy — free it yourself (this script never kills anything it did not boot) or re-run with DIST_UI_PORT/DIST_OTLP_PORT"
done

say "dist: tarball contents ($TARBALL):"
tar -tzf "$TARBALL" | sed 's/^/    /'
say "dist: $(cat dist/SHA256SUMS.txt)"

EX="$TMP/extract"
mkdir -p "$EX"
tar -xzf "$TARBALL" -C "$EX"

TOKEN="lotel-dist-$(openssl rand -hex 16)"
export LOTEL_ADDR="127.0.0.1:$DIST_UI_PORT"
export LOTEL_OTLP_ADDR="127.0.0.1:$DIST_OTLP_PORT"
export LOTEL_DATA_DIR="$TMP/store"
export LOTEL_INGEST_TOKEN="$TOKEN"

"$EX/bin/lotel" >"$TMP/lotel.log" 2>&1 &
SRV=$!

HEALTH_OK=0
for _ in $(seq 1 50); do
    kill -0 "$SRV" 2>/dev/null || break
    if curl -fsS "http://127.0.0.1:$DIST_UI_PORT/api/health" 2>/dev/null | grep -q ok; then
        HEALTH_OK=1
        break
    fi
    sleep 0.2
done
if [ "$HEALTH_OK" -ne 1 ]; then
    say 'dist: FAIL — /api/health never reported ok; lotel log:'
    cat "$TMP/lotel.log" >&2
    exit 1
fi
say 'PASS  health  /api/health ok'

curl -fsS -X POST "http://127.0.0.1:$DIST_OTLP_PORT/v1/logs" \
    -H "Authorization: Bearer $TOKEN" \
    -H 'Content-Type: application/json' \
    --data "@testdata/logs.json" >/dev/null
say 'PASS  ingest  POST testdata/logs.json accepted'

say 'dist: waiting for rows to become visible…'
python3 - "$DIST_UI_PORT" <<'PY'
import re, sys, time, urllib.parse, urllib.request

B = "http://127.0.0.1:" + sys.argv[1]
sql = urllib.parse.quote("SELECT count(*) AS n FROM otel.main.otlp_logs")
url = B + "/query?mode=sql&sql=" + sql
deadline = time.time() + 60
last = ""
while time.time() < deadline:
    try:
        with urllib.request.urlopen(url, timeout=10) as r:
            last = r.read().decode()
    except Exception:
        time.sleep(1)
        continue
    if "Query error" in last:
        sys.exit("dist: FAIL — row-count query errored:\n" + last[:500])
    m = re.search(r"<td>\s*([0-9][0-9,]*)\s*</td>", last)
    if m and int(m.group(1).replace(",", "")) > 0:
        print("PASS  row count > 0 (n=%s)" % m.group(1))
        sys.exit(0)
    time.sleep(1)
sys.exit("dist: FAIL — timeout waiting for rows > 0; last page:\n" + last[:500])
PY

say "dist: $TARBALL + dist/SHA256SUMS.txt — sanity gate green"
