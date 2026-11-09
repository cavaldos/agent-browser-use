#!/usr/bin/env bash
# Install agent-browser-use without sudo: scripts/install.sh [asset-url-or-tag]
# Defaults to ~/.local/bin (user-writable). Same layout as pitago's script/install.sh.
set -euo pipefail
REPO=${REPO:-cavaldos/agent-browser-use}
DEST="${DEST:-$HOME/.local/bin/agent-browser-use}"
mkdir -p "$(dirname "$DEST")"

OS="$(uname -s | tr '[:upper:]' '[:lower:]')"; ARCH="$(uname -m)"
[[ "$ARCH" == x86_64 ]] && ARCH=amd64
[[ "$ARCH" == aarch64 ]] && ARCH=arm64
[[ "$OS" == darwin ]] || OS=linux

if [[ "${1:-}" == http* ]]; then
  URL="$1"
elif [[ "${1:-}" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  URL="https://github.com/$REPO/releases/download/$1/agent-browser-use-$OS-$ARCH"
else
  URL="https://github.com/$REPO/releases/latest/download/agent-browser-use-$OS-$ARCH"
fi

curl -fL -o "$DEST" "$URL" || { rm -f "$DEST"; echo "error: download failed: $URL" >&2; exit 1; }
chmod +x "$DEST"
case ":$PATH:" in *":$(dirname "$DEST"):"*) ;; *)
  echo "note: add to PATH: export PATH=\"$(dirname "$DEST"):\$PATH\"" >&2 ;;
esac
echo "installed $DEST"
"$DEST" --version 2>/dev/null || true
