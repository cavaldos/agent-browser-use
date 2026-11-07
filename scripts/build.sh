#!/bin/sh
# Build the binary into bin/agent-browser-use
set -e
ROOT=$(cd "$(dirname "$0")/.." && pwd)
mkdir -p "$ROOT/bin"
cd "$ROOT/app"
CGO_ENABLED=0 go build -trimpath -ldflags="-s -w" -o "$ROOT/bin/agent-browser-use" .
echo "Built: $ROOT/bin/agent-browser-use"
