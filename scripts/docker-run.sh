#!/bin/sh
# Run the image. Env: IMAGE, NAME, PORT (3000), HOST (127.0.0.1). Extra args go to the server.
set -e
IMAGE=${IMAGE:-agent-browser-use}
NAME=${NAME:-agent-browser-use}
PORT=${PORT:-3000}
HOST=${HOST:-127.0.0.1}
docker rm -f "$NAME" >/dev/null 2>&1 || true
docker run -d --name "$NAME" --init --shm-size=1g \
  -p "$HOST:$PORT:3000" -v "$NAME-data:/data" "$IMAGE" "$@"
echo "Running: http://localhost:$PORT  (MCP: http://localhost:$PORT/mcp)"
