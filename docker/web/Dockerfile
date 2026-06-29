# muteki web control plane (FastAPI/uvicorn) — P2-v3 unified containerization.
#
# This image runs the COORDINATOR (apps.web.server). It launches worker containers
# as SIBLINGS on the HOST docker daemon via the bind-mounted /var/run/docker.sock,
# so it needs the docker CLI (not a docker daemon — no dind). The Next UI is a
# SEPARATE service (docker/ui) — this image serves only /api.
#
# Build context = repo root (so the whole source tree is COPY-able):
#   docker build -f docker/web/Dockerfile -t muteki-web .
# Usually built via docker-compose.yml at the repo root.

FROM python:3.13-slim

ENV PYTHONUNBUFFERED=1 \
    PIP_NO_CACHE_DIR=1 \
    # P2-v3: this process runs inside a container → force container worker backend,
    # bind the control receiver on 0.0.0.0 (sibling workers reach it over the
    # compose network), and dial the receiver by the web service alias. compose
    # can override these, but bake sane defaults so the image is correct alone.
    MUTEKI_IN_CONTAINER=1 \
    MUTEKI_CONTROL_BIND=0.0.0.0 \
    MUTEKI_CONTROL_HOST=web-api

# docker CLI client ONLY + git/curl. We talk to the HOST daemon over the mounted
# socket; no daemon runs here. NOTE: on Debian Trixie (python:3.13-slim) the
# `docker.io` package ships only the daemon (dockerd) — the `docker` CLI client was
# split into the separate `docker-cli` package. Installing docker.io alone leaves
# `docker: not found`, which silently breaks sibling-container launch. Install
# docker-cli (client) instead; it's also smaller (no daemon/containerd/runc).
RUN apt-get update && \
    apt-get install -y --no-install-recommends \
      ca-certificates curl git docker-cli && \
    apt-get clean && rm -rf /var/lib/apt/lists/*

# uv for fast, reproducible installs (the repo uses uv / pyproject).
COPY --from=ghcr.io/astral-sh/uv:latest /uv /usr/local/bin/uv

WORKDIR /app
# Dependency layer first for cache reuse.
COPY pyproject.toml uv.lock* /app/
RUN uv sync --frozen --no-dev 2>/dev/null || uv sync --no-dev || pip install -e .

# Then the source.
COPY . /app

EXPOSE 8000
# Bind 0.0.0.0 so the ui service (and the operator) can reach /api over the
# compose network. Auth (P3) gates the surface; compose sets MUTEKI_WEB_PASSWORD.
ENV MUTEKI_WEB_BIND=0.0.0.0
CMD ["uv", "run", "uvicorn", "apps.web.server:create_app", "--factory", \
     "--host", "0.0.0.0", "--port", "8000"]
