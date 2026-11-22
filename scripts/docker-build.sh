#!/bin/sh
# Build the Docker image (default tag: agent-browser-use). VERSION=... stamps the version (default: git describe).
set -e
ROOT=$(cd "$(dirname "$0")/.." && pwd)
IMAGE=${IMAGE:-agent-browser-use}
VERSION=${VERSION:-$(git -C "$ROOT" describe --tags --always --dirty 2>/dev/null || echo dev)}
docker build -f "$ROOT/docker/Dockerfile" --build-arg "AGENT_BROWSER_USE_VERSION=$VERSION" -t "$IMAGE" "$ROOT"
