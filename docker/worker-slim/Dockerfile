# muteki worker image — SLIM build (plain Ubuntu + reverse-connector + 9 engine CLIs).
# A lightweight alternative to the full Kali image for FAST testing: it ships ONLY
#   - the in-container Runtime Control Plane supervisor (runtime_agent, PID1/ENTRYPOINT)
#   - all nine Worker CLIs
#   - the muteki-blackboard skill
# and no full Kali toolchain (no kali-linux-headless, ghidra, sagemath,
# volatility3 or knowledge base). It pulls/builds in a fraction of the
# time and is meant for wiring/integration tests where you only need a worker that
# can connect back, run an agent, and write the blackboard — not actually solve.
#
# It keeps the EXACT SAME in-container path contract as the Kali image so
# container_exec.py needs NO change — just point MUTEKI_WORKER_IMAGE at this tag:
#   - user            : kali (NOPASSWD sudo)
#   - workspace       : /home/kali/workspace   (CONTAINER_WORKSPACE)
#   - cursor-agent    : /home/kali/.local/bin/cursor-agent  (_CONTAINER_BIN["cursor"])
#   - claude/codex    : /usr/local/bin (on default PATH)
#   - control / accts : /run/muteki/control , /run/muteki/accounts
#   - ENTRYPOINT      : /opt/muteki/runtime_agent --sock … --workspace …
#
# Build (always via docker/worker-slim/build.sh, which cross-compiles runtime_agent):
#   ./docker/worker-slim/build.sh                       # → muteki-worker-slim:0.2.5 + :latest
#   ./docker/worker-slim/build.sh ghcr.io/fishcodetech/muteki-worker-slim 0.2.5
# Then run a slim swarm with:  MUTEKI_WORKER_IMAGE=muteki-worker-slim:latest ./run.sh …
#
# Platform defaults to the host architecture and can be selected by build.sh.

FROM ubuntu:24.04

# Optional build-time mirror for environments where ports.ubuntu.com is slow.
# The default stays on Ubuntu's image-provided source configuration.
ARG UBUNTU_MIRROR=""

ENV TZ=Asia/Shanghai \
    CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1 \
    DEBIAN_FRONTEND=noninteractive \
    PYTHONUNBUFFERED=1

# 1) Minimal base: the few tools the supervisor + CLIs + blackboard actually need.
#    - nodejs/npm     : claude & codex are npm globals
#    - curl/ca-certs  : cursor-agent installer + general fetch
#    - git/unzip      : agents routinely shell these
#    - python3/pip    : blackboard.py is python; agents use python3
#    - sudo           : the worker runs as kali WITH sudo (parity with Kali image §2)
#    - openssh-client : nested-SSH challenges (cheap, keep it)
#    Ubuntu's node package also supplies base integration; an exact Node 22 runtime
#    is installed below because Kimi Code requires 22.19 or newer.
RUN if [ -n "$UBUNTU_MIRROR" ]; then \
      sed -i "s|http://ports.ubuntu.com/ubuntu-ports|$UBUNTU_MIRROR|g" \
        /etc/apt/sources.list.d/ubuntu.sources; \
    fi && \
    apt-get update && \
    apt-get install -y --no-install-recommends \
      ca-certificates curl wget git unzip \
      nodejs npm \
      python3 python3-pip python3-venv \
      sudo openssh-client jq ripgrep && \
    apt-get clean && rm -rf /var/lib/apt/lists/*

# 2) User model: kali + NOPASSWD sudo — IDENTICAL to the Kali image so every baked
#    path (/home/kali/…) and the runtime uid-drop line up. The name "kali" is part of
#    the contract (container_exec pins /home/kali/...), even though the base is Ubuntu.
RUN useradd -m -s /bin/bash kali && \
    echo "kali ALL=(ALL) NOPASSWD:ALL" >> /etc/sudoers

# 2b) Agent ergonomics: workers are supposed to be able to install missing tools on
# demand. Models sometimes type `apt-get install ...` without `sudo`; because
# /usr/local/bin is before /usr/bin, these wrappers transparently retry common
# package-manager commands through NOPASSWD sudo when the caller is not root.
RUN cat > /usr/local/sbin/muteki-auto-sudo <<'EOF' && \
    chmod +x /usr/local/sbin/muteki-auto-sudo && \
    ln -sf /usr/local/sbin/muteki-auto-sudo /usr/local/bin/apt-get && \
    ln -sf /usr/local/sbin/muteki-auto-sudo /usr/local/bin/apt && \
    ln -sf /usr/local/sbin/muteki-auto-sudo /usr/local/bin/dpkg
#!/bin/sh
name="$(basename "$0")"
real="/usr/bin/$name"
if [ "$(id -u)" = "0" ]; then
  exec "$real" "$@"
fi
exec sudo -n "$real" "$@"
EOF

# 3) A tiny, useful python stack for wiring tests (NOT the full CTF stack). pwntools
#    is the one solvers reach for most; keep it so a slim worker can still do a basic
#    interaction. Everything heavy (angr/sage/z3/volatility3) is intentionally absent.
RUN pip3 install --break-system-packages --no-cache-dir \
      requests pwntools deepseek-harness-sdk==0.1.0rc6

# Ubuntu 24.04 ships Node.js 18, while the pinned Pi CLI requires Node.js
# 22.19 or newer. Install one architecture-matched runtime ahead of /usr/bin.
ARG TARGETARCH
ARG NODE_VERSION="22.19.0"
ARG NODE_MIRROR="https://nodejs.org/dist"
RUN case "$TARGETARCH" in \
      amd64) node_arch="x64" ;; \
      arm64) node_arch="arm64" ;; \
      *) echo "unsupported Node architecture: $TARGETARCH" >&2; exit 2 ;; \
    esac && \
    curl -fsSL "$NODE_MIRROR/v$NODE_VERSION/node-v$NODE_VERSION-linux-$node_arch.tar.gz" \
      -o /tmp/node.tar.gz && \
    tar -xzf /tmp/node.tar.gz -C /usr/local --strip-components=1 && \
    rm -f /tmp/node.tar.gz && \
    node --version && npm --version

# 4) Engine CLIs — same pins as the Kali image (§5) so behaviour matches.
#    Credentials are injected at runtime from Credential Accounts — NEVER baked.
ARG NPM_REGISTRY=""
RUN if [ -n "$NPM_REGISTRY" ]; then npm config set registry "$NPM_REGISTRY"; fi && \
    npm install -g \
      @openai/codex@0.146.0 \
      @anthropic-ai/claude-code@2.1.220 \
      @earendil-works/pi-coding-agent@0.84.1 \
      @moonshot-ai/kimi-code@0.36.1 \
      opencode-ai@1.18.18

# cursor-agent is NOT an npm package — installs via cursor.com/install into the kali
# user's ~/.local/bin. Run as kali so it lands in kali's HOME (container_exec pins the
# absolute path /home/kali/.local/bin/cursor-agent). Mirror the Kali image exactly.
# OMP's npm entrypoint requires Bun at runtime. Install its official prebuilt binary
# and pin the release so slim/full stay reproducible and use the same executable.
USER kali
RUN curl -fsSL https://cursor.com/install | bash
RUN curl -fsSL https://omp.sh/install | sh -s -- --binary --ref v17.3.5 && \
    /home/kali/.local/bin/omp --version
RUN curl -fsSL https://x.ai/cli/install.sh | bash -s 1.0.4 && \
    /home/kali/.grok/bin/grok --version
USER root

# 5) Runtime Agent (Go supervisor) — cross-compiled static binary, COPY'd in.
#    build.sh and release-images.yml name it by the target architecture so one
#    Dockerfile can build amd64 and arm64 without copying the wrong executable.
COPY ./runtime_agent-${TARGETARCH} /opt/muteki/runtime_agent
COPY ./deepseek_harness_worker.py /opt/muteki/deepseek_harness_worker.py
COPY ./offline_acp_bridge.py /opt/muteki/offline_acp_bridge.py
COPY ./omp_offline_config.yml /opt/muteki/omp_offline_config.yml
COPY ./kimi_offline_agent.md /opt/muteki/kimi_offline_agent.md
COPY ./grok_offline_agent.md /opt/muteki/grok_offline_agent.md
RUN chmod +x /opt/muteki/runtime_agent

# 6) Tool-awareness map + blackboard skill, baked OUTSIDE the mounted workspace. The
#    supervisor cp's AGENTS.md/CLAUDE.md into the workspace AFTER the host bind-mount so
#    the CLIs auto-read them (claude←CLAUDE.md, codex←AGENTS.md). Same content, two
#    names. The blackboard skill goes to the known CLI skill roots.
#    (Verbatim parity with Kali image §8 — same in-container layout.)
COPY ./AGENTS.md /opt/muteki/AGENTS.md
COPY ./AGENTS.md /opt/muteki/CLAUDE.md
COPY ./blackboard.SKILL.md /opt/muteki/muteki-blackboard/SKILL.md
COPY ./blackboard.py /opt/muteki/muteki-blackboard/blackboard.py
RUN chmod +x /opt/muteki/muteki-blackboard/blackboard.py && \
    cp /opt/muteki/muteki-blackboard/blackboard.py /usr/local/bin/blackboard.py && \
    chmod +x /usr/local/bin/blackboard.py

# 7) workspace + control dir; supervisor as ENTRYPOINT (PID1). Verbatim parity with
#    the Kali image §9 so the bind-mount targets and control socket path are identical.
RUN mkdir -p /home/kali/workspace && chown kali:kali /home/kali/workspace && \
    mkdir -p /run/muteki/control /run/muteki/accounts && \
    chown -R kali:kali /run/muteki

# Version label so the settings "image version check" (P2 v3) has a fact to read.
# build.sh passes --build-arg IMAGE_VERSION=<version>.
ARG IMAGE_VERSION=dev
LABEL org.opencontainers.image.title="muteki-worker-slim" \
      org.opencontainers.image.description="Slim Ubuntu muteki worker: reverse-connector + 9 agent CLIs, no full Kali toolchain" \
      org.opencontainers.image.version="${IMAGE_VERSION}"

WORKDIR /home/kali/workspace
# Supervisor runs as root (PID1) so it can drop workers to the kali uid; the worker
# itself runs as kali (with sudo). It listens on the bind-mounted control socket.
# IDENTICAL ENTRYPOINT to the Kali image — container_exec appends --connect/--run-id.
ENTRYPOINT ["/opt/muteki/runtime_agent", "--sock", "/run/muteki/control/control.sock", "--workspace", "/home/kali/workspace"]
