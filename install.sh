#!/bin/sh
#
# Kigi installer (macOS / Linux) — PRD F8.
#
# Downloads the matching platform artifact from this repo's GitHub Releases,
# verifies its SHA-256 against the release's SHA256SUMS manifest, and installs
# the binary as ~/.kigi/bin/kigi (the same managed layout the self-updater
# maintains: versioned binary in ~/.kigi/downloads/, atomic symlink in bin/).
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/ZacharyZhang-NY/Kigi-CLI/main/install.sh | sh
#   sh install.sh --version v0.1.0        # pin a specific release
#
# Environment:
#   KIGI_SHARE_DIR        install root (default: ~/.kigi)
#   KIGI_UPDATE_BASE_URL  GitHub-Releases-shaped API base (default:
#                         https://api.github.com/repos/ZacharyZhang-NY/Kigi-CLI/releases)
#
# Fails fast on any error; never leaves a partial binary as the active kigi.

set -eu

REPO="ZacharyZhang-NY/Kigi-CLI"
API_BASE="${KIGI_UPDATE_BASE_URL:-https://api.github.com/repos/${REPO}/releases}"
KIGI_HOME="${KIGI_SHARE_DIR:-$HOME/.kigi}"

err() {
    printf 'install.sh: error: %s\n' "$*" >&2
    exit 1
}

usage() {
    sed -n '2,20p' "$0" 2>/dev/null | sed 's/^# \{0,1\}//'
}

# ── Arguments ────────────────────────────────────────────────────────────────
VERSION=""
while [ $# -gt 0 ]; do
    case "$1" in
        --version)
            [ $# -ge 2 ] || err "--version requires an argument (e.g. --version v0.1.0)"
            VERSION="$2"
            shift
            ;;
        --version=*)
            VERSION="${1#--version=}"
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            err "unknown argument: $1 (supported: --version vX.Y.Z)"
            ;;
    esac
    shift
done
VERSION="${VERSION#v}"
if [ -n "$VERSION" ]; then
    case "$VERSION" in
        [0-9]*.[0-9]*.[0-9]*) ;;
        *) err "invalid version '$VERSION' (expected X.Y.Z or vX.Y.Z)" ;;
    esac
fi

# ── Platform detection ───────────────────────────────────────────────────────
OS="$(uname -s)"
ARCH="$(uname -m)"
case "$OS" in
    Darwin)
        PLATFORM_OS="macos"
        case "$ARCH" in
            arm64|aarch64) TRIPLE="aarch64-apple-darwin"; PLATFORM_ARCH="aarch64" ;;
            x86_64)        TRIPLE="x86_64-apple-darwin";  PLATFORM_ARCH="x86_64" ;;
            *) err "unsupported macOS architecture: $ARCH" ;;
        esac
        ;;
    Linux)
        PLATFORM_OS="linux"
        case "$ARCH" in
            arm64|aarch64) TRIPLE="aarch64-unknown-linux-gnu"; PLATFORM_ARCH="aarch64" ;;
            x86_64|amd64)  TRIPLE="x86_64-unknown-linux-gnu";  PLATFORM_ARCH="x86_64" ;;
            *) err "unsupported Linux architecture: $ARCH" ;;
        esac
        ;;
    *)
        err "unsupported OS: $OS (Windows: use install.ps1)"
        ;;
esac

# ── Downloader ───────────────────────────────────────────────────────────────
if command -v curl >/dev/null 2>&1; then
    fetch()        { curl -fsSL -o "$2" "$1"; }
    fetch_stdout() { curl -fsSL "$1"; }
elif command -v wget >/dev/null 2>&1; then
    fetch()        { wget -q -O "$2" "$1"; }
    fetch_stdout() { wget -q -O - "$1"; }
else
    err "either curl or wget is required"
fi

# ── SHA-256 tool ─────────────────────────────────────────────────────────────
if command -v sha256sum >/dev/null 2>&1; then
    sha256_of() { sha256sum "$1" | cut -d' ' -f1; }
elif command -v shasum >/dev/null 2>&1; then
    sha256_of() { shasum -a 256 "$1" | cut -d' ' -f1; }
else
    err "either sha256sum or shasum is required to verify the download"
fi

TMP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/kigi-install.XXXXXX")"
trap 'rm -rf "$TMP_DIR"' EXIT INT TERM

# ── Resolve the release ──────────────────────────────────────────────────────
if [ -n "$VERSION" ]; then
    RELEASE_URL="$API_BASE/tags/v$VERSION"
else
    RELEASE_URL="$API_BASE/latest"
fi
printf 'Resolving release from %s\n' "$RELEASE_URL"
RELEASE_JSON="$(fetch_stdout "$RELEASE_URL")" \
    || err "could not fetch release metadata from $RELEASE_URL"

TAG="$(printf '%s' "$RELEASE_JSON" \
    | tr ',' '\n' \
    | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' \
    | head -n 1)"
[ -n "$TAG" ] || err "release metadata has no tag_name (endpoint: $RELEASE_URL)"
RESOLVED_VERSION="${TAG#v}"
if [ -n "$VERSION" ] && [ "$RESOLVED_VERSION" != "$VERSION" ]; then
    err "requested version $VERSION but release tag is $TAG"
fi

ASSET="kigi-${RESOLVED_VERSION}-${TRIPLE}.tar.gz"

# Pull every browser_download_url out of the JSON, then select by asset name.
URLS="$(printf '%s' "$RELEASE_JSON" \
    | tr ',' '\n' \
    | sed -n 's/.*"browser_download_url"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p')"
ARCHIVE_URL="$(printf '%s\n' "$URLS" | grep -F "/$ASSET" | head -n 1 || true)"
SUMS_URL="$(printf '%s\n' "$URLS" | grep -F "/SHA256SUMS" | head -n 1 || true)"
[ -n "$ARCHIVE_URL" ] || err "release $TAG has no asset $ASSET (this platform may not be published yet)"
[ -n "$SUMS_URL" ] || err "release $TAG has no SHA256SUMS asset; refusing to install unverified binaries"

# ── Download + verify ────────────────────────────────────────────────────────
printf 'Downloading kigi v%s (%s)...\n' "$RESOLVED_VERSION" "$TRIPLE"
fetch "$ARCHIVE_URL" "$TMP_DIR/$ASSET" || err "download failed: $ARCHIVE_URL"
fetch "$SUMS_URL" "$TMP_DIR/SHA256SUMS" || err "download failed: $SUMS_URL"

EXPECTED=""
while IFS=' 	' read -r hash name; do
    name="${name#\*}"
    if [ "$name" = "$ASSET" ]; then
        EXPECTED="$hash"
    fi
done < "$TMP_DIR/SHA256SUMS"
[ -n "$EXPECTED" ] || err "SHA256SUMS has no entry for $ASSET"

ACTUAL="$(sha256_of "$TMP_DIR/$ASSET")"
if [ "$ACTUAL" != "$EXPECTED" ]; then
    err "SHA256 mismatch for $ASSET: expected $EXPECTED, got $ACTUAL"
fi
printf 'Checksum verified.\n'

# ── Extract + install ────────────────────────────────────────────────────────
tar -xzf "$TMP_DIR/$ASSET" -C "$TMP_DIR" || err "failed to extract $ASSET"
[ -f "$TMP_DIR/kigi" ] || err "archive $ASSET does not contain a 'kigi' binary"
chmod 0755 "$TMP_DIR/kigi"

DOWNLOADS_DIR="$KIGI_HOME/downloads"
BIN_DIR="$KIGI_HOME/bin"
mkdir -p "$DOWNLOADS_DIR" "$BIN_DIR"

# Versioned binary + atomic symlink swap — the exact layout the self-updater
# maintains, so `kigi update` takes over seamlessly from here.
VERSIONED="kigi-${RESOLVED_VERSION}-${PLATFORM_OS}-${PLATFORM_ARCH}"
mv -f "$TMP_DIR/kigi" "$DOWNLOADS_DIR/$VERSIONED"

TMP_LINK="$BIN_DIR/kigi.install.$$"
ln -s "../downloads/$VERSIONED" "$TMP_LINK"
mv -f "$TMP_LINK" "$BIN_DIR/kigi"

# Smoke-test the installed binary through the managed link.
"$BIN_DIR/kigi" --version >/dev/null 2>&1 \
    || err "installed binary failed to run; your PATH still has no working kigi"

printf '\nkigi v%s installed to %s\n' "$RESOLVED_VERSION" "$BIN_DIR/kigi"

# Persist a line into an rc file. Idempotent via the grep guard; on write
# failure the manual command is printed and the script fails loudly (the
# binary itself is already installed).
persist_line() {
    rc="$1"
    line="$2"
    guard="$3"
    what="$4"
    if [ -f "$rc" ] && grep -qF "$guard" "$rc"; then
        printf '\n%s is already configured in %s.\n' "$what" "$rc"
        return 0
    fi
    printf '\n# Added by the kigi installer\n%s\n' "$line" >> "$rc" \
        || err "could not write $rc — configure it manually: $line"
    printf '\nAdded %s in %s.\n' "$what" "$rc"
}

# Resolve the login shell's rc file and env-line syntax once; PATH and
# feature-flag persistence below both write to the same place.
case "${SHELL:-}" in
    */zsh)
        RC_FILE="${ZDOTDIR:-$HOME}/.zshrc"
        PATH_LINE="export PATH=\"$BIN_DIR:\$PATH\""
        GRAPH_LINE="export KIGI_GRAPH=1"
        ;;
    */bash)
        # macOS login shells read ~/.bash_profile; Linux reads ~/.bashrc.
        if [ "$PLATFORM_OS" = "macos" ]; then
            RC_FILE="$HOME/.bash_profile"
        else
            RC_FILE="$HOME/.bashrc"
        fi
        PATH_LINE="export PATH=\"$BIN_DIR:\$PATH\""
        GRAPH_LINE="export KIGI_GRAPH=1"
        ;;
    */fish)
        # fish_add_path in config.fish is fish's own idempotent way
        # to persist a PATH entry.
        FISH_CONF_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/fish"
        mkdir -p "$FISH_CONF_DIR"
        RC_FILE="$FISH_CONF_DIR/config.fish"
        PATH_LINE="fish_add_path $BIN_DIR"
        GRAPH_LINE="set -gx KIGI_GRAPH 1"
        ;;
    *)
        RC_FILE="$HOME/.profile"
        PATH_LINE="export PATH=\"$BIN_DIR:\$PATH\""
        GRAPH_LINE="export KIGI_GRAPH=1"
        ;;
esac

case ":$PATH:" in
    *":$BIN_DIR:"*)
        printf 'Run `kigi` to get started.\n'
        ;;
    *)
        persist_line "$RC_FILE" "$PATH_LINE" "$BIN_DIR" "$BIN_DIR (PATH)"
        printf 'Open a new terminal, then run `kigi` to get started.\n'
        ;;
esac

# Graph engineering ships enabled by default. The KIGI_GRAPH guard makes
# this idempotent AND respects an explicit user opt-out (an existing
# `export KIGI_GRAPH=0` line is left untouched). Disable any time with:
#   echo 'export KIGI_GRAPH=0' >> <your shell rc>
persist_line "$RC_FILE" "$GRAPH_LINE" "KIGI_GRAPH" "graph engineering (KIGI_GRAPH=1)"
