#!/usr/bin/env bash
# Build the SLIM muteki worker image (plain Ubuntu + reverse-connector + 3 agent CLIs).
# A lightweight alternative to docker/worker/build.sh for FAST testing — same two steps:
#   1) cross-compile the Go runtime-agent (supervisor) to docker/worker-slim/runtime_agent
#      (the docker build context — COPY ./runtime_agent resolves relative to it).
#   2) docker build the amd64 image, tagging both the version and :latest.
#
# Usage: ./docker/worker-slim/build.sh [repo] [version] [arch]
#   repo:    image repository (default: muteki-worker-slim; e.g. ghcr.io/fishcodetech/muteki-worker-slim)
#   version: version tag       (default: 0.1.0)
#   arch:    amd64 | arm64     (default: HOST arch — arm64 on Apple Silicon)
# Tags built: <repo>:<version> AND <repo>:latest.
#
# Unlike the Kali image (pinned amd64 because ghidra/sage are amd64), the slim image
# has NO arch-locked tooling — the only baked binary we control is the Go runtime_agent
# (GOARCH), the engine CLIs are arch-agnostic JS (npm) + an arch-detecting cursor
# installer. So it builds NATIVELY for the host arch by default: on an Apple-Silicon
# mac that means arm64, which AVOIDS QEMU emulation entirely (emulated amd64 apt on
# arm64 Docker Desktop fails GPG verification — "invalid signature"), and is faster to
# build AND run locally. Pass `amd64` as the 3rd arg to force parity with the Kali
# image (e.g. to push a slim tag a remote amd64 host will pull).
#
# Run a slim swarm with it:
#   MUTEKI_WORKER_IMAGE=muteki-worker-slim:latest ./run.sh …
# Keeps the EXACT in-container path contract as the Kali image, so no code change.
set -euo pipefail

REPO_IMAGE="${1:-muteki-worker-slim}"
VERSION="${2:-0.1.0}"
# Default arch = host arch (uname -m → docker/go naming). Override with 3rd arg.
_host_arch="$(uname -m)"
case "${_host_arch}" in
  arm64|aarch64) _host_arch="arm64" ;;
  x86_64|amd64)  _host_arch="amd64" ;;
esac
ARCH="${3:-$_host_arch}"
case "${ARCH}" in
  amd64|arm64) ;;
  *) echo "!! unsupported arch '${ARCH}' (want amd64|arm64)" >&2; exit 2 ;;
esac
TAG="${REPO_IMAGE}:${VERSION}"
LATEST="${REPO_IMAGE}:latest"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd)"

echo ">> [1/2] cross-compiling runtime-agent (linux/${ARCH}, static)..."
CGO_ENABLED=0 GOOS=linux GOARCH="${ARCH}" \
  go build -C "$REPO/cmd/runtime-agent" -trimpath -ldflags="-s -w" \
    -o "$HERE/runtime_agent" .
ls -la "$HERE/runtime_agent"
file "$HERE/runtime_agent" 2>/dev/null || true

echo ">> syncing AGENTS.md + muteki-blackboard skill into docker build context..."
# AGENTS.md: reuse the trimmed copy the Kali build context already maintains (it is a
# slimmed prompt, NOT the repo-root AGENTS.md). Keep the two images in lockstep.
cp "$REPO/docker/worker/AGENTS.md" "$HERE/AGENTS.md"
cp "$REPO/skills/muteki-blackboard/SKILL.md" "$HERE/blackboard.SKILL.md"
cp "$REPO/skills/muteki-blackboard/blackboard.py" "$HERE/blackboard.py"
chmod +x "$HERE/blackboard.py"

# --platform linux/${ARCH} + --load forces the docker exporter into the local image
# store (avoids the arm64 Docker Desktop containerd-store export bug the Kali build
# documents). --build-arg IMAGE_VERSION stamps the OCI version label. Native host arch
# by default → no QEMU.
echo ">> [2/2] docker build --platform linux/${ARCH} --load -t $TAG -t $LATEST $HERE ..."
docker build --platform "linux/${ARCH}" --load \
  --build-arg "IMAGE_VERSION=${VERSION}" \
  -t "$TAG" -t "$LATEST" "$HERE"

echo ">> done: $TAG (+ $LATEST)"
echo ">> quick verify (bypass ENTRYPOINT, it's the supervisor):"
echo "   docker run --rm --entrypoint sh $TAG -c 'id; which claude codex; ls -la /home/kali/.local/bin/cursor-agent; ls /opt/muteki'"
