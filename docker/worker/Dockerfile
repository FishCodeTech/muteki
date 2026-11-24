ARG KALI_IMAGE=kalilinux/kali-rolling@sha256:256893c92bbd289b07d9ef8a62e75f9c7cb3d9e570fb3d3725b2e86b9acd5728
FROM ${KALI_IMAGE} AS worker-base
ARG KALI_MIRROR=http://kali.download/kali
ENV DEBIAN_FRONTEND=noninteractive TZ=Asia/Shanghai PYTHONUNBUFFERED=1 \
    CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1
COPY curated-packages.txt /opt/muteki/curated-packages.txt
RUN --mount=type=cache,target=/var/cache/apt,sharing=locked \
    --mount=type=cache,target=/var/lib/apt/lists,sharing=locked \
    rm -f /etc/apt/apt.conf.d/docker-clean && \
    sed -i "s|http://http.kali.org/kali/|${KALI_MIRROR%/}/|g" /etc/apt/sources.list.d/kali.sources && \
    printf '%s\n' 'Acquire::Retries "2";' 'Acquire::http::Pipeline-Depth "0";' 'Acquire::http::Timeout "20";' 'Acquire::https::Timeout "20";' 'Binary::apt::APT::Keep-Downloaded-Packages "true";' 'Acquire::IndexTargets::deb::Contents-deb::DefaultEnabled "false";' 'Acquire::IndexTargets::deb::Contents-deb-legacy::DefaultEnabled "false";' > /etc/apt/apt.conf.d/99muteki && \
    echo 'wireshark-common wireshark-common/install-setuid boolean false' | debconf-set-selections && \
    packages="$(sed '/^#/d; /^$/d' /opt/muteki/curated-packages.txt | tr '\n' ' ')" && \
    attempt=1 && \
    until timeout 900 apt-get update && timeout 900 apt-get install -y --no-install-recommends --download-only $packages; do \
      if [ "$attempt" -ge 8 ]; then echo "apt download failed after $attempt attempts" >&2; exit 1; fi; \
      attempt=$((attempt + 1)); echo ">> apt download retry $attempt/8" >&2; sleep 10; \
    done && \
    apt-get install -y --no-install-recommends --no-download $packages && \
    for pkg in subfinder httpx-toolkit dnsx katana dalfox gitleaks; do \
      apt-get install -y --no-install-recommends "$pkg" || echo ">> optional recon package $pkg unavailable on mirror"; \
    done && \
    mkdir -p /opt/muteki && dpkg-query -W -f='${Package}\t${Version}\t${Architecture}\n' > /opt/muteki/packages.tsv
RUN useradd -m -s /bin/bash kali && \
    printf 'kali ALL=(ALL) NOPASSWD:ALL\n' > /etc/sudoers.d/muteki-worker && chmod 0440 /etc/sudoers.d/muteki-worker && \
    shell_path="$(command -v chromium-headless-shell)" && test -n "$shell_path" && \
    ln -s "$shell_path" /usr/bin/chromium && chromium --version && \
    mkdir -p /home/kali/workspace /run/muteki/control /run/muteki/accounts && \
    chown -R kali:kali /home/kali/workspace /run/muteki && chmod 0755 /home/kali

FROM worker-base AS node-base
ARG TARGETARCH
ARG NODE_VERSION=24.14.0
ARG NODE_MIRROR=https://nodejs.org/dist
RUN case "$TARGETARCH" in amd64) arch=x64;; arm64) arch=arm64;; *) exit 2;; esac && \
    archive="node-v${NODE_VERSION}-linux-${arch}.tar.xz" && \
    curl -fsSL --retry 3 "${NODE_MIRROR}/v${NODE_VERSION}/${archive}" -o "/tmp/${archive}" && \
    curl -fsSL --retry 3 "${NODE_MIRROR}/v${NODE_VERSION}/SHASUMS256.txt" -o /tmp/node-shasums && \
    cd /tmp && grep "  ${archive}$" node-shasums | sha256sum -c - && \
    tar -xJf "$archive" -C /usr/local --strip-components=1 && \
    rm "$archive" node-shasums && node --version

FROM worker-base AS knowledge
COPY wordlists/ctf-micro/ /usr/share/wordlists/ctf-micro/
RUN set -eu; \
    fetch() { target="$1"; repo="$2"; revision="$3"; \
      git init -q "$target"; git -C "$target" remote add origin "$repo"; \
      git -C "$target" sparse-checkout set --no-cone '/*' '!*.png' '!*.jpg' '!*.jpeg' '!*.gif' '!*.mp4' '!*.pdf' '!*.zip' '!*.gz' '!*.jar' '!*.exe' '!*.dll' '!*.so'; \
      git -C "$target" fetch --filter=blob:none --depth=1 origin "$revision"; \
      git -C "$target" checkout --detach FETCH_HEAD; rm -rf "$target/.git"; \
      printf '%s\t%s\n' "$repo" "$revision" >> /opt/muteki/knowledge-commits.tsv; }; \
    fetch /home/kali/knowledges/PayloadsAllTheThings https://github.com/swisskyrepo/PayloadsAllTheThings.git 3ac27901c711bdf3f5b65a7b1d1820a1f65bd09a; \
    fetch /home/kali/knowledges/InternalAllTheThings https://github.com/swisskyrepo/InternalAllTheThings.git 203bb0c0b290bf7c9158c32d43523b8d66f292c1; \
    fetch /home/kali/pocs/Awesome-POC https://github.com/Threekiii/Awesome-POC.git 0f9e22b660cb1ea12c1c3668f9128d93e08da412; \
    fetch /home/kali/pocs/vulhub https://github.com/vulhub/vulhub.git 8fd63916f7a8711e2e01dda0d27237e4d6175d38; \
    fetch /home/kali/.local/nuclei-templates https://github.com/projectdiscovery/nuclei-templates.git 883c2a69abe7560f469e737260322bd05888e887; \
    find /home/kali/knowledges /home/kali/pocs /home/kali/.local/nuclei-templates -type f | sort > /home/kali/KNOWLEDGE_INDEX.txt

FROM node-base AS final
ARG TARGETARCH
ARG NPM_REGISTRY=https://registry.npmjs.org
COPY engines.json minimum_versions.json capabilities.py /opt/muteki/
RUN python3 /opt/muteki/capabilities.py --check-pins
RUN --mount=type=cache,target=/root/.npm \
    npm install -g npm@11.20.0 --registry "$NPM_REGISTRY" && \
    npm install -g --registry "$NPM_REGISTRY" --foreground-scripts --loglevel=info \
      --fetch-timeout=120000 --fetch-retries=2 \
      --allow-scripts=@anthropic-ai/claude-code,@moonshot-ai/kimi-code,@opencode/cli,droid,agent-browser,@google/genai,protobufjs \
      $(python3 -c 'import json; print(" ".join(r["npm_package"]+"@"+r["version"] for r in json.load(open("/opt/muteki/engines.json")) if "npm_package" in r))') \
      agent-browser@0.41.0 playwright@1.64.1 && \
    npm list -g --depth=0 --json > /opt/muteki/npm-packages.json
USER kali
RUN CURSOR_VERSION="$(python3 -c 'import json; print(next(r["version"] for r in json.load(open("/opt/muteki/engines.json")) if r["engine"] == "cursor"))')" && \
    OMP_VERSION="$(python3 -c 'import json; print(next(r["version"] for r in json.load(open("/opt/muteki/engines.json")) if r["engine"] == "omp"))')" && \
    case "$TARGETARCH" in amd64) arch=x64;; arm64) arch=arm64;; *) exit 2;; esac && \
    mkdir -p /home/kali/.local/bin /home/kali/.local/share/cursor-agent && \
    curl -fsSL --retry 3 "https://downloads.cursor.com/lab/${CURSOR_VERSION}/linux/${arch}/agent-cli-package.tar.gz" -o /tmp/cursor.tar.gz && \
    sha256sum /tmp/cursor.tar.gz > /home/kali/.local/share/cursor-agent/archive.sha256 && \
    tar -xzf /tmp/cursor.tar.gz --strip-components=1 -C /home/kali/.local/share/cursor-agent && \
    ln -s /home/kali/.local/share/cursor-agent/cursor-agent /home/kali/.local/bin/cursor-agent && \
    rm /tmp/cursor.tar.gz && /home/kali/.local/bin/cursor-agent --version && \
    curl -fsSL --retry 3 "https://github.com/can1357/oh-my-pi/releases/download/v${OMP_VERSION}/omp-linux-${arch}" -o /home/kali/.local/bin/omp && \
    chmod 0755 /home/kali/.local/bin/omp && /home/kali/.local/bin/omp --version
RUN GROK_VERSION="$(python3 -c 'import json; print(next(r["version"] for r in json.load(open("/opt/muteki/engines.json")) if r["engine"] == "grok"))')" && \
    curl -fsSL --retry 3 https://x.ai/cli/install.sh -o /tmp/grok-install.sh && \
    bash /tmp/grok-install.sh "$GROK_VERSION" && rm /tmp/grok-install.sh && \
    /home/kali/.grok/bin/grok --version
USER root
COPY --from=knowledge /home/kali/knowledges/ /home/kali/knowledges/
COPY --from=knowledge /home/kali/pocs/ /home/kali/pocs/
COPY --from=knowledge /home/kali/.local/nuclei-templates/ /home/kali/.local/nuclei-templates/
COPY --from=knowledge /home/kali/KNOWLEDGE_INDEX.txt /home/kali/KNOWLEDGE_INDEX.txt
COPY --from=knowledge /opt/muteki/knowledge-commits.tsv /opt/muteki/knowledge-commits.tsv
COPY --from=knowledge /usr/share/wordlists/ctf-micro/ /usr/share/wordlists/ctf-micro/
ARG CHISEL_VERSION=1.12.0
RUN mkdir -p /usr/share/chisel-common-binaries && \
    curl -fsSL --connect-timeout 15 --max-time 180 --retry 3 "https://github.com/jpillora/chisel/releases/download/v${CHISEL_VERSION}/chisel_${CHISEL_VERSION}_linux_amd64.gz" -o /tmp/chisel-amd64.gz && \
    curl -fsSL --connect-timeout 15 --max-time 180 --retry 3 "https://github.com/jpillora/chisel/releases/download/v${CHISEL_VERSION}/chisel_${CHISEL_VERSION}_linux_arm64.gz" -o /tmp/chisel-arm64.gz && \
    printf '%s\n' 'f3f180f1d93aa72cce4e6386f98cc06569a0146fbd65eb4423cf83e6434bcfe6  /tmp/chisel-amd64.gz' '2ec6152cd2c74fe0146d4d79e4e7aa174521368c56e433d55e023a92ea404ec3  /tmp/chisel-arm64.gz' > /opt/muteki/chisel-assets.sha256 && \
    sha256sum -c /opt/muteki/chisel-assets.sha256 && \
    gzip -dc /tmp/chisel-amd64.gz > /usr/share/chisel-common-binaries/chisel-linux-amd64 && \
    gzip -dc /tmp/chisel-arm64.gz > /usr/share/chisel-common-binaries/chisel-linux-arm64 && \
    chmod 0755 /usr/share/chisel-common-binaries/chisel-linux-* && \
    file /usr/share/chisel-common-binaries/chisel-linux-amd64 | grep -Eq 'x86-64.*statically linked' && \
    file /usr/share/chisel-common-binaries/chisel-linux-arm64 | grep -Eq 'ARM aarch64.*statically linked' && \
    rm /tmp/chisel-amd64.gz /tmp/chisel-arm64.gz
COPY runtime_agent-${TARGETARCH} /opt/muteki/runtime_agent
COPY offline_acp_bridge.py omp_offline_config.yml kimi_offline_agent.md grok_offline_agent.md /opt/muteki/
COPY browser_coord.py /opt/muteki/browser_coord.py
COPY agent-browser.SKILL.md /opt/muteki/agent-browser/SKILL.md
COPY AGENTS.curated.md /opt/muteki/AGENTS.md
COPY AGENTS.curated.md /opt/muteki/CLAUDE.md
COPY blackboard.SKILL.md /opt/muteki/muteki-blackboard/SKILL.md
COPY blackboard.py /opt/muteki/muteki-blackboard/blackboard.py
RUN test -x /usr/lib/chromium/chromium-headless-shell && \
    ln -sfn /usr/lib/chromium/chromium-headless-shell /usr/bin/chromium && \
    python3 -m pip install --break-system-packages --no-cache-dir pycryptodome==3.23.0 && \
    TERM=xterm python3 -c "import pwn, z3, sympy, gmpy2, Crypto" && \
    chmod 0755 /opt/muteki/runtime_agent /opt/muteki/browser_coord.py /opt/muteki/muteki-blackboard/blackboard.py && \
    cp /opt/muteki/muteki-blackboard/blackboard.py /usr/local/bin/blackboard.py && \
    python3 /opt/muteki/capabilities.py
LABEL org.opencontainers.image.title="muteki-worker" \
      org.opencontainers.image.description="Curated Kali Worker with scoped Runner and pinned agent CLIs" \
      io.muteki.capabilities.schema="1" \
      io.muteki.image.flavor="curated"
USER root
WORKDIR /home/kali/workspace
ENTRYPOINT ["/opt/muteki/runtime_agent", "--sock", "/run/muteki/control/control.sock", "--workspace", "/home/kali/workspace"]
