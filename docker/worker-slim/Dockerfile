# muteki worker image — SLIM build (plain Ubuntu + reverse-connector + 3 engine CLIs).
# A lightweight alternative to the full Kali image for FAST testing: it ships ONLY
#   - the in-container Runtime Control Plane supervisor (runtime_agent, PID1/ENTRYPOINT)
#   - the three engine CLIs (claude / codex / cursor-agent)
#   - the muteki-blackboard skill
# and NOTHING from the CTF toolchain (no kali-linux-headless, ghidra, sagemath,
# volatility3, pwntools, knowledge base, …). It pulls/builds in a fraction of the
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
#   ./docker/worker-slim/build.sh                       # → muteki-worker-slim:0.1.0 + :latest
#   ./docker/worker-slim/build.sh snowywar/muteki-worker-slim 0.1.0
# Then run a slim swarm with:  MUTEKI_WORKER_IMAGE=muteki-worker-slim:latest ./run.sh …
#
# Platform: amd64 (match the Kali image + the cross-compiled runtime_agent). Set by the
# build command (--platform linux/amd64), NOT a FROM const (same BuildKit arm64 export
# bug the Kali Dockerfile documents). Leave FROM platform-less.

FROM ubuntu:24.04

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
#    Ubuntu 24.04's nodejs is recent enough for the CLIs; no NodeSource needed.
RUN apt-get update && \
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
      requests pwntools

# 4) Engine CLIs — same pins as the Kali image (§5) so behaviour matches.
#    Credentials are injected at runtime from Credential Accounts — NEVER baked.
RUN npm install -g \
      @openai/codex@0.138.0 \
      @anthropic-ai/claude-code@2.1.98

# cursor-agent is NOT an npm package — installs via cursor.com/install into the kali
# user's ~/.local/bin. Run as kali so it lands in kali's HOME (container_exec pins the
# absolute path /home/kali/.local/bin/cursor-agent). Mirror the Kali image exactly.
USER kali
RUN curl -fsSL https://cursor.com/install | bash
USER root

# 5) Runtime Agent (Go supervisor) — cross-compiled static binary, COPY'd in.
#    build.sh outputs it to docker/worker-slim/runtime_agent (this build context).
COPY ./runtime_agent /opt/muteki/runtime_agent
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
    chmod +x /usr/local/bin/blackboard.py && \
    install -d -o kali -g kali \
      /home/kali/.claude/skills/muteki-blackboard \
      /home/kali/.agents/skills/muteki-blackboard \
      /home/kali/.codex/skills/muteki-blackboard \
      /home/kali/.cursor/skills-cursor/muteki-blackboard \
      /home/kali/.cursor/skills/muteki-blackboard && \
    cp /opt/muteki/muteki-blackboard/SKILL.md \
      /home/kali/.claude/skills/muteki-blackboard/SKILL.md && \
    cp /opt/muteki/muteki-blackboard/blackboard.py \
      /home/kali/.claude/skills/muteki-blackboard/blackboard.py && \
    cp /opt/muteki/muteki-blackboard/SKILL.md \
      /home/kali/.agents/skills/muteki-blackboard/SKILL.md && \
    cp /opt/muteki/muteki-blackboard/blackboard.py \
      /home/kali/.agents/skills/muteki-blackboard/blackboard.py && \
    cp /opt/muteki/muteki-blackboard/SKILL.md \
      /home/kali/.codex/skills/muteki-blackboard/SKILL.md && \
    cp /opt/muteki/muteki-blackboard/blackboard.py \
      /home/kali/.codex/skills/muteki-blackboard/blackboard.py && \
    cp /opt/muteki/muteki-blackboard/SKILL.md \
      /home/kali/.cursor/skills-cursor/muteki-blackboard/SKILL.md && \
    cp /opt/muteki/muteki-blackboard/blackboard.py \
      /home/kali/.cursor/skills-cursor/muteki-blackboard/blackboard.py && \
    cp /opt/muteki/muteki-blackboard/SKILL.md \
      /home/kali/.cursor/skills/muteki-blackboard/SKILL.md && \
    cp /opt/muteki/muteki-blackboard/blackboard.py \
      /home/kali/.cursor/skills/muteki-blackboard/blackboard.py && \
    chown -R kali:kali /home/kali/.claude /home/kali/.agents /home/kali/.codex /home/kali/.cursor && \
    chmod +x \
      /home/kali/.claude/skills/muteki-blackboard/blackboard.py \
      /home/kali/.agents/skills/muteki-blackboard/blackboard.py \
      /home/kali/.codex/skills/muteki-blackboard/blackboard.py \
      /home/kali/.cursor/skills-cursor/muteki-blackboard/blackboard.py \
      /home/kali/.cursor/skills/muteki-blackboard/blackboard.py

# 7) workspace + control dir; supervisor as ENTRYPOINT (PID1). Verbatim parity with
#    the Kali image §9 so the bind-mount targets and control socket path are identical.
RUN mkdir -p /home/kali/workspace && chown kali:kali /home/kali/workspace && \
    mkdir -p /run/muteki/control /run/muteki/accounts && \
    chown -R kali:kali /run/muteki

# Version label so the settings "image version check" (P2 v3) has a fact to read.
# build.sh passes --build-arg IMAGE_VERSION=<version>.
ARG IMAGE_VERSION=dev
LABEL org.opencontainers.image.title="muteki-worker-slim" \
      org.opencontainers.image.description="Slim Ubuntu muteki worker: reverse-connector + 3 agent CLIs, no CTF toolchain" \
      org.opencontainers.image.version="${IMAGE_VERSION}"

WORKDIR /home/kali/workspace
# Supervisor runs as root (PID1) so it can drop workers to the kali uid; the worker
# itself runs as kali (with sudo). It listens on the bind-mounted control socket.
# IDENTICAL ENTRYPOINT to the Kali image — container_exec appends --connect/--run-id.
ENTRYPOINT ["/opt/muteki/runtime_agent", "--sock", "/run/muteki/control/control.sock", "--workspace", "/home/kali/workspace"]
