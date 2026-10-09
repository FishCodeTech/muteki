#!/usr/bin/env bash
# Build the SLIM muteki worker image (plain Ubuntu + reverse-connector + 8 agent CLIs).
# Local development helper only — formal multi-arch release goes through
# .github/workflows/release-images.yml (do not docker push GHCR from here).
# A lightweight alternative to docker/worker/build.sh for FAST testing — same two steps:
#   1) cross-compile the Go runtime-agent (supervisor) to an architecture-named
#      file in docker/worker-slim (the docker build context).
#   2) docker build the amd64 image, tagging both the version and :latest.
#
# Usage: ./docker/worker-slim/build.sh [repo] [version] [arch]
#   repo:    image repository (default: muteki-worker-slim; e.g. ghcr.io/fishcodetech/muteki-worker-slim)
#   version: version tag       (default: v0.4.1; GHCR release tags keep the leading v)
#   arch:    amd64 | arm64     (default: HOST arch — arm64 on Apple Silicon)
# Tags built: <repo>:<version> AND <repo>:latest.
#
# Unlike the Kali image whose local build helper targets amd64, the slim image has
# NO arch-locked tooling — the only baked binary we control is the Go runtime_agent
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
VERSION="${2:-v0.4.1}"
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

echo ">> [1/3] cross-compiling runtime-agent (linux/${ARCH}, static)..."
RUNTIME_AGENT="$HERE/runtime_agent-${ARCH}"
if command -v go >/dev/null 2>&1; then
  CGO_ENABLED=0 GOOS=linux GOARCH="${ARCH}" \
    go build -C "$REPO/cmd/runtime-agent" -trimpath -ldflags="-s -w" \
      -o "$RUNTIME_AGENT" .
else
  echo ">> host Go unavailable; compiling with golang:1.26-bookworm..."
  docker run --rm \
    --user "$(id -u):$(id -g)" \
    -e CGO_ENABLED=0 -e GOOS=linux -e GOARCH="${ARCH}" \
    -e GOCACHE=/tmp/go-cache -e GOMODCACHE=/tmp/go-mod \
    -v "$REPO:/src" -w /src/cmd/runtime-agent \
    golang:1.26-bookworm \
    go build -trimpath -ldflags="-s -w" \
      -o "/src/docker/worker-slim/runtime_agent-${ARCH}" .
fi
ls -la "$RUNTIME_AGENT"
file "$RUNTIME_AGENT" 2>/dev/null || true

echo ">> syncing slim AGENTS.md + muteki-blackboard skill into docker build context..."
# AGENTS.md: slim keeps its own minimal capability guide (private cwd + shared/
# handoff). Do not overwrite with the Kali full copy — that would reintroduce
# full-only tool claims and dangling toolbox expectations.
if [ ! -f "$HERE/AGENTS.md" ]; then
  echo "!! missing docker/worker-slim/AGENTS.md" >&2
  exit 2
fi
cp "$REPO/skills/muteki-blackboard/SKILL.md" "$HERE/blackboard.SKILL.md"
cp "$REPO/skills/muteki-blackboard/blackboard.py" "$HERE/blackboard.py"
cp "$REPO/muteki/solver/offline_acp_bridge.py" "$HERE/offline_acp_bridge.py"
cp "$REPO/muteki/solver/omp_offline_config.yml" "$HERE/omp_offline_config.yml"
cp "$REPO/muteki/solver/kimi_offline_agent.md" "$HERE/kimi_offline_agent.md"
cp "$REPO/muteki/solver/grok_offline_agent.md" "$HERE/grok_offline_agent.md"
chmod +x "$HERE/blackboard.py"

# --platform linux/${ARCH} + --load forces the docker exporter into the local image
# store (avoids the arm64 Docker Desktop containerd-store export bug the Kali build
# documents). --build-arg IMAGE_VERSION stamps the OCI version label. Native host arch
# by default → no QEMU.
echo ">> [2/3] docker build --platform linux/${ARCH} --load -t $TAG -t $LATEST $HERE ..."
build_args=(--build-arg "IMAGE_VERSION=${VERSION}")
# Docker build stages cannot reach a proxy bound to the host loopback address.
# Forward the developer shell's proxy through Docker Desktop's stable host name.
# apt reads the lowercase variables while several installers read the uppercase
# variants, so always populate both spellings from whichever one the shell has.
http_proxy_value="${http_proxy:-${HTTP_PROXY:-}}"
https_proxy_value="${https_proxy:-${HTTPS_PROXY:-}}"
all_proxy_value="${all_proxy:-${ALL_PROXY:-}}"
for proxy_name in HTTP_PROXY http_proxy; do
  if [[ -n "$http_proxy_value" ]]; then
    proxy_value="${http_proxy_value//127.0.0.1/host.docker.internal}"
    proxy_value="${proxy_value//localhost/host.docker.internal}"
    build_args+=(--build-arg "${proxy_name}=${proxy_value}")
  fi
done
for proxy_name in HTTPS_PROXY https_proxy; do
  if [[ -n "$https_proxy_value" ]]; then
    proxy_value="${https_proxy_value//127.0.0.1/host.docker.internal}"
    proxy_value="${proxy_value//localhost/host.docker.internal}"
    build_args+=(--build-arg "${proxy_name}=${proxy_value}")
  fi
done
for proxy_name in ALL_PROXY all_proxy; do
  if [[ -n "$all_proxy_value" ]]; then
    proxy_value="${all_proxy_value//127.0.0.1/host.docker.internal}"
    proxy_value="${proxy_value//localhost/host.docker.internal}"
    build_args+=(--build-arg "${proxy_name}=${proxy_value}")
  fi
done
no_proxy_value="${no_proxy:-${NO_PROXY:-}}"
for no_proxy_name in NO_PROXY no_proxy; do
  if [[ -n "$no_proxy_value" ]]; then
    build_args+=(--build-arg "${no_proxy_name}=${no_proxy_value}")
  fi
done
if [[ -n "${MUTEKI_UBUNTU_MIRROR:-}" ]]; then
  build_args+=(--build-arg "UBUNTU_MIRROR=${MUTEKI_UBUNTU_MIRROR}")
fi
if [[ -n "${MUTEKI_NODE_MIRROR:-}" ]]; then
  build_args+=(--build-arg "NODE_MIRROR=${MUTEKI_NODE_MIRROR}")
fi
if [[ -n "${MUTEKI_NPM_REGISTRY:-}" ]]; then
  build_args+=(--build-arg "NPM_REGISTRY=${MUTEKI_NPM_REGISTRY}")
fi
docker build --platform "linux/${ARCH}" --load \
  "${build_args[@]}" \
  -t "$TAG" -t "$LATEST" "$HERE"

echo ">> [3/3] verifying eight container engines + core paths..."
DIGEST="$(docker image inspect --format '{{.Id}}' "$TAG")"
echo ">> image id: $DIGEST"
"$REPO/scripts/verify_worker_image.sh" --image "$TAG" --variant slim --platform "linux/${ARCH}" \
  --versions-file "$HERE/.last-engine-versions.txt"
echo ">> done: $TAG (+ $LATEST); 8 engines verified (local helper only — formal release uses release-images.yml)"
