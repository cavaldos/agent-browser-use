#!/bin/sh
# Build the binary.
#   scripts/build.sh                       -> bin/agent-browser-use for this machine
#   VERSION=v1.2.3 scripts/build.sh        -> stamp the version into the binary
#   GOOS=linux GOARCH=amd64 OUT=dist/x scripts/build.sh   -> cross-compile to OUT
set -e
ROOT=$(cd "$(dirname "$0")/.." && pwd)
VERSION=${VERSION:-dev}
OUT=${OUT:-$ROOT/bin/agent-browser-use}
mkdir -p "$(dirname "$OUT")"
cd "$ROOT/app"
CGO_ENABLED=0 go build -trimpath -ldflags="-s -w -X main.version=$VERSION" -o "$OUT" .
echo "Built: $OUT ($VERSION)"
