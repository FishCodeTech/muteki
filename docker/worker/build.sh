#!/usr/bin/env bash
# Build the muteki worker image (ONE generic image — not a per-recipe tag).
# Local development helper only — formal multi-arch release goes through
# .github/workflows/release-images.yml (do not docker push GHCR from here).
# Two steps:
#   1) cross-compile the Go runtime-agent (supervisor) to
#      docker/worker/runtime_agent-amd64 (the docker build context).
#   2) docker build the amd64 image, tagging both the version and :latest.
#
# Usage: ./docker/worker/build.sh [repo] [version]
#   repo:    image repository (default: muteki-worker; e.g. ghcr.io/fishcodetech/muteki-worker)
#   version: version tag       (default: v0.4.2; GHCR release tags keep the leading v)
# Tags built: <repo>:<version> AND <repo>:latest (code defaults to :latest).
set -euo pipefail

REPO_IMAGE="${1:-muteki-worker}"
VERSION="${2:-v0.4.2}"
TAG="${REPO_IMAGE}:${VERSION}"
LATEST="${REPO_IMAGE}:latest"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd)"

echo ">> [1/3] cross-compiling runtime-agent (linux/amd64, static)..."
RUNTIME_AGENT="$HERE/runtime_agent-amd64"
if command -v go >/dev/null 2>&1; then
  CGO_ENABLED=0 GOOS=linux GOARCH=amd64 \
    go build -C "$REPO/cmd/runtime-agent" -trimpath -ldflags="-s -w" \
      -o "$RUNTIME_AGENT" .
else
  echo ">> host Go unavailable; compiling with golang:1.26-bookworm..."
  docker run --rm \
    --user "$(id -u):$(id -g)" \
    -e CGO_ENABLED=0 -e GOOS=linux -e GOARCH=amd64 \
    -e GOCACHE=/tmp/go-cache -e GOMODCACHE=/tmp/go-mod \
    -v "$REPO:/src" -w /src/cmd/runtime-agent \
    golang:1.26-bookworm \
    go build -trimpath -ldflags="-s -w" \
      -o /src/docker/worker/runtime_agent-amd64 .
fi
ls -la "$RUNTIME_AGENT"
file "$RUNTIME_AGENT" 2>/dev/null || true

echo ">> syncing muteki-blackboard skill into docker build context..."
cp "$REPO/skills/muteki-blackboard/SKILL.md" "$HERE/blackboard.SKILL.md"
cp "$REPO/skills/muteki-blackboard/blackboard.py" "$HERE/blackboard.py"
cp "$REPO/muteki/solver/offline_acp_bridge.py" "$HERE/offline_acp_bridge.py"
cp "$REPO/muteki/solver/omp_offline_config.yml" "$HERE/omp_offline_config.yml"
cp "$REPO/muteki/solver/kimi_offline_agent.md" "$HERE/kimi_offline_agent.md"
cp "$REPO/muteki/solver/grok_offline_agent.md" "$HERE/grok_offline_agent.md"
chmod +x "$HERE/blackboard.py"

# --platform linux/amd64 (full form, not the "amd64" shorthand) + --load forces the
# docker exporter into the local image store. On arm64 Docker Desktop with the
# containerd image store, the default OCI exporter has hit "operating system is not
# supported" at load time for this image even after layers export fine; --load avoids
# that path. If your build still trips it, see the docker-load fallback note below.
echo ">> [2/3] docker build --platform linux/amd64 --load -t $TAG -t $LATEST $HERE ..."
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
if [[ -n "${MUTEKI_NODE_MIRROR:-}" ]]; then
  build_args+=(--build-arg "NODE_MIRROR=${MUTEKI_NODE_MIRROR}")
fi
if [[ -n "${MUTEKI_KALI_MIRROR:-}" ]]; then
  build_args+=(--build-arg "KALI_MIRROR=${MUTEKI_KALI_MIRROR}")
fi
if [[ -n "${MUTEKI_NPM_REGISTRY:-}" ]]; then
  build_args+=(--build-arg "NPM_REGISTRY=${MUTEKI_NPM_REGISTRY}")
fi
docker build --platform linux/amd64 --load \
  "${build_args[@]}" \
  -t "$TAG" -t "$LATEST" "$HERE"

echo ">> [3/3] verifying eight container engines + core tools..."
DIGEST="$(docker image inspect --format '{{.Id}}' "$TAG")"
echo ">> image id: $DIGEST"
"$REPO/scripts/verify_worker_image.sh" --image "$TAG" --variant full --platform linux/amd64 \
  --versions-file "$HERE/.last-engine-versions.txt"
echo ">> done: $TAG (+ $LATEST); 8 engines + core tools verified (local helper only — formal release uses release-images.yml)"
