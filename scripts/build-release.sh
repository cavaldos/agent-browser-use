#!/bin/sh
# Build every release binary into a directory: scripts/build-release.sh [DIST_DIR]
# Used by .github/workflows/release.yml and by scripts/test-release.sh, so CI and local
# runs build the exact same set. Output names: agent-browser-use-<os>-<arch>[.exe]
#   VERSION=v1.2.3 scripts/build-release.sh dist
#   TARGETS="linux/amd64 darwin/arm64" scripts/build-release.sh /tmp/out
set -e
ROOT=$(cd "$(dirname "$0")/.." && pwd)
VERSION=${VERSION:-dev}
DIST=${1:-$ROOT/dist}
# cloudflared publishes no windows/arm64 build, so it is not a target.
TARGETS=${TARGETS:-"darwin/arm64 darwin/amd64 linux/amd64 linux/arm64 windows/amd64"}

mkdir -p "$DIST"
DIST=$(cd "$DIST" && pwd)   # absolute: build.sh runs go build inside app/

for target in $TARGETS; do
    os=${target%/*}; arch=${target#*/}
    ext=; [ "$os" = windows ] && ext=.exe
    out="$DIST/agent-browser-use-$os-$arch$ext"
    # Embed cloudflared for the target so -tunnel works in the released binary.
    "$ROOT/scripts/fetch-cloudflared.sh" "$os" "$arch"
    GOOS=$os GOARCH=$arch VERSION=$VERSION OUT=$out "$ROOT/scripts/build.sh"
    # Fail here instead of publishing an empty release.
    [ -s "$out" ] || { echo "error: $out was not produced" >&2; exit 1; }
done
ls -la "$DIST"
