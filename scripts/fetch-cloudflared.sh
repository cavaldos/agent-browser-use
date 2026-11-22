#!/bin/sh
# Download cloudflared for the platform you build for into app/cloudflared/, so that
# `cargo build` embeds it in the binary. Usage: scripts/fetch-cloudflared.sh [OS ARCH]
# OS: darwin|linux|windows, ARCH: amd64|arm64 (default: this machine).
set -e
ROOT=$(cd "$(dirname "$0")/.." && pwd)
case "$(uname -s)" in Darwin) HOST_OS=darwin ;; Linux) HOST_OS=linux ;; MINGW*|MSYS*|CYGWIN*) HOST_OS=windows ;; *) HOST_OS=$(uname -s | tr A-Z a-z) ;; esac
case "$(uname -m)" in x86_64|amd64) HOST_ARCH=amd64 ;; arm64|aarch64) HOST_ARCH=arm64 ;; *) HOST_ARCH=$(uname -m) ;; esac
OS=${1:-$HOST_OS}
ARCH=${2:-$HOST_ARCH}
DEST="$ROOT/app/cloudflared"
BASE=https://github.com/cloudflare/cloudflared/releases/latest/download
mkdir -p "$DEST"
rm -f "$DEST/cloudflared" "$DEST/cloudflared.exe"
case "$OS" in
  darwin)
    TMP=$(mktemp -d)
    curl -fsSL "$BASE/cloudflared-darwin-$ARCH.tgz" -o "$TMP/cf.tgz"
    tar -xzf "$TMP/cf.tgz" -C "$TMP"
    mv "$TMP/cloudflared" "$DEST/cloudflared"; rm -rf "$TMP" ;;
  linux)
    curl -fsSL "$BASE/cloudflared-linux-$ARCH" -o "$DEST/cloudflared" ;;
  windows)
    curl -fsSL "$BASE/cloudflared-windows-$ARCH.exe" -o "$DEST/cloudflared.exe" ;;
  *) echo "unsupported OS: $OS" >&2; exit 1 ;;
esac
chmod +x "$DEST"/cloudflared* 2>/dev/null || true
echo "cloudflared ($OS/$ARCH) ready in $DEST"
