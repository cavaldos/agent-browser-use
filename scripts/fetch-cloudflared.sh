#!/bin/sh
# Download cloudflared for the platform you build for into app/cloudflared/, so that
# `go build` embeds it in the binary. Usage: scripts/fetch-cloudflared.sh [GOOS GOARCH]
set -e
ROOT=$(cd "$(dirname "$0")/.." && pwd)
OS=${1:-$(go env GOOS)}
ARCH=${2:-$(go env GOARCH)}
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
