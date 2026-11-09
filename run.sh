#!/bin/sh
# Build and (re)start the production container.
#
# Usage:  GIT_HASH=$(git rev-parse --short HEAD) ./run.sh
#
# The hash is baked into `/ver` by build.rs; an empty value makes the build
# fall back to a source digest, which is still correct but less useful.
set -e
cd "$(dirname "$0")"

GIT_HASH="${GIT_HASH:-}"

echo "[run.sh] building image (GIT_HASH=${GIT_HASH:-<digest>})"
docker compose build --build-arg GIT_HASH="$GIT_HASH"

echo "[run.sh] recreating container"
docker compose up -d --force-recreate

echo "[run.sh] pruning dangling images"
docker image prune -f --filter "dangling=true" >/dev/null 2>&1 || true

docker compose ps
