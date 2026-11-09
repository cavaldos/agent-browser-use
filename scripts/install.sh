#!/bin/sh
# Install agent-browser-use into ~/.local/bin. No root needed.
#
#   scripts/install.sh                 # latest release
#   scripts/install.sh v1.2.3          # a specific release
#   BIN_DIR=$HOME/bin scripts/install.sh
#
# Downloads the release for this OS/CPU from GitHub, checks its SHA-256 and copies the
# binary into BIN_DIR (default: $HOME/.local/bin).
set -e

REPO=${REPO:-cavaldos/agent-browser-use}
BIN_DIR=${BIN_DIR:-$HOME/.local/bin}
VERSION=${1:-latest}

die() { echo "error: $*" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || die "'$1' is required"; }
need curl; need tar; need uname

case "$(uname -s)" in
    Darwin) OS=darwin ;;
    Linux)  OS=linux ;;
    *) die "unsupported OS: $(uname -s) (download a binary from the Releases page instead)" ;;
esac
case "$(uname -m)" in
    x86_64|amd64)  ARCH=amd64 ;;
    arm64|aarch64) ARCH=arm64 ;;
    *) die "unsupported CPU: $(uname -m)" ;;
esac

if command -v shasum >/dev/null 2>&1; then
    sha256() { shasum -a 256 "$1" | cut -d' ' -f1; }
elif command -v sha256sum >/dev/null 2>&1; then
    sha256() { sha256sum "$1" | cut -d' ' -f1; }
else
    die "need shasum or sha256sum to verify the download"
fi

if [ "$VERSION" = latest ]; then
    BASE="https://github.com/$REPO/releases/latest/download"
    # The archive name carries the version, so ask GitHub which tag "latest" is.
    VERSION=$(curl -fsSI "https://github.com/$REPO/releases/latest" -o /dev/null -w '%{url_effective}' \
        | sed 's:.*/tag/::')
    case "$VERSION" in v*) ;; *) die "could not find the latest release of $REPO" ;; esac
else
    BASE="https://github.com/$REPO/releases/download/$VERSION"
fi

NAME="agent-browser-use_${VERSION}_${OS}_${ARCH}"
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

echo "Downloading $NAME ..."
curl -fsSL "$BASE/$NAME.tar.gz" -o "$TMP/$NAME.tar.gz" || die "no $VERSION build for $OS/$ARCH"
curl -fsSL "$BASE/SHA256SUMS" -o "$TMP/SHA256SUMS"

expected=$(grep " $NAME.tar.gz\$" "$TMP/SHA256SUMS" | cut -d' ' -f1)
[ -n "$expected" ] || die "$NAME.tar.gz is not listed in SHA256SUMS"
[ "$(sha256 "$TMP/$NAME.tar.gz")" = "$expected" ] || die "checksum mismatch, download discarded"

tar -xzf "$TMP/$NAME.tar.gz" -C "$TMP"
mkdir -p "$BIN_DIR"
install -m 755 "$TMP/$NAME/agent-browser-use" "$BIN_DIR/agent-browser-use" 2>/dev/null \
    || { cp "$TMP/$NAME/agent-browser-use" "$BIN_DIR/agent-browser-use" && chmod 755 "$BIN_DIR/agent-browser-use"; }

echo "Installed: $BIN_DIR/agent-browser-use ($VERSION)"
case ":$PATH:" in
    *":$BIN_DIR:"*) ;;
    *) echo "Add it to your PATH (zsh/bash):  echo 'export PATH=\"$BIN_DIR:\$PATH\"' >> ~/.zshrc" ;;
esac
