#!/bin/sh
# lotel installer — POSIX sh, no bashisms: strangers curl-pipe this
# with whatever /bin/sh they have.
#
#   install.sh --local <tarball> [--prefix DIR]
#       Offline install from a release tarball built by `just dist`.
#       Verifies against a sibling SHA256SUMS.txt when one is present,
#       extracts, and installs bin/lotel into DIR (default ~/.local/bin,
#       falling back to /usr/local/bin via sudo when it isn't writable).
#       --prefix never escalates: an unwritable DIR is an error, not a sudo.
#
#   install.sh <version> [--prefix DIR]        (GitHub Releases mode)
#       Downloads
#       https://github.com/hamin/lotel/releases/download/v<VER>/lotel-<VER>-<platform>.tar.gz
#       and verifies it against that release's SHA256SUMS.txt. The repo is
#       configurable with LOTEL_REPO="owner/repo". With no version it errors
#       and points at the releases page.
#
# A checksum mismatch aborts before anything is extracted or written.
set -eu

name=lotel
prog=${0##*/}
repo=${LOTEL_REPO:-hamin/lotel}
release_base=https://github.com/$repo/releases/download

TMP=
say() { printf '%s\n' "$*"; }
warn() { printf '%s: warning: %s\n' "$prog" "$*" >&2; }
die() { printf '%s: %s\n' "$prog" "$*" >&2; exit 1; }
cleanup() { [ -z "$TMP" ] || rm -rf "$TMP"; }
trap cleanup EXIT

usage() {
    cat <<'EOF'
usage:
  install.sh --local <tarball> [--prefix DIR]   offline install from a release tarball
  install.sh <version>   [--prefix DIR]         install a GitHub release, e.g. 0.1.0

Environment:
  LOTEL_REPO=owner/repo   which GitHub repo to install from (default: hamin/lotel)
EOF
    exit "${1:-1}"
}

# --- platform, the same shape dist.sh stamps into the tarball name.
platform() {
    s=$(uname -s) || die "uname -s failed"
    m=$(uname -m) || die "uname -m failed"
    printf '%s-%s\n' \
        "$(printf '%s' "$s" | tr '[:upper:]' '[:lower:]')" \
        "$(printf '%s' "$m" | tr '[:upper:]' '[:lower:]')"
}

# --- first hash tool available (dist.sh writes SHA256SUMS.txt with shasum).
have_hash_tool() {
    command -v sha256sum >/dev/null 2>&1 || command -v shasum >/dev/null 2>&1
}

# A tool on PATH can still be broken (e.g. a wrong-arch sha256sum on PATH),
# so each is only trusted if it actually runs.
hash_of() {
    have_hash_tool || die "no sha256sum/shasum available — cannot verify the download"
    if out=$(sha256sum "$1" 2>/dev/null); then
        :
    elif out=$(shasum -a 256 "$1" 2>/dev/null); then
        :
    else
        die "sha256 verification could not run (sha256sum/shasum broken?) — refusing to install $1"
    fi
    printf '%s\n' "${out%% *}"
}

# fetch URL OUT — quietly, failing on HTTP errors (404s must not save a body).
fetch() {
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL --retry 2 -o "$2" "$1"
    elif command -v wget >/dev/null 2>&1; then
        wget -q -O "$2" "$1"
    else
        die "need curl or wget to download"
    fi
}

# Compare the tarball's sha256 against its SUMS file. Entries are matched by
# basename: dist.sh records the path it hashed (dist/lotel-…-….tar.gz).
verify_against() {
    have_hash_tool || die "found $2 but no sha256sum/shasum to check it with"
    file=${1##*/}
    expected=$(awk -v f="$file" '$2 == f || $2 ~ ("[^/]*" f "$") { print $1; exit }' "$2")
    [ -n "$expected" ] || die "$2 has no entry for $file — refusing to install unverified"
    actual=$(hash_of "$1")
    [ "$actual" = "$expected" ] || die "SHA256 mismatch for $file: got $actual, expected $expected from $2 — refusing to install (corrupt or tampered)"
    say "install: sha256 ok  $file"
}

# Extract to the temp dir and validate it looks like a lotel release; leaves
# the binary path in BIN.
extract_tarball() {
    TMP=$(mktemp -d "${TMPDIR:-/tmp}/lotel-install.XXXXXX") || die "mktemp -d failed"
    tar -xzf "$1" -C "$TMP" || die "could not extract $1 (corrupt, or not a .tar.gz)"
    BIN="$TMP/bin/$name"
    [ -f "$BIN" ] || die "$1 does not contain bin/$name — not a lotel release tarball"
}

# Stage + rename, so a failed write never leaves a half-written binary.
install_bin() {
    cp "$1" "$2/$name.tmp.$$" || die "cannot write into $2"
    chmod 0755 "$2/$name.tmp.$$" || die "cannot chmod the staged binary in $2"
    mv -f "$2/$name.tmp.$$" "$2/$name" || {
        rm -f "$2/$name.tmp.$$"
        die "could not move the binary into place in $2"
    }
    say "install: $name → $2/$name"
}

announce() {
    case ":$PATH:" in
        *":$1:"*) ;;
        *) warn "$1 is not on your PATH — add it, or run $1/$name directly" ;;
    esac
    say ''
    say 'Next steps:'
    say "  1. run:  $name"
    say '     it starts the UI and the OTLP receiver, and prints the bearer'
    say '     token in its startup banner'
    say '  2. copy that token into your OTLP exporter'
    say '     (Authorization: Bearer <token>)'
    say '  3. open the UI at the URL the banner prints'
}

# --- argument parsing.
mode=
tarball=
version=
prefix=
while [ $# -gt 0 ]; do
    case $1 in
        --local)
            [ $# -ge 2 ] || die "--local needs a tarball path (see --help)"
            [ -z "$mode" ] || die "--local given twice"
            mode=local
            tarball=$2
            shift 2
            ;;
        --prefix)
            [ $# -ge 2 ] || die "--prefix needs a directory"
            [ -z "$prefix" ] || die "--prefix given twice"
            prefix=$2
            shift 2
            ;;
        -h | --help)
            usage 0
            ;;
        --*)
            die "unknown option: $1 (see --help)"
            ;;
        *)
            [ -z "$version" ] || die "unexpected argument: $1 (see --help)"
            version=$1
            shift
            ;;
    esac
done

plat=$(platform)

if [ "$mode" = local ]; then
    # --- offline mode: verify, extract, install. Must work with no network.
    [ -z "$version" ] || die "unexpected argument: $version (use --prefix DIR)"
    [ -f "$tarball" ] || die "no such tarball: $tarball"
    case $tarball in
        */*) sums=${tarball%/*}/SHA256SUMS.txt ;;
        *) sums=SHA256SUMS.txt ;;
    esac
    if [ -f "$sums" ]; then
        verify_against "$tarball" "$sums"
    else
        warn "no SHA256SUMS.txt next to the tarball — installing unverified"
    fi
    extract_tarball "$tarball"
else
    # --- GitHub Releases mode.
    if [ -z "$version" ]; then
        die "give a version to install, e.g. '$prog 0.1.0'
     (releases: https://github.com/$repo/releases), or build a tarball with
     'just dist' and run: $prog --local dist/lotel-<version>-$plat.tar.gz"
    fi
    v=${version#v}
    case $v in
        '' | *[!0-9A-Za-z.+-]*) die "bad version: $version" ;;
    esac
    case $repo in
        */*/*) die "LOTEL_REPO must be owner/repo, got: $repo" ;;
        */*) ;;
        *) die "LOTEL_REPO must be owner/repo, got: $repo" ;;
    esac
    file="lotel-$v-$plat.tar.gz"
    TMP=$(mktemp -d "${TMPDIR:-/tmp}/lotel-install.XXXXXX") || die "mktemp -d failed"
    url="$release_base/v$v/$file"
    say "install: downloading $url"
    fetch "$url" "$TMP/$file" || die "could not download $url (does release v$v exist in $repo?)"
    if fetch "$release_base/v$v/SHA256SUMS.txt" "$TMP/SHA256SUMS.txt" 2>/dev/null; then
        verify_against "$TMP/$file" "$TMP/SHA256SUMS.txt"
    else
        warn "release has no SHA256SUMS.txt asset — installing unverified"
    fi
    extract_tarball "$TMP/$file"
fi

# --- install.
if [ -n "$prefix" ]; then
    case $prefix in
        "~") prefix=$HOME ;;
        "~/"*) prefix="$HOME/${prefix#~/}" ;;
    esac
    while [ "$prefix" != "/" ] && [ "${prefix%/}" != "$prefix" ]; do
        prefix=${prefix%/}
    done
    [ -n "$prefix" ] || die "--prefix must not be empty"
    [ -d "$prefix" ] || mkdir -p "$prefix" || die "cannot create $prefix"
    [ -w "$prefix" ] || die "not writable: $prefix — pass a writable --prefix DIR"
    install_bin "$BIN" "$prefix"
else
    prefix="$HOME/.local/bin"
    if mkdir -p "$prefix" 2>/dev/null && [ -w "$prefix" ]; then
        install_bin "$BIN" "$prefix"
    else
        # sudo fallback, default prefix only — --prefix never reaches sudo.
        warn "cannot write $prefix — falling back to /usr/local/bin (sudo)"
        command -v sudo >/dev/null 2>&1 || die "no sudo available; pass --prefix DIR"
        sudo mkdir -p /usr/local/bin || die "could not create /usr/local/bin"
        sudo cp "$BIN" "/usr/local/bin/$name.tmp.$$" || die "could not stage the binary"
        sudo chmod 0755 "/usr/local/bin/$name.tmp.$$" || die "could not chmod the binary"
        sudo mv -f "/usr/local/bin/$name.tmp.$$" "/usr/local/bin/$name" ||
            die "could not move the binary into /usr/local/bin"
        prefix=/usr/local/bin
        say "install: $name → $prefix/$name"
    fi
fi

announce "$prefix"
