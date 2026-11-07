#!/bin/sh
# Run from source: viewer http://localhost:3000, MCP http://localhost:3000/mcp
# Extra flags are passed through, e.g. scripts/run.sh -port 8080
set -e
ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT/app"
exec go run . -profile "$ROOT/chrome-profile" "$@"
