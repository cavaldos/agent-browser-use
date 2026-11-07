#!/bin/sh
# Build the Docker image (default tag: agent-browser-use)
set -e
ROOT=$(cd "$(dirname "$0")/.." && pwd)
IMAGE=${IMAGE:-agent-browser-use}
docker build -f "$ROOT/docker/Dockerfile" -t "$IMAGE" "$ROOT"
