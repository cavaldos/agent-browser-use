#!/bin/sh
# Run the browser in Docker with its own Cloudflare WARP IP. Only the container's browser
# uses WARP; your computer's network is untouched.   Env: IMAGE, NAME, PORT, HOST.
set -e
IMAGE=${IMAGE:-agent-browser-use}
NAME=${NAME:-agent-browser-use}
PORT=${PORT:-3000}
HOST=${HOST:-127.0.0.1}
NET=$NAME-net
WARP=$NAME-warp

docker network inspect "$NET" >/dev/null 2>&1 || docker network create "$NET" >/dev/null
docker rm -f "$WARP" "$NAME" >/dev/null 2>&1 || true

# WARP client inside its own container, exposing a SOCKS5 proxy on :1080 (network-internal only).
docker run -d --name "$WARP" --network "$NET" --network-alias warp \
  --restart unless-stopped \
  --cap-add NET_ADMIN --sysctl net.ipv6.conf.all.disable_ipv6=0 \
  -e WARP_SLEEP=2 -v "$WARP-data:/var/lib/cloudflare-warp" \
  caomingjun/warp >/dev/null

echo "Waiting for WARP..."
i=0
until docker exec "$WARP" curl -fsS -x socks5h://127.0.0.1:1080 https://www.cloudflare.com/cdn-cgi/trace 2>/dev/null | grep -q 'warp=\(on\|plus\)'; do
  i=$((i+1)); [ $i -gt 40 ] && { echo "WARP did not connect: docker logs $WARP"; exit 1; }
  sleep 2
done

docker run -d --name "$NAME" --network "$NET" --init --shm-size=1g \
  -p "$HOST:$PORT:3000" -v "$NAME-data:/data" "$IMAGE" -proxy socks5://warp:1080 "$@" >/dev/null
echo "Running with WARP: http://localhost:$PORT  (MCP: http://localhost:$PORT/mcp)"
