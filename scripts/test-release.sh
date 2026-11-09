#!/bin/sh
# Dry-run the release pipeline on this machine, without tagging, pushing or publishing.
#
#   scripts/test-release.sh          # full: check URLs, build all targets, test the installer
#   scripts/test-release.sh --quick  # only check URLs, workflow syntax and the host build
#
# Steps (the same ones release.yml runs, in the same order):
#   1. vet + tests            2. cloudflared exists for every target
#   3. build-release.sh       4. every artifact exists, host binary runs --version
#   5. install.sh downloads the host binary from a local HTTP server and runs it
set -e
ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"
QUICK=; [ "${1:-}" = --quick ] && QUICK=1

pass() { printf '  ok   %s\n' "$*"; }
fail() { printf '  FAIL %s\n' "$*" >&2; exit 1; }
step() { printf '\n== %s\n' "$*"; }

TMP=$(mktemp -d)
SERVER_PID=
cleanup() { if [ -n "$SERVER_PID" ]; then kill "$SERVER_PID" 2>/dev/null || true; wait "$SERVER_PID" 2>/dev/null || true; fi; rm -rf "$TMP"; }
trap cleanup EXIT

TARGETS=$(sed -n 's/^TARGETS=\${TARGETS:-"\(.*\)"}$/\1/p' scripts/build-release.sh)
[ -n "$TARGETS" ] || fail "could not read TARGETS from scripts/build-release.sh"
[ -z "$QUICK" ] || TARGETS="$(go env GOOS)/$(go env GOARCH)"
HOST_OS=$(go env GOOS); HOST_ARCH=$(go env GOARCH)
VERSION=v0.0.0-localtest

step "1. vet and tests"
(cd app && go vet ./... && go test ./...) || fail "go vet/test"
pass "go vet ./... && go test ./..."

step "2. scripts and workflow"
for f in scripts/build.sh scripts/build-release.sh scripts/fetch-cloudflared.sh scripts/install.sh scripts/release.sh; do
    bash -n "$f" || fail "syntax error in $f"
done
pass "shell syntax"
if command -v actionlint >/dev/null 2>&1; then
    actionlint .github/workflows/*.yml || fail "actionlint"
    pass "actionlint"
else
    echo "  skip actionlint (brew install actionlint to check workflows)"
fi
grep -q 'scripts/build-release.sh' .github/workflows/release.yml || fail "release.yml does not call scripts/build-release.sh"
pass "release.yml uses scripts/build-release.sh"

step "3. cloudflared is published for every target"
base=https://github.com/cloudflare/cloudflared/releases/latest/download
for t in $TARGETS; do
    os=${t%/*}; arch=${t#*/}
    case "$os" in
        darwin)  file=cloudflared-darwin-$arch.tgz ;;
        linux)   file=cloudflared-linux-$arch ;;
        windows) file=cloudflared-windows-$arch.exe ;;
    esac
    code=$(curl -sIL -o /dev/null -w '%{http_code}' "$base/$file")
    [ "$code" = 200 ] || fail "$file -> HTTP $code (no cloudflared for $t; remove it from TARGETS)"
    pass "$file"
done

step "4. build ($TARGETS)"
DIST="$TMP/dist"
touch "$TMP/marker"
VERSION=$VERSION TARGETS="$TARGETS" scripts/build-release.sh "$DIST" >"$TMP/build.log" 2>&1 \
    || { cat "$TMP/build.log" >&2; fail "build-release.sh"; }
for t in $TARGETS; do
    os=${t%/*}; arch=${t#*/}; ext=; [ "$os" = windows ] && ext=.exe
    [ -s "$DIST/agent-browser-use-$os-$arch$ext" ] || fail "missing agent-browser-use-$os-$arch$ext"
    pass "agent-browser-use-$os-$arch$ext"
done
stray=$(find app -path app/cloudflared -prune -o -type f -name 'agent-browser-use-*' -newer "$TMP/marker" -print 2>/dev/null)
[ -z "$stray" ] || fail "a build wrote inside app/ (relative OUT?): $stray"
HOST_BIN="$DIST/agent-browser-use-$HOST_OS-$HOST_ARCH"
out=$("$HOST_BIN" -version 2>&1) || fail "host binary does not run"
echo "$out" | grep -q "$VERSION" || fail "host binary reports '$out', expected $VERSION"
pass "host binary runs: $out"

step "5. install.sh against a local server"
PORT=$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1])')
(cd "$DIST" && exec python3 -m http.server "$PORT" --bind 127.0.0.1 >/dev/null 2>&1) &
SERVER_PID=$!
sleep 1
DEST="$TMP/bin/agent-browser-use"
DEST=$DEST bash scripts/install.sh "http://127.0.0.1:$PORT/agent-browser-use-$HOST_OS-$HOST_ARCH" >"$TMP/install.log" 2>&1 \
    || { cat "$TMP/install.log" >&2; fail "install.sh"; }
[ -x "$DEST" ] || fail "install.sh did not leave an executable at $DEST"
pass "installed and runs: $("$DEST" -version 2>&1)"

# install.sh must ask for exactly the file names the workflow publishes.
want=agent-browser-use-$HOST_OS-$HOST_ARCH
grep -q 'agent-browser-use-\$OS-\$ARCH' scripts/install.sh || fail "install.sh no longer uses agent-browser-use-\$OS-\$ARCH"
[ -f "$DIST/$want" ] || fail "$want is not among the built artifacts"
pass "installer asset name matches build output ($want)"

printf '\nAll checks passed. Safe to run: make release V=vX.Y.Z\n'
