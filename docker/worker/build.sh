#!/usr/bin/env bash
# Build the muteki worker image (ONE generic image — not a per-recipe tag). Two steps:
#   1) cross-compile the Go runtime-agent (supervisor) to docker/worker/runtime_agent
#      (the docker build context — COPY ./runtime_agent resolves relative to it).
#   2) docker build the amd64 image, tagging both the version and :latest.
#
# Usage: ./docker/worker/build.sh [repo] [version]
#   repo:    image repository (default: muteki-worker; e.g. ghcr.io/fishcodetech/muteki-worker)
#   version: version tag       (default: 0.2.3)
# Tags built: <repo>:<version> AND <repo>:latest (code defaults to :latest).
set -euo pipefail

REPO_IMAGE="${1:-muteki-worker}"
VERSION="${2:-0.2.3}"
TAG="${REPO_IMAGE}:${VERSION}"
LATEST="${REPO_IMAGE}:latest"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd)"

echo ">> [1/2] cross-compiling runtime-agent (linux/amd64, static)..."
CGO_ENABLED=0 GOOS=linux GOARCH=amd64 \
  go build -C "$REPO/cmd/runtime-agent" -trimpath -ldflags="-s -w" \
    -o "$HERE/runtime_agent" .
ls -la "$HERE/runtime_agent"
file "$HERE/runtime_agent" 2>/dev/null || true

echo ">> syncing muteki-blackboard skill into docker build context..."
cp "$REPO/skills/muteki-blackboard/SKILL.md" "$HERE/blackboard.SKILL.md"
cp "$REPO/skills/muteki-blackboard/blackboard.py" "$HERE/blackboard.py"
chmod +x "$HERE/blackboard.py"

# --platform linux/amd64 (full form, not the "amd64" shorthand) + --load forces the
# docker exporter into the local image store. On arm64 Docker Desktop with the
# containerd image store, the default OCI exporter has hit "operating system is not
# supported" at load time for this image even after layers export fine; --load avoids
# that path. If your build still trips it, see the docker-load fallback note below.
echo ">> [2/2] docker build --platform linux/amd64 --load -t $TAG -t $LATEST $HERE ..."
docker build --platform linux/amd64 --load \
  --build-arg "IMAGE_VERSION=${VERSION}" \
  -t "$TAG" -t "$LATEST" "$HERE"

echo ">> done: $TAG (+ $LATEST)"
echo ">> quick verify (bypass ENTRYPOINT, it's the supervisor):"
echo "   docker run --rm --entrypoint sh $TAG -c 'which claude codex cursor-agent ghidra sage vol radare2; ls /opt/muteki'"
