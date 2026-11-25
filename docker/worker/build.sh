#!/usr/bin/env bash
set -euo pipefail

REPO_IMAGE="${1:-muteki-worker}"
host_arch="$(uname -m)"
case "$host_arch" in
  arm64|aarch64) host_arch=arm64 ;;
  x86_64|amd64) host_arch=amd64 ;;
esac
ARCH="${2:-$host_arch}"
case "$ARCH" in
  amd64|arm64) ;;
  *) echo "unsupported architecture: $ARCH" >&2; exit 2 ;;
esac
TAG="${REPO_IMAGE}:latest"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd)"
python3 "$REPO/scripts/prepare_worker_capabilities.py"

echo ">> [1/3] cross-compiling runtime-agent (linux/$ARCH, static)..."
RUNTIME_AGENT="$HERE/runtime_agent-$ARCH"
if command -v go >/dev/null 2>&1; then
  CGO_ENABLED=0 GOOS=linux GOARCH="$ARCH" \
    go build -C "$REPO/cmd/runtime-agent" -trimpath -ldflags="-s -w" \
      -o "$RUNTIME_AGENT" .
else
  echo ">> host Go unavailable; compiling with golang:latest..."
  docker run --rm \
    --user "$(id -u):$(id -g)" \
    -e CGO_ENABLED=0 -e GOOS=linux -e GOARCH="$ARCH" \
    -e GOCACHE=/tmp/go-cache -e GOMODCACHE=/tmp/go-mod \
    -v "$REPO:/src" -w /src/cmd/runtime-agent \
    golang:latest \
    go build -trimpath -ldflags="-s -w" \
      -o "/src/docker/worker/runtime_agent-$ARCH" .
fi
ls -la "$RUNTIME_AGENT"
file "$RUNTIME_AGENT" 2>/dev/null || true

echo ">> [2/3] docker build --platform linux/$ARCH --load -t $TAG $HERE ..."
build_args=()
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
if [[ -n "${MUTEKI_NODE_MIRROR:-}" ]]; then
  build_args+=(--build-arg "NODE_MIRROR=${MUTEKI_NODE_MIRROR}")
fi
if [[ -n "${MUTEKI_KALI_MIRROR:-}" ]]; then
  build_args+=(--build-arg "KALI_MIRROR=${MUTEKI_KALI_MIRROR}")
fi
if [[ -n "${MUTEKI_NPM_REGISTRY:-}" ]]; then
  build_args+=(--build-arg "NPM_REGISTRY=${MUTEKI_NPM_REGISTRY}")
fi
if [[ -n "${MUTEKI_PIP_INDEX_URL:-}" ]]; then
  build_args+=(--build-arg "PIP_INDEX_URL=${MUTEKI_PIP_INDEX_URL}")
fi
docker build --pull --platform "linux/$ARCH" --load \
  "${build_args[@]}" \
  -t "$TAG" "$HERE"

echo ">> [3/3] verifying nine container engines + core tools..."
"$REPO/scripts/verify_worker_image.sh" --image "$TAG" --variant curated --platform "linux/$ARCH" \
  --versions-file "$HERE/.last-engine-versions.txt"
echo ">> done: $TAG; 9 engines + core tools verified (local helper only — formal release uses release-images.yml)"
