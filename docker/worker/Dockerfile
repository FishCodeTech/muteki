# muteki worker image — clean Kali build (only public tooling, no baked login state).
# Ships the full Kali/CTF toolchain + offline knowledge base + the three engine CLIs
# + the in-container Runtime Control Plane supervisor (runtime_agent, PID1/ENTRYPOINT).
# Credentials are injected at runtime from Credential Accounts — never baked here.
#
# Build (always via docker/worker/build.sh, which cross-compiles runtime_agent first):
#   ./docker/worker/build.sh
# Platform is amd64 (Kali main + ghidra/sage are amd64; arm64 mac builds via QEMU).
# The platform is set by the BUILD COMMAND (--platform linux/amd64), NOT a const here:
# a `FROM --platform=amd64` const trips a BuildKit export bug on arm64 Docker Desktop
# with the containerd image store ("operating system is not supported" at load time,
# after the layers already exported) — and is what the FromPlatformFlagConstDisallowed
# warning flags. Leave FROM platform-less so it follows the build's --platform.

# Pinned to a specific Kali rolling digest for reproducible builds (rolling:latest
# changes daily). Re-pin periodically: `docker pull kalilinux/kali-rolling:latest`
# then read the new digest from `docker images --digests`. Last pinned: 2026-06-17.
FROM kalilinux/kali-rolling@sha256:256893c92bbd289b07d9ef8a62e75f9c7cb3d9e570fb3d3725b2e86b9acd5728

ENV TZ=Asia/Shanghai \
    PLAYWRIGHT_MCP_BROWSER=chromium \
    CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1 \
    PYTHONUNBUFFERED=1

# 0) apt resilience for the Kali rolling mirror: retry transient fetch failures, and
#    SKIP the huge `Contents-*.gz` index (only apt-file needs it; it is NOT required
#    to install packages, and it's the file that fails a hash/size check when the
#    mirror is mid-sync — "File has unexpected size. Mirror sync in progress?" — which
#    would otherwise abort an `apt update` and the whole layer). Without it, installs
#    work fine off the Packages index.
RUN printf 'Acquire::Retries "8";\nAcquire::IndexTargets::deb::Contents-deb::DefaultEnabled "false";\nAcquire::IndexTargets::deb::Contents-deb-legacy::DefaultEnabled "false";\n' \
      > /etc/apt/apt.conf.d/99muteki-robust

# 1) Kali headless metapackage — the broad tool base every track draws on.
RUN apt update && \
    apt -y install kali-linux-headless && \
    apt clean && \
    rm -rf /var/lib/apt/lists/*

# 2) User model: kali + NOPASSWD sudo. The worker runs as kali WITH full sudo so it
#    can install packages / change the system / start services while solving. This is
#    deliberate (solving ability first); the control socket is NOT a boundary against
#    the worker (see DESIGN §8).
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

# 3) CTF supplements + RE/forensics/crypto heavyweights. Split so a single flaky
#    mirror doesn't void the whole layer. (kali-linux-headless already pulls many of
#    these; listing the ones we depend on makes the image self-documenting and pins
#    the big optional ones — ghidra/sagemath/volatility3.)
RUN apt update && \
    apt install -y --no-install-recommends \
      npm jq ncat netcat-traditional rlwrap tmux ripgrep fd-find \
      binwalk foremost exiftool sqlmap gobuster ffuf nikto gdb tshark \
      radare2 git curl wget unzip openssh-client sshpass python3-pip && \
    apt clean && rm -rf /var/lib/apt/lists/*

# ghidra in its own layer (slow, large, apt-installable). sagemath/volatility3 are
# NOT apt-installable in current Kali rolling (sagemath has no candidate at all —
# it's mid Python-transition, dropped from Debian testing too; volatility3 has no
# apt package) → sage comes from conda-forge (step 4b), volatility3 from pip (step 4).
RUN apt update && \
    apt install -y --no-install-recommends ghidra && \
    apt clean && rm -rf /var/lib/apt/lists/*

# 4) CTF python stack (system-wide, so any worker/user picks it up). volatility3 is
#    here (pip) since Kali no longer packages it — gives the `vol`/`vol3` commands.
#    --ignore-installed: angr pulls a newer mpmath, but Kali's mpmath was apt-
#    installed with no pip RECORD file, so pip refuses to uninstall it and aborts
#    ("Cannot uninstall mpmath ... no RECORD file"). --ignore-installed tells pip to
#    install its versions ALONGSIDE the apt ones instead of trying to remove them.
RUN pip3 install --break-system-packages --no-cache-dir --ignore-installed \
      pwntools requests beautifulsoup4 lxml pymongo \
      pycryptodome sympy gmpy2 z3-solver ropgadget angr volatility3

# 4b) SageMath via conda-forge (SageMath's officially-recommended install; the only
#     reliable route while the apt package is gone). Installed into /opt/conda; the
#     `sage` binary is symlinked onto PATH. This is a real, full sage. ~2-3G, slow.
#     conda pulls hundreds of MB; on a flaky link a single download can break mid-
#     stream ("Connection broken: IncompleteRead"). Bump conda's retry count + a
#     retry loop around the install so a transient drop doesn't void the whole layer.
# Miniforge pinned to a specific release + SHA256-verified before we execute it as
# root (supply-chain: never pipe an unverified `latest` installer into a root shell).
# Re-pin: bump MINIFORGE_VERSION, download the installer, `shasum -a 256` it.
ARG MINIFORGE_VERSION=26.3.2-3
ARG MINIFORGE_SHA256=848194851a98903134187fbb4ab50efe87b003e0c0f808f97644b7524a62bf2c
RUN sudo curl -fsSL --retry 8 --retry-all-errors -o /tmp/miniforge.sh \
      "https://github.com/conda-forge/miniforge/releases/download/${MINIFORGE_VERSION}/Miniforge3-${MINIFORGE_VERSION}-Linux-x86_64.sh" && \
    echo "${MINIFORGE_SHA256}  /tmp/miniforge.sh" | sha256sum -c - && \
    sudo bash /tmp/miniforge.sh -b -p /opt/conda && \
    sudo rm -f /tmp/miniforge.sh && \
    sudo /opt/conda/bin/conda config --set remote_max_retries 10 && \
    sudo /opt/conda/bin/conda config --set remote_connect_timeout_secs 30 && \
    sudo /opt/conda/bin/conda config --set remote_read_timeout_secs 120 && \
    for i in 1 2 3 4 5; do \
      sudo /opt/conda/bin/conda install -y -c conda-forge sage && break || \
      { echo "conda install attempt $i failed, retrying..."; sleep 10; }; \
    done && \
    sudo /opt/conda/bin/conda list sage | grep -qi '^sage ' && \
    sudo /opt/conda/bin/conda clean -afy && \
    sudo ln -sf /opt/conda/bin/sage /usr/local/bin/sage

# 5) Engine CLIs. claude/codex via npm (global, on PATH at /usr/local/bin).
#    Credentials are injected at runtime — login state is NEVER baked.
RUN npm install -g \
      @openai/codex@0.138.0 \
      @anthropic-ai/claude-code@2.1.98

# cursor-agent is NOT an npm package — it installs via cursor.com/install into
# ~/.local/bin. Do it as the kali user so it lands in kali's HOME (the worker runs as
# kali; container_exec pins the absolute path /home/kali/.local/bin/cursor-agent).
USER kali
RUN curl -fsSL https://cursor.com/install | bash
USER root

# 6) Offline knowledge base (public upstream repos only). Shallow
#    clones, .git stripped, owned by kali. ~2.6G. Domain-pentest-only tooling
#    (netexec/bloodyad/coercer/kerbrute/cloudfox/chisel) is intentionally NOT included.
RUN mkdir -p /home/kali/knowledges /home/kali/pocs /home/kali/.local && \
    cd /home/kali/knowledges && \
      git clone --depth=1 https://github.com/swisskyrepo/PayloadsAllTheThings.git && \
      git clone --depth=1 https://github.com/swisskyrepo/InternalAllTheThings.git && \
      git clone --depth=1 https://github.com/HackTricks-wiki/hacktricks.git && \
      git clone --depth=1 https://github.com/HackTricks-wiki/hacktricks-cloud.git && \
    cd /home/kali/pocs && \
      git clone --depth=1 https://github.com/vulhub/vulhub.git && \
      git clone --depth=1 https://github.com/Threekiii/Awesome-POC.git && \
    git clone --depth=1 https://github.com/projectdiscovery/nuclei-templates.git \
      /home/kali/.local/nuclei-templates && \
    find /home/kali/knowledges /home/kali/pocs /home/kali/.local/nuclei-templates \
      -name .git -type d -prune -exec rm -rf {} + && \
    chown -R kali:kali /home/kali/knowledges /home/kali/pocs /home/kali/.local

# 7) Runtime Agent (Go supervisor, §10) — cross-compiled static binary, COPY'd in.
#    build.sh outputs it to docker/worker/runtime_agent (this build context) first.
COPY ./runtime_agent /opt/muteki/runtime_agent
RUN chmod +x /opt/muteki/runtime_agent

# 8) Tool-awareness map, baked OUTSIDE the mounted workspace (坑 A). The supervisor
#    cp's these into /home/kali/workspace AFTER the host bind-mount, so the CLIs
#    auto-read them (claude←CLAUDE.md, codex←AGENTS.md). Same content, two names.
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

# 9) workspace + control dir; supervisor as ENTRYPOINT (PID1, replaces sleep infinity).
RUN mkdir -p /home/kali/workspace && chown kali:kali /home/kali/workspace && \
    mkdir -p /run/muteki/control /run/muteki/accounts && \
    chown -R kali:kali /run/muteki

# OCI version label so the settings "worker image check" (P2-v3) can compare the
# pulled image's version against what the app expects. build.sh passes
# --build-arg IMAGE_VERSION=<version>.
ARG IMAGE_VERSION=dev
LABEL org.opencontainers.image.title="muteki-worker" \
      org.opencontainers.image.description="Kali muteki worker: full CTF toolchain + reverse-connector + 3 agent CLIs" \
      org.opencontainers.image.version="${IMAGE_VERSION}"

WORKDIR /home/kali/workspace
# Supervisor runs as root (PID1) so it can drop workers to the kali uid; the worker
# itself runs as kali (with sudo). It listens on the bind-mounted control socket.
ENTRYPOINT ["/opt/muteki/runtime_agent", "--sock", "/run/muteki/control/control.sock", "--workspace", "/home/kali/workspace"]
