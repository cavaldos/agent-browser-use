#!/usr/bin/env bash
# Parity harness: record how a binary behaves on its public surface.
# run.sh <binary> <outdir>
# Records into <outdir>/ (CLI output) and <outdir>/{nobrowser,browser}/ (server):
# version.txt, help.txt, bad-flag.txt ... CLI output and exit codes
# static.sha256 sha256 of every file served from /
# mcp-http-*.json MCP over HTTP (POST /mcp)
# mcp-stdio.jsonl MCP over stdio (`<binary> mcp ...`)
# api-*.json / api-*.meta GET /api/* (body normalised with jq; status + content-type)
# auth-*.txt password protection (-pw)
# ws-hello.json WebSocket hello sequence (needs node >= 22)
# "nobrowser" runs with -no-browser. "browser" starts the real shared browser and is skipped when it
# cannot start (PARITY_SKIP_BROWSER=1 skips it on purpose, CHROME=/path picks the executable).
set -u
BIN=${1:?usage: run.sh <binary> <outdir>}
OUT=${2:?usage: run.sh <binary> <outdir>}
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
BIN=$(cd "$(dirname "$BIN")" && pwd)/$(basename "$BIN")
mkdir -p "${OUT:?}"
OUT=$(cd "$OUT" && pwd)
command -v jq >/dev/null || { echo "jq is required" >&2; exit 1; }

PIDS=()
TMPS=()
cleanup() {
 local p t
 for p in ${PIDS[@]+"${PIDS[@]}"}; do kill "$p" 2>/dev/null; done
 sleep 0.5
 for p in ${PIDS[@]+"${PIDS[@]}"}; do kill -9 "$p" 2>/dev/null; done
 for t in ${TMPS[@]+"${TMPS[@]}"}; do [ -n "$t" ] && rm -rf "${t:?}"; done
}
trap cleanup EXIT

free_port() { python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close()'; }

# Replace everything that changes between runs (tab titles/urls/favicons depend on the live network).
NORM='
 def scrub:
 walk(if type == "object" then
 (if has("ip") then .ip = "<ip>" else . end)
 | (if has("loc") then .loc = "<loc>" else . end)
 | (if has("checking") then .checking = "<bool>" else . end)
 | (if has("id") and (.id|type) == "string" and (.id|test("^[0-9a-f]{6,16}$")) then .id = "<id>" else . end)
 | (if has("time") and (.time|type) == "number" then .time = 0 else . end)
 | (if has("color") then .color = "<color>" else . end)
 | (if has("path") then .path = "<path>" else . end)
 | (if has("id") and has("url") and has("title") then del(.title, .url, .favicon, .loading, .audible, .muted, .canBack, .canFwd) else . end)
 elif type == "string" then
 (if test("^[0-9A-F]{32}$") then "<tab-id>" else . end)
 | gsub("#[0-9a-f]{4}$"; "#<hex4>")
 else . end);
 scrub'
norm_json() { jq -S "$NORM" 2>/dev/null || cat; }

wait_http() { # port seconds
 local i
 for ((i = 0; i < $2 * 10; i++)); do
 curl -s -o /dev/null -m 1 "http://127.0.0.1:$1/" && return 0
 sleep 0.1
 done
 return 1
}

SID=""
rpc() { # port file json [session] -> normalised body in <file>, status/headers in <file>.meta; sets SID
 local port=$1 f=$2 body=$3 sid=${4:-}
 local args=(-s -m 20 -D "$f.hdr" -o "$f.raw" -X POST "http://127.0.0.1:$port/mcp" -H 'Content-Type: application/json' -H 'Accept: application/json, text/event-stream')
 [ -n "$sid" ] && args+=(-H "Mcp-Session-Id: $sid")
 curl "${args[@]}" -d "$body"
 local rc=$?
 if [ $rc -ne 0 ] || [ ! -s "$f.raw" ]; then
 printf '{"harness":"no response","curl_exit":%s}\n' "$rc" >"$f"
 else
 norm_json <"$f.raw" >"$f"
 fi
 SID=$(grep -i '^mcp-session-id:' "$f.hdr" 2>/dev/null | sed 's/.*: *//' | tr -d '\r\n')
 { head -1 "$f.hdr" 2>/dev/null | tr -d '\r'; grep -i '^content-type:' "$f.hdr" 2>/dev/null | tr -d '\r'; if [ -n "$SID" ]; then echo "mcp-session-id: <present>"; fi; } >"$f.meta"
 rm -f "$f.raw" "$f.hdr"
}

INIT='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"parity","version":"1"}}}'
LIST='{"jsonrpc":"2.0","id":2,"method":"tools/list"}'

record_cli() { # dir
 local d=$1
 "$BIN" -version >"$d/version.txt" 2>&1; echo "exit=$?" >>"$d/version.txt"
 "$BIN" -v >"$d/version-short.txt" 2>&1; echo "exit=$?" >>"$d/version-short.txt"
 "$BIN" -h >"$d/help.txt" 2>&1; echo "exit=$?" >>"$d/help.txt"
 "$BIN" -port abc >"$d/bad-flag.txt" 2>&1; echo "exit=$?" >>"$d/bad-flag.txt"
 "$BIN" 8080 >"$d/unexpected-args.txt" 2>&1; echo "exit=$?" >>"$d/unexpected-args.txt"
 AGENT_BROWSER_USE_PROXY=socks5://127.0.0.1:9 "$BIN" -h >"$d/help-env-proxy.txt" 2>&1; echo "exit=$?" >>"$d/help-env-proxy.txt"
}

record_server() { # dir mode(nobrowser|browser)
 local d=$1 mode=$2 port home log pid f ep
 mkdir -p "$d"
 port=$(free_port)
 home=$(mktemp -d); TMPS+=("$home")
 log="$OUT/server-$mode.log"
 local flags=(-home "$home" -port "$port" -open=false)
 if [ "$mode" = nobrowser ]; then flags+=(-no-browser); elif [ -n "${CHROME:-}" ]; then flags+=(-chrome "$CHROME"); fi
 "$BIN" "${flags[@]}" >"$log" 2>&1 &
 pid=$!; PIDS+=("$pid")
 if ! wait_http "$port" 40; then
 echo "server did not start in mode $mode (see $log)" | tee "$d/SKIPPED"
 kill "$pid" 2>/dev/null; return 1
 fi

 # static files: same bytes as app/static
 while read -r f; do
 printf '%s %s\n' "$(curl -s -m 5 "http://127.0.0.1:$port/$f" | shasum -a 256 | cut -d' ' -f1)" "$f"
 done < <(cd "$ROOT/app/static" && find . -type f | sed 's|^\./||' | sort) >"$d/static.sha256"
 printf '%s /\n' "$(curl -s -m 5 "http://127.0.0.1:$port/" | shasum -a 256 | cut -d' ' -f1)" >>"$d/static.sha256"
 curl -s -m 5 -D - -o /dev/null "http://127.0.0.1:$port/index.html" | tr -d '\r' | grep -iE '^(HTTP|content-type)' >"$d/static-index.meta"

 # MCP over HTTP (initialize first in browser mode, so tools/list is sent with the session like a real client)
 rpc "$port" "$d/mcp-http-initialize.json" "$INIT"
 if [ "$mode" = browser ]; then rpc "$port" "$d/mcp-http-tools-list.json" "$LIST" "$SID"; else rpc "$port" "$d/mcp-http-tools-list.json" "$LIST"; fi
 rpc "$port" "$d/mcp-http-ping.json" '{"jsonrpc":"2.0","id":3,"method":"ping"}'
 rpc "$port" "$d/mcp-http-resources-list.json" '{"jsonrpc":"2.0","id":4,"method":"resources/list"}'
 rpc "$port" "$d/mcp-http-unknown-method.json" '{"jsonrpc":"2.0","id":5,"method":"nope/nope"}'
 rpc "$port" "$d/mcp-http-parse-error.json" '{not json'
 rpc "$port" "$d/mcp-http-unknown-tool.json" '{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"nope","arguments":{}}}'
 rpc "$port" "$d/mcp-http-notification.json" '{"jsonrpc":"2.0","method":"notifications/initialized"}'
 curl -s -m 5 -D - -o /dev/null "http://127.0.0.1:$port/mcp" | tr -d '\r' | grep -iE '^(HTTP|allow)' >"$d/mcp-http-get.meta"
 curl -s -m 5 -D - -o /dev/null -X POST "http://127.0.0.1:$port/mcp" -H 'Origin: https://evil.example' -d "$LIST" | tr -d '\r' | head -1 >"$d/mcp-http-bad-origin.meta"

 # read-only /api/* endpoints (every GET-able HandleFunc without required parameters)
 for ep in assets devtools flows keepalive proxy states tunnel; do
 curl -s -m 15 -D "$d/api-$ep.hdr" -o "$d/api-$ep.raw" "http://127.0.0.1:$port/api/$ep"
 { head -1 "$d/api-$ep.hdr" | tr -d '\r'; grep -iE '^content-type:' "$d/api-$ep.hdr" | tr -d '\r'; } >"$d/api-$ep.meta"
 norm_json <"$d/api-$ep.raw" >"$d/api-$ep.json"
 rm -f "$d/api-$ep.hdr" "$d/api-$ep.raw"
 done

 # WebSocket hello
 if command -v node >/dev/null; then
 node "$HERE/ws-hello.mjs" "$port" 2>/dev/null | jq -S "$NORM" >"$d/ws-hello.json" 2>/dev/null || echo '{"error":"node/ws failed"}' >"$d/ws-hello.json"
 fi
 kill "$pid" 2>/dev/null
 wait "$pid" 2>/dev/null
 return 0
}

record_stdio() { # dir mode
 local d=$1 mode=$2 port home pid i
 port=$(free_port)
 home=$(mktemp -d); TMPS+=("$home")
 local flags=(mcp -home "$home" -port "$port" -open=false)
 if [ "$mode" = nobrowser ]; then flags+=(-no-browser); elif [ -n "${CHROME:-}" ]; then flags+=(-chrome "$CHROME"); fi
 local msgs
 if [ "$mode" = browser ]; then
 msgs=("$INIT" '{"jsonrpc":"2.0","method":"notifications/initialized"}' "$LIST" '{"jsonrpc":"2.0","id":3,"method":"ping"}')
 else # no initialize: it would crash a server without a browser
 msgs=("$LIST" '{"jsonrpc":"2.0","id":3,"method":"ping"}')
 fi
 local raw="$d/mcp-stdio.raw"
 ( cd "$home" && { for m in "${msgs[@]}"; do printf '%s\n' "$m"; done; sleep "${STDIO_WAIT:-6}"; } | "$BIN" "${flags[@]}" >"$raw" 2>"$OUT/stdio-$mode.log" ) &
 pid=$!
 for ((i = 0; i < 400; i++)); do
 kill -0 "$pid" 2>/dev/null || break
 sleep 0.1
 done
 kill "$pid" 2>/dev/null
 pkill -f -- "-home $home" 2>/dev/null
 # one line per response, sorted by id so concurrency does not matter
 jq -cS "$NORM" <"$raw" 2>/dev/null | jq -cs 'sort_by(.id)|.[]' >"$d/mcp-stdio.jsonl" 2>/dev/null
 [ -s "$d/mcp-stdio.jsonl" ] || echo '{"harness":"no stdout"}' >"$d/mcp-stdio.jsonl"
 rm -f "$raw"
}

record_auth() { # dir
 local d=$1 port home pid
 mkdir -p "$d"
 port=$(free_port)
 home=$(mktemp -d); TMPS+=("$home")
 "$BIN" -home "$home" -port "$port" -open=false -no-browser -pw secret >"$OUT/server-auth.log" 2>&1 &
 pid=$!; PIDS+=("$pid")
 wait_http "$port" 40 || { echo "auth server did not start" >"$d/SKIPPED"; kill "$pid" 2>/dev/null; return; }
 local u="http://127.0.0.1:$port"
 hdr() { tr -d '\r' | grep -iE '^(HTTP|www-authenticate|content-type|location|set-cookie)' | sed -E 's/(=)[0-9a-f]{32,}/\1<token>/; s/Expires=[^;]*/Expires=<date>/I; s/Max-Age=[0-9]+/Max-Age=<n>/I'; }
 curl -s -m 5 -D - -o /dev/null "$u/api/tunnel" | hdr >"$d/auth-api-no-password.txt"
 curl -s -m 5 -D - -o /dev/null -H 'Accept: text/html' "$u/" | hdr >"$d/auth-page-no-password.txt"
 curl -s -m 5 -H 'Accept: text/html' "$u/" | shasum -a 256 | cut -d' ' -f1 >"$d/auth-login-page.sha256"
 curl -s -m 5 -D - -o /dev/null -H 'Authorization: Bearer secret' "$u/api/tunnel" | hdr >"$d/auth-api-bearer.txt"
 curl -s -m 5 -D - -o /dev/null -u x:secret "$u/api/tunnel" | hdr >"$d/auth-api-basic.txt"
 curl -s -m 5 -D - -o /dev/null -H 'Authorization: Bearer wrong' "$u/api/tunnel" | hdr >"$d/auth-api-wrong.txt"
 curl -s -m 8 -D - -o /dev/null -d 'password=secret&next=/x' "$u/login" | hdr >"$d/auth-login-ok.txt"
 curl -s -m 8 -D - -o /dev/null -d 'password=nope' "$u/login" | hdr >"$d/auth-login-wrong.txt"
 kill "$pid" 2>/dev/null
}

echo "== $BIN -> $OUT"
record_cli "$OUT"
for mode in nobrowser browser; do
 d="$OUT/$mode"; mkdir -p "$d"
 if [ "$mode" = browser ] && [ -n "${PARITY_SKIP_BROWSER:-}" ]; then echo "skipped by PARITY_SKIP_BROWSER" >"$d/SKIPPED"; continue; fi
 echo "-- $mode: http"
 record_server "$d" "$mode" || continue
 echo "-- $mode: stdio"
 record_stdio "$d" "$mode"
done
echo "-- auth"
record_auth "$OUT/nobrowser"
# server logs hold ports, pids and times: keep them out of the diff
mkdir -p "$OUT/logs"
for l in server-nobrowser server-browser server-auth stdio-nobrowser stdio-browser; do
 [ -f "$OUT/$l.log" ] && mv "$OUT/$l.log" "$OUT/logs/$l.log"
done
echo "done: $OUT"
