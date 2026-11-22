#!/usr/bin/env bash
# Fetch the extension blobs the binary embeds at compile time
# (include_bytes! in db.rs) into crates/lotel/extensions/.
#
#   just fetch-extensions          # or ./scripts/fetch-extensions.sh
#
# All three come from extensions.lock: pinned URL + sha256 per platform,
# downloaded and verified, so a silent miss is not possible (build.rs would
# otherwise stub the blob with zero bytes and the binary would ingest nothing).
#
# CAPPED_EXT=/path/to/capped.duckdb_extension uses a local capped build
# instead of the pinned release (for working on capped itself).
#
# Platform is auto-detected (override with LOTEL_EXT_PLATFORM=… for a
# deliberate cross-fetch).
set -euo pipefail
cd "$(dirname "$0")/.."

DEST="crates/lotel/extensions"
LOCK="crates/lotel/extensions.lock"

say() { printf '%s\n' "$*"; }
die() { printf 'fetch-extensions: %s\n' "$*" >&2; exit 1; }

[ -f "$LOCK" ] || die "$LOCK missing — it is the source of truth for pinned blobs"

# --- 1. Platform (duckdb's naming: the registry paths use it verbatim).
case "$(uname -s)/$(uname -m)" in
    Darwin/arm64)  platform=osx_arm64 ;;
    Darwin/x86_64) platform=osx_amd64 ;;
    Linux/aarch64) platform=linux_arm64 ;;
    Linux/x86_64)  platform=linux_amd64 ;;
    *) die "unsupported platform $(uname -s)/$(uname -m) — set LOTEL_EXT_PLATFORM explicitly if extensions.lock has rows for it" ;;
esac
platform="${LOTEL_EXT_PLATFORM:-$platform}"

# --- 2. Lock lookup: exactly one row for <ext> <platform> → "sha256 url".
lock_row() { # <ext> <platform>
    local row
    row="$(awk -v e="$1" -v p="$2" '$1==e && $2==p {n++; if (n > 1) exit 2; print $3, $4}' "$LOCK")" \
        || die "extensions.lock: duplicate rows for $1 $2 — refusing to guess"
    [ -n "$row" ] || die "extensions.lock has no row for $1 $2"
    printf '%s\n' "$row"
}

hash_file() {
    # Trust sha256sum only if it actually runs: PATH may hold a foreign-arch
    # coreutils that dies with "Bad CPU type" on this Mac; fall back to the
    # system shasum (always present on macOS, and any Linux worth building on
    # ships coreutils anyway).
    if sha256sum "$1" >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    else
        shasum -a 256 "$1" | awk '{print $1}'
    fi
}

mkdir -p "$DEST"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# --- 3. download → gunzip → verify sha256 → install atomically.
for ext in otlp fts capped; do
    if [ "$ext" = capped ] && [ -n "${CAPPED_EXT:-}" ]; then
        [ -s "$CAPPED_EXT" ] || die "CAPPED_EXT=$CAPPED_EXT is missing or empty"
        say "fetch-extensions: capped $platform ← $CAPPED_EXT (local override, not hash-checked)"
        cp "$CAPPED_EXT" "$DEST/capped.duckdb_extension"
        continue
    fi
    read -r want url <<<"$(lock_row "$ext" "$platform")"
    case "$url" in
        http://*|https://*) ;;
        *) die "extensions.lock row for $ext $platform has url '$url' — only http(s) downloads are supported for $ext" ;;
    esac
    say "fetch-extensions: $ext $platform ← $url"
    curl -fsSL --retry 3 --retry-all-errors -o "$TMP/$ext.dl" "$url"
    case "$url" in
        *.gz) gunzip -c "$TMP/$ext.dl" >"$TMP/$ext" ;;
        *)    mv "$TMP/$ext.dl" "$TMP/$ext" ;;
    esac
    [ -s "$TMP/$ext" ] || die "$ext $platform: downloaded blob is empty"
    got="$(hash_file "$TMP/$ext")"
    [ "$got" = "$want" ] || die "$ext $platform: sha256 mismatch
  expected $want (extensions.lock)
  got      $got
  from $url"
    mv "$TMP/$ext" "$DEST/$ext.duckdb_extension"
    say "fetch-extensions: $ext $platform ok (sha256 $got)"
done

# --- 4. Final gate: nothing stub-shaped may leave this script.
for blob in capped otlp fts; do
    f="$DEST/$blob.duckdb_extension"
    [ -s "$f" ] || die "$f is missing or 0 bytes after fetch — refusing to leave a stub behind"
done
say "fetch-extensions: installed for $platform:"
for blob in capped otlp fts; do
    f="$DEST/$blob.duckdb_extension"
    say "  $(hash_file "$f")  $(wc -c <"$f" | tr -d ' ')B  $f"
done
