#!/bin/sh
# Build the binary.
#   scripts/build.sh                       -> bin/agent-browser-use for this machine
#   VERSION=v1.2.3 scripts/build.sh        -> stamp the version into the binary
#   TARGET_TRIPLE=x86_64-unknown-linux-gnu OUT=dist/x scripts/build.sh   -> build for another target into OUT
set -e
ROOT=$(cd "$(dirname "$0")/.." && pwd)
VERSION=${VERSION:-${AGENT_BROWSER_USE_VERSION:-dev}}
OUT=${OUT:-$ROOT/bin/agent-browser-use}
mkdir -p "$(dirname "$OUT")"

CRATE=$ROOT/app
export AGENT_BROWSER_USE_VERSION="$VERSION"
EXE=
case "${TARGET_TRIPLE:-}" in *windows*) EXE=.exe ;; esac
[ -n "${TARGET_TRIPLE:-}" ] || case "$(uname -s)" in MINGW*|MSYS*|CYGWIN*) EXE=.exe ;; esac
if [ -n "${TARGET_TRIPLE:-}" ]; then
    cargo build --release --target "$TARGET_TRIPLE" --manifest-path "$CRATE/Cargo.toml"
    SRC="$CRATE/target/$TARGET_TRIPLE/release/agent-browser-use$EXE"
else
    cargo build --release --manifest-path "$CRATE/Cargo.toml"
    SRC="$CRATE/target/release/agent-browser-use$EXE"
fi
[ -s "$SRC" ] || { echo "error: $SRC not produced" >&2; exit 1; }
cp "$SRC" "$OUT"
echo "Built: $OUT ($VERSION)"
